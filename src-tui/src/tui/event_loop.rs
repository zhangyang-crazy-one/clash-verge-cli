use std::time::Duration;

use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind};
use serde_yaml_ng::Value;
use tokio::sync::mpsc;
use tokio::time;
use tokio_stream::StreamExt as _;

use crate::app::{
    Action, App, CoreState, EditorTarget, Focus, InputMode, Overlay, PendingSudoAction, ProxyDisplayRow, TrustPending,
    TunSetupReason, View, first_selectable_proxy_group, proxy_display_rows,
};
use crate::i18n::Language;
use crate::mihomo_api::types::{LogEntry, TrafficData};
use crate::runtime_config::{
    RUNTIME_CONFIG_IO, commit_runtime_config, reload_config_file, reload_remote_profile, write_runtime_config,
    write_runtime_config_unlocked,
};
use crate::tui::{TerminalGuard, input};

fn key_context(app: &App) -> input::KeyContext<'_> {
    input::KeyContext {
        view: app.view,
        focus: app.focus,
        overlay: app.overlay,
        pending_connection_close: app.pending_connection_close.as_deref(),
    }
}

/// Key-path actions that must be forwarded to the action channel instead of
/// being handled inline. Every overlay-confirm/cancel action produced by
/// `input::map_key` has its handler in the channel match; missing an entry
/// here silently drops the key (the PR #13 service-uninstall dialog could
/// never be dismissed because Confirm/CancelServiceUninstall fell through to
/// the key path's discard arm). `DismissOverlay` is deliberately NOT listed:
/// it has an inline handler in the key-path match, and the channel match has
/// no arm for it.
fn forward_to_channel(action: &Action) -> bool {
    matches!(
        action,
        Action::PasswordChar(_)
            | Action::PasswordBackspace
            | Action::PasswordSubmit
            | Action::PasswordCancel
            | Action::ConfirmTrustImport
            | Action::CancelTrustImport
            | Action::ConfirmTunSetup
            | Action::SkipTunSetupStart
            | Action::ConfirmServiceUninstall
            | Action::CancelServiceUninstall
    )
}

fn dismiss_overlay(app: &mut App) {
    app.overlay = None;
    app.filter = None;
    app.pending_connection_close = None;
    app.pending_trust = None;
    app.focus = Focus::Menu;
}

/// Transition into `view`, firing the per-view refresh probes. Every entry
/// path (number keys via `SwitchView`, menu j/k via `MoveNext`/`MovePrevious`)
/// routes through here so cached state — notably the Settings
/// service/autostart probes — is refreshed on every transition INTO a view,
/// never only on the explicit switch keys.
fn transition_to_view(app: &mut App, action_tx: &mpsc::UnboundedSender<Action>, view: View) {
    app.view = view;
    match view {
        View::Proxies if app.proxy_groups.is_empty() => {
            let _ = action_tx.send(Action::ProxiesRefresh);
        }
        View::Connections => {
            let _ = action_tx.send(Action::ConnectionsRefresh);
        }
        View::Logs => {
            let _ = action_tx.send(Action::LogsRefresh);
        }
        View::Rules => {
            if app.rules.is_empty() {
                let _ = action_tx.send(Action::RulesRefresh);
            }
            if app.rule_providers.is_empty() {
                let _ = action_tx.send(Action::RuleProvidersRefresh);
            }
        }
        View::Settings => {
            // Refresh the cached read-only service/autostart probes on every
            // Settings entry (SwitchView AND menu navigation) so the two
            // lifecycle rows stay fresh — no stale install/uninstall offers.
            let _ = action_tx.send(Action::ServiceStatusRefresh);
        }
        _ => {}
    }
}

/// Menu j/k navigation: wrap the view cursor and route through
/// [`transition_to_view`] so entering Settings by menu navigation runs the
/// same lifecycle probe refresh as the explicit switch keys.
fn menu_move(app: &mut App, action_tx: &mpsc::UnboundedSender<Action>, forward: bool) {
    let index = View::ALL.iter().position(|view| *view == app.view).unwrap_or_default();
    let next = if forward {
        (index + 1) % View::ALL.len()
    } else {
        (index + View::ALL.len() - 1) % View::ALL.len()
    };
    transition_to_view(app, action_tx, View::ALL[next]);
}

fn begin_connection_close(app: &mut App) {
    if let Some(id) = app.selected_connection_id.clone() {
        app.pending_connection_close = Some(id.clone());
        app.overlay = Some(Overlay::CloseConfirmation);
        app.status_msg = Some(format!("Close connection {id}? Press Enter to confirm"));
    } else {
        app.status_msg = Some("Select a connection before closing it".into());
    }
}

fn close_confirmation_is_current(app: &App, id: &str) -> bool {
    app.pending_connection_close.as_deref() == Some(id)
        && app.selected_connection_id.as_deref() == Some(id)
        && app.overlay == Some(Overlay::CloseConfirmation)
}

fn connection_matches_filter(connection: &crate::mihomo_api::types::ConnectionInfo, query: &str) -> bool {
    let query = query.to_ascii_lowercase();
    let host = connection
        .metadata
        .as_ref()
        .and_then(|metadata| metadata.host.as_deref())
        .unwrap_or_default();
    let rule = connection.rule.as_deref().unwrap_or_default();
    connection.id.to_ascii_lowercase().contains(&query)
        || host.to_ascii_lowercase().contains(&query)
        || rule.to_ascii_lowercase().contains(&query)
}

fn visible_connection_ids(app: &App) -> Vec<String> {
    match app.connection_filter.as_deref() {
        Some(query) if !query.is_empty() => app
            .connections
            .iter()
            .filter(|connection| connection_matches_filter(connection, query))
            .map(|connection| connection.id.clone())
            .collect(),
        _ => app.connections.iter().map(|connection| connection.id.clone()).collect(),
    }
}

fn move_connection_selection(app: &mut App, forward: bool) {
    let ids = visible_connection_ids(app);
    if ids.is_empty() {
        app.selected_connection_id = None;
        app.connection_selected_index = 0;
        return;
    }
    let current = app
        .selected_connection_id
        .as_deref()
        .and_then(|id| ids.iter().position(|candidate| candidate == id))
        .unwrap_or(0);
    let next = if forward {
        (current + 1) % ids.len()
    } else {
        (current + ids.len() - 1) % ids.len()
    };
    app.connection_selected_index = next;
    app.selected_connection_id = Some(ids[next].clone());
}

fn visible_log_count(app: &App) -> usize {
    match app.log_filter.as_deref() {
        Some(query) if !query.is_empty() => {
            let query = query.to_ascii_lowercase();
            app.logs
                .iter()
                .filter(|entry| {
                    entry.level.to_ascii_lowercase().contains(&query)
                        || entry.payload.to_ascii_lowercase().contains(&query)
                })
                .count()
        }
        _ => app.logs.len(),
    }
}

fn move_log_selection(app: &mut App, forward: bool) {
    let count = visible_log_count(app);
    if count == 0 {
        app.log_selected_index = 0;
    } else if forward {
        app.log_selected_index = (app.log_selected_index + 1) % count;
    } else {
        app.log_selected_index = (app.log_selected_index + count - 1) % count;
    }
}

/// Mihomo sends real-time APIs as newline-delimited JSON. Keep incomplete
/// packets buffered so records split across socket reads remain valid JSON.
fn drain_ndjson<T: serde::de::DeserializeOwned>(buffer: &mut Vec<u8>, chunk: &[u8]) -> Result<Vec<T>, String> {
    const MAX_PENDING_BYTES: usize = 1024 * 1024;

    buffer.extend_from_slice(chunk);
    if buffer.len() > MAX_PENDING_BYTES && !buffer.contains(&b'\n') {
        return Err("Mihomo stream sent more than 1 MiB without a newline".into());
    }

    let mut entries = Vec::new();
    while let Some(end) = buffer.iter().position(|byte| *byte == b'\n') {
        let line: Vec<u8> = buffer.drain(..=end).collect();
        let line = line.trim_ascii();
        if line.is_empty() {
            continue;
        }
        entries.push(serde_json::from_slice(line).map_err(|error| format!("invalid Mihomo stream entry: {error}"))?);
    }
    Ok(entries)
}

async fn receive_traffic_stream(
    api: crate::mihomo_api::client::MihomoApi,
    tx: mpsc::UnboundedSender<Action>,
) -> Result<(), String> {
    let response = api.stream_traffic().await.map_err(|error| error.to_string())?;
    let mut stream = response.bytes_stream();
    let mut buffer = Vec::new();

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| error.to_string())?;
        for traffic in drain_ndjson::<TrafficData>(&mut buffer, &chunk)? {
            if tx.send(Action::TrafficFetched(traffic)).is_err() {
                return Ok(());
            }
        }
    }

    Err("Mihomo traffic stream closed".into())
}

async fn receive_log_stream(
    api: crate::mihomo_api::client::MihomoApi,
    tx: mpsc::UnboundedSender<Action>,
) -> Result<(), String> {
    let response = api.stream_logs("info").await.map_err(|error| error.to_string())?;
    let mut stream = response.bytes_stream();
    let mut buffer = Vec::new();

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| error.to_string())?;
        for log in drain_ndjson::<LogEntry>(&mut buffer, &chunk)? {
            if tx.send(Action::LogReceived(log)).is_err() {
                return Ok(());
            }
        }
    }

    Err("Mihomo log stream closed".into())
}

/// Write TUN-enabled runtime config and apply it under one IO lock.
///
/// Re-reads `clash.yaml` inside the lock so a concurrent profile/mode commit is not
/// overwritten by a stale pre-spawn snapshot. Owned cores restart (stop-by-pid works
/// when the Child sits in the watcher); attached cores API-reload the written file.
async fn apply_tun_runtime(
    manager: &crate::mihomo_manager::manager::MihomoManager,
    _guard: std::sync::Arc<tokio::sync::Mutex<crate::tui::TerminalGuard>>,
    owns_core: bool,
    enable_tun: bool,
) -> Result<bool, String> {
    // TUN capability is ensured by the read-only preflight before this runs
    // (Settings toggle checks the binary before persisting; the manager
    // repeats the check before every TUN-enabled spawn). No askpass here:
    // the password popup is only reachable from Settings → TUN setup.
    let _guard = RUNTIME_CONFIG_IO.lock().await;
    let config = clash_verge_core::config::IClashTemp::new().await.0;
    let path = write_runtime_config_unlocked(config, enable_tun).await?;
    if owns_core {
        manager.restart().await.map(|_| ()).map_err(|error| error.to_string())?;
    } else {
        reload_config_file(&manager.api(), &path).await?;
    }
    Ok(false)
}

/// Write the runtime config (with TUN flag) and start the core. Shared by
/// the StartCore key path and the resolve-then-start path.
async fn start_core_with_tun(
    manager: &crate::mihomo_manager::manager::MihomoManager,
    enable_tun: bool,
) -> Result<(), String> {
    let config = clash_verge_core::config::IClashTemp::new().await.0;
    write_runtime_config(config, enable_tun).await?;
    manager.start().await.map(|_| ()).map_err(|error| error.to_string())
}

/// Persist TUN flag into a freshly loaded runtime config (core stopped path).
async fn write_tun_runtime(enable_tun: bool) -> Result<std::path::PathBuf, String> {
    let _guard = RUNTIME_CONFIG_IO.lock().await;
    let config = clash_verge_core::config::IClashTemp::new().await.0;
    write_runtime_config_unlocked(config, enable_tun).await
}

/// Detect an SSRF-blocked subscription host without parsing error strings.
///
/// Returns the bare host only when all three hold: the URL parses, the
/// default (empty) allowlist check returns a genuine `CheckError::Blocked`
/// result (the host actually resolved to a private/loopback/link-local/ULA
/// address), AND adding that host to the allowlist would let it through.
/// Matching the typed `Blocked` variant — rather than treating any failure as
/// a block — excludes DNS-resolution and no-address failures, so the trust
/// prompt is never offered for a host that trusting would not actually
/// unblock. The allowlist lookup in `ssrf::check_url_host` precedes DNS
/// resolution, so the trusted re-check is exact and needs no network.
fn ssrf_blocked_host(url: &str) -> Option<String> {
    let cleaned = crate::subscribe::from_url::fix_dirty_url(url).ok()?;
    let host = cleaned.host_str().map(str::to_string)?;
    // Ordinary imports keep default SSRF protection: empty allowlist. Only a
    // genuine blocked-address result may offer trust; DNS failures surface as
    // plain import errors instead of a "trust this host" prompt.
    let blocked = matches!(
        crate::subscribe::ssrf::check_url_host(cleaned.as_str(), &[]),
        Err(crate::subscribe::ssrf::CheckError::Blocked { .. })
    );
    if !blocked {
        return None;
    }
    crate::subscribe::ssrf::check_url_host(cleaned.as_str(), std::slice::from_ref(&host))
        .is_ok()
        .then_some(host)
}

/// Import a subscription URL in the background, preserving default SSRF
/// protection (`option` is `None` for ordinary imports). Results arrive back
/// as `ProfileImported` / `ProfileImportFailed`.
fn spawn_import(
    action_tx: &mpsc::UnboundedSender<Action>,
    url: String,
    option: Option<clash_verge_core::config::PrfOption>,
) {
    let tx = action_tx.clone();
    tokio::spawn(async move {
        match crate::profile_store::store::ProfileStore::import_url_locked(&url, None, option.as_ref()).await {
            Ok(_) => {
                let _ = tx.send(Action::ProfileImported);
            }
            Err(error) => {
                let _ = tx.send(Action::ProfileImportFailed(error.to_string()));
            }
        }
    });
}

/// Resolve a confirmed SSRF trust prompt.
///
/// Import (`pending.uid` is `None`): retry only that import with the host in
/// `trusted_hosts`; the option is persisted into the imported profile by the
/// existing `from_url` path, so later manual/automatic updates reuse it.
///
/// Manual refresh (`pending.uid` is `Some`): persist the normalized host into
/// that profile's stored `option.trusted_hosts` (merge + save `profiles.yaml`),
/// then retry the update; the re-read allowlist unblocks the fetch. The uid is
/// required so trust lands on the existing profile, never on a new one.
fn handle_confirm_trust(app: &mut App, action_tx: &mpsc::UnboundedSender<Action>) {
    let Some(pending) = app.pending_trust.take() else {
        // Stale duplicate `y` after the prompt already closed: ignore instead
        // of retrying an import the user may have cancelled.
        return;
    };
    app.overlay = None;
    if let Some(uid) = pending.uid {
        let host = pending.host;
        app.status_msg = Some(format!("Updating profile {uid} (trusted host)..."));
        let tx = action_tx.clone();
        tokio::spawn(async move {
            if let Err(error) = crate::profile_store::store::ProfileStore::add_trusted_host_locked(&uid, &host).await {
                let _ = tx.send(Action::ProfileUpdateFailed(format!("trust persist: {error}")));
                return;
            }
            // update_remote_locked re-reads profiles.yaml, so the persisted
            // (merged) allowlist is what unblocks this retry.
            match crate::profile_store::store::ProfileStore::update_remote_locked(&uid, None).await {
                Ok(is_current) => {
                    let _ = tx.send(Action::ProfileUpdated { uid, is_current });
                }
                Err(error) => {
                    let _ = tx.send(Action::ProfileUpdateFailed(error.to_string()));
                }
            }
        });
        return;
    }
    app.status_msg = Some(format!("Importing {} (trusted host)...", pending.host));
    let option = clash_verge_core::config::PrfOption {
        trusted_hosts: Some(vec![pending.host.clone().into()]),
        ..Default::default()
    };
    spawn_import(action_tx, pending.url, Some(option));
}

/// Cancel the trust prompt. Only in-memory state changes: no exception is
/// written to `profiles.yaml`, so the host stays blocked and the update/import
/// keeps its failure status.
fn handle_cancel_trust(app: &mut App) {
    let was_update = app.pending_trust.as_ref().is_some_and(|pending| pending.uid.is_some());
    app.pending_trust = None;
    app.overlay = None;
    app.status_msg = Some(if was_update {
        "Update cancelled — host was not trusted".into()
    } else {
        "Import cancelled — host was not trusted".into()
    });
}

/// Open the SSRF trust prompt for a manual refresh blocked on `host`.
/// The pending state carries the profile `uid` so confirming persists the
/// trust into that profile's stored option before retrying.
fn begin_update_trust(app: &mut App, uid: String, host: String) {
    app.pending_trust = Some(TrustPending {
        url: String::new(),
        host: host.clone(),
        uid: Some(uid),
    });
    app.overlay = Some(Overlay::TrustConfirmation);
    app.focus = Focus::Content;
    app.status_msg = Some(format!(
        "{} — {}: {host}",
        app.tr("dialog.trust_update_title"),
        app.tr("dialog.target")
    ));
}

/// Decide whether a manual refresh of `item` must first ask the user to trust
/// its URL host. Returns `(uid, host)` when the host is genuinely SSRF-blocked
/// AND the profile's stored allowlist does not already cover it — i.e. the
/// update would fail right now and trusting this host would fix it.
///
/// Hosts already in the profile's `trusted_hosts` never prompt again: their
/// refresh passes the SSRF check through the stored allowlist. Background and
/// auto updates never route through this decision (they keep their existing
/// error surface), so only an explicit user refresh can open the prompt.
fn update_flow_decision(item: &clash_verge_core::config::PrfItem) -> Option<(String, String)> {
    let uid = item.uid.as_deref()?;
    let url = item.url.as_deref()?;
    let allowlist = crate::subscribe::from_url::trusted_hosts_allowlist(item.option.as_ref());
    if crate::subscribe::ssrf::check_url_host(url, &allowlist).is_ok() {
        return None;
    }
    let host = ssrf_blocked_host(url)?;
    Some((uid.to_string(), host))
}

/// Failure of a Settings service install that needed the core handoff.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ServiceInstallError {
    /// Stopping the running TUI core failed; the install was aborted (a
    /// second mihomo on the same config/socket/ports must never be spawned).
    HandoffStop(String),
    /// The sudo -S install transaction failed after the handoff.
    Transaction(String),
}

/// Install the system service from Settings with the handoff ordering.
///
/// A second mihomo on the same config/socket/ports must never be spawned, so
/// a running TUI-owned core is stopped BEFORE the sudo -S transaction runs
/// with start_now=true. `stop_core` is only awaited while `core_running`;
/// its failure aborts the install (the core is still up — installing would
/// conflict). Injectable so the stop-before-install ordering is testable
/// without sudo or a real core.
async fn handoff_then_install_service<F, G>(
    core_running: bool,
    stop_core: F,
    install: G,
) -> Result<(), ServiceInstallError>
where
    F: std::future::Future<Output = anyhow::Result<()>>,
    G: std::future::Future<Output = std::io::Result<()>>,
{
    if core_running {
        stop_core
            .await
            .map_err(|error| ServiceInstallError::HandoffStop(error.to_string()))?;
    }
    install
        .await
        .map_err(|error| ServiceInstallError::Transaction(error.to_string()))?;
    Ok(())
}

/// Handle a submitted password for the one pending sudo action (TUN setup /
/// service install / service uninstall).
///
/// A submit with no pending action is a stale duplicate Enter (e.g. the
/// second Enter of a double-press after the popup already closed): it is
/// ignored instead of aborting the TUI event loop. The spawned task only
/// runs when a pending action actually exists.
///
/// On success the resume context (`resume_start`, set when the TUN setup was
/// offered from the core-start prompt) travels with `TunSetupSucceeded` so
/// the pending core start resumes automatically; the explicit Settings flow
/// passes `None`.
///
/// A service install hands off a running TUI-owned core BEFORE the sudo -S
/// transaction: installing with start_now=true while the core runs would
/// spawn a second mihomo on the same config/socket/ports (the dual-core
/// conflict). The handoff stop failure aborts the install; a transaction
/// failure surfaces as a service action error that records the handoff.
fn handle_password_submit(
    app: &mut App,
    action_tx: &mpsc::UnboundedSender<Action>,
    manager: &crate::mihomo_manager::manager::MihomoManager,
) {
    let Some(pending) = app.pending_sudo.take() else {
        return;
    };
    let password: String = app.password_buffer.drain(..).collect();
    app.overlay = None;
    let tx = action_tx.clone();
    let manager = manager.clone();
    // Capture at submit time (the popup was opened on the running core): the
    // handoff must stop the core the user saw running when they confirmed.
    let core_running = app.core_state == CoreState::Running && manager.pid().is_some();
    tokio::spawn(async move {
        match pending {
            PendingSudoAction::TunSetup {
                binary, resume_start, ..
            } => match crate::commands::privilege::apply_tun_capability_with_password(&binary, &password) {
                Ok(()) => {
                    let _ = tx.send(Action::TunSetupSucceeded { resume_start });
                }
                Err(error) => {
                    let _ = tx.send(Action::CoreError(error.to_string()));
                }
            },
            PendingSudoAction::ServiceInstall {
                binary_path,
                config_dir,
            } => {
                let result =
                    handoff_then_install_service(core_running, async move { manager.stop().await }, async move {
                        crate::service_cmd::install_service_with_password(&binary_path, &config_dir, true, &password)
                    })
                    .await;
                match result {
                    Ok(()) => {
                        let _ = tx.send(Action::ServiceInstalled);
                    }
                    Err(ServiceInstallError::HandoffStop(message)) => {
                        let _ = tx.send(Action::ServiceActionFailed(message));
                    }
                    Err(ServiceInstallError::Transaction(message)) if core_running => {
                        // The core was already handed off (stopped) before the
                        // transaction ran; say so so the user is not surprised
                        // their running core is gone.
                        let _ = tx.send(Action::ServiceActionFailed(format!(
                            "{message} — the running core was stopped for the service handoff"
                        )));
                    }
                    Err(ServiceInstallError::Transaction(message)) => {
                        let _ = tx.send(Action::ServiceActionFailed(message));
                    }
                }
            }
            PendingSudoAction::ServiceUninstall => {
                match crate::service_cmd::uninstall_service_with_password(&password) {
                    Ok(()) => {
                        let _ = tx.send(Action::ServiceUninstalled);
                    }
                    Err(error) => {
                        let _ = tx.send(Action::ServiceActionFailed(error.to_string()));
                    }
                }
            }
        }
    });
}

/// Cancel the password popup. When a core start depended on this setup, the
/// start is abandoned: the transient `Starting` state is reset to `Stopped`
/// and nothing stale remains (no resume can fire).
fn handle_password_cancel(app: &mut App) {
    let pending = app.pending_sudo.take();
    app.overlay = None;
    app.password_buffer.clear();
    match pending {
        Some(PendingSudoAction::TunSetup {
            resume_start: Some(_), ..
        }) => {
            app.core_state = CoreState::Stopped;
            app.status_msg = Some("TUN setup cancelled — core not started".into());
        }
        Some(PendingSudoAction::TunSetup { .. }) => {
            app.status_msg = Some("TUN setup cancelled".into());
        }
        Some(PendingSudoAction::ServiceInstall { .. } | PendingSudoAction::ServiceUninstall) => {
            app.status_msg = Some(app.tr("settings.service_cancelled").into());
        }
        None => {}
    }
}

/// Pure decision for the TUI-native setup gate on core start: the inline
/// confirm is offered when the binary lacks the TUN file capability (and the
/// process is not root) OR the systemd-resolved DNS polkit rule is missing.
/// Injectable so all four combinations are testable without getcap/polkit.
fn tun_start_offers_setup(capable: bool, root: bool, rule_needed: bool) -> bool {
    let cap_ok = root || capable;
    !cap_ok || rule_needed
}

/// Open the TUI-native setup confirm dialog for a TUN-enabled core start
/// that needs the one-time setup. The pending state carries the resolved
/// binary and `resume_start: Some(enable_tun)` so a confirmed setup resumes
/// the start on success, plus the gate that fired (`reason`) so the skip key
/// knows whether starting anyway is safe.
fn begin_tun_setup_confirm(app: &mut App, binary: std::path::PathBuf, enable_tun: bool, reason: TunSetupReason) {
    app.pending_sudo = Some(PendingSudoAction::TunSetup {
        binary,
        resume_start: Some(enable_tun),
        reason,
    });
    app.overlay = Some(Overlay::TunSetupConfirmation);
    app.focus = Focus::Content;
    app.status_msg = Some(app.tun_setup_confirm_hint().into());
}

/// `y` on the core-start setup confirm: open the existing password popup.
/// `pending_tun` (binary + resume context) is kept so the password submit
/// can resume the pending start on success.
fn confirm_tun_setup(app: &mut App) {
    app.password_prompt = Some(app.tr("settings.tun_setup_prompt").into());
    app.password_buffer.clear();
    app.overlay = Some(Overlay::PasswordInput);
}

/// `n`/Esc/`q` on the core-start setup confirm: dismiss the dialog. What
/// "skip" means depends on why the setup was offered:
/// - missing file capability (hard gate): the spawn preflight would
///   hard-fail on the uncapped binary a moment later, so skip CANCELS the
///   start, resets the transient Starting state, and points at the TUN
///   setup command instead of a confusing resume-then-fail.
/// - missing DNS polkit rule only (soft gate, capability present): skip
///   starts anyway, preserving the passive DNS-rule warning.
fn skip_tun_setup_start(app: &mut App, action_tx: &mpsc::UnboundedSender<Action>) {
    let pending = app.pending_sudo.take();
    app.overlay = None;
    let Some(PendingSudoAction::TunSetup {
        resume_start, reason, ..
    }) = pending
    else {
        return;
    };
    let Some(enable_tun) = resume_start else {
        return;
    };
    if reason == TunSetupReason::MissingCapability {
        // The capability gate cannot be skipped: starting anyway would only
        // hit the spawn preflight hard-fail. Cancel cleanly instead.
        app.core_state = CoreState::Stopped;
        app.status_msg = Some(format!(
            "{} — run {} to install it",
            app.tr("settings.tun_capability_missing"),
            crate::commands::privilege::TUN_SETUP_COMMAND
        ));
        return;
    }
    // Capability present; only the DNS rule may be missing → start anyway.
    if crate::commands::privilege::resolve1_rule_needed(true) {
        app.status_msg = Some(format!(
            "{} — {}",
            app.tr("settings.tun_dns_rule_missing"),
            crate::commands::privilege::TUN_SETUP_COMMAND
        ));
    } else {
        app.status_msg = Some(app.tr("home.starting_core").into());
    }
    let _ = action_tx.send(Action::ResumeCoreStart { enable_tun });
}

/// Record a successful TUN setup transaction. With `resume_start` set, the
/// pending core start is resumed via `ResumeCoreStart`; `None` (explicit
/// Settings flow) just marks the TUI as privileged.
fn note_tun_setup_succeeded(app: &mut App, resume_start: Option<bool>, action_tx: &mpsc::UnboundedSender<Action>) {
    app.tun_privileged = true;
    app.status_msg = Some(if crate::commands::privilege::resolved_policy_present() {
        "TUN capability and DNS polkit rule installed (one-time sudo)".into()
    } else {
        "TUN capability installed (one-time sudo)".into()
    });
    if let Some(enable_tun) = resume_start {
        let _ = action_tx.send(Action::ResumeCoreStart { enable_tun });
    }
}

/// Whether the system service unit is installed (per the cached unit-file
/// presence probe, refreshed on Settings entry). `is-enabled` alone is NOT
/// used: an installed-but-disabled unit reports `disabled` and would be
/// misclassified as not-installed, dropping the uninstall offer.
fn service_installed(app: &App) -> bool {
    app.service_installed
}

/// Begin the Settings 'System service' row action.
///
/// Installed (unit present — including installed-but-disabled) → explicit
/// TUI confirm overlay; `y` then opens the password popup with a
/// `ServiceUninstall` pending action. Not installed → install, unless login
/// autostart is enabled: the two lifecycle mechanisms are mutually
/// exclusive, so installing while autostart is on is rejected with a clear
/// status pointing at the row to disable first.
fn begin_service_action(app: &mut App) {
    if service_installed(app) {
        app.overlay = Some(Overlay::ServiceUninstallConfirmation);
        app.focus = Focus::Content;
        app.status_msg = Some(app.tr("dialog.service_uninstall_hint").into());
    } else if app.auto_launch_enabled {
        // The system service (boot) and login autostart (login) are mutually
        // exclusive lifecycle mechanisms: both enabled would start a second
        // mihomo on the same config/socket/ports at the next login. Reject
        // the install and point at the autostart row.
        app.status_msg = Some(app.tr("settings.service_conflicts_autostart").into());
    } else {
        app.password_prompt = Some(app.tr("settings.service_install_prompt").into());
        app.password_buffer.clear();
        app.pending_sudo = Some(PendingSudoAction::ServiceInstall {
            binary_path: std::env::current_exe()
                .map(|path| path.to_string_lossy().into_owned())
                .unwrap_or_default(),
            config_dir: clash_verge_core::utils::dirs::app_home_dir()
                .map(|path| path.to_string_lossy().into_owned())
                .unwrap_or_default(),
        });
        app.overlay = Some(Overlay::PasswordInput);
    }
}

/// `y` on the service-uninstall confirm: open the existing password popup
/// with a `ServiceUninstall` pending action.
fn confirm_service_uninstall(app: &mut App) {
    app.password_prompt = Some(app.tr("settings.service_uninstall_prompt").into());
    app.password_buffer.clear();
    app.pending_sudo = Some(PendingSudoAction::ServiceUninstall);
    app.overlay = Some(Overlay::PasswordInput);
}

/// `n`/Esc/q on the service-uninstall confirm: dismiss, no transaction runs.
fn cancel_service_uninstall(app: &mut App) {
    app.pending_sudo = None;
    app.overlay = None;
    app.status_msg = Some(app.tr("settings.service_uninstall_cancelled").into());
}

/// Apply the login-autostart toggle end to end: persist `enable_auto_launch`
/// into verge.yaml, then apply the systemd `--user` unit via `unit_apply`
/// (the injected seam — production uses [`apply_autostart_unit`]).
///
/// On unit failure the persisted flag is rolled back so verge.yaml stays
/// aligned with the real systemd state, and the error (systemctl stderr,
/// e.g. "Failed to connect to bus" on a headless session) is surfaced.
async fn toggle_autostart(
    enabled: bool,
    binary_path: &str,
    config_dir: &str,
    unit_apply: &(dyn Fn(bool, &str, &str) -> Result<(), String> + Send + Sync),
) -> Result<(), String> {
    let mut gui_config = clash_verge_core::config::IVerge::new().await;
    let previous = gui_config.enable_auto_launch;
    gui_config.enable_auto_launch = Some(enabled);
    gui_config.save_file().await.map_err(|error| error.to_string())?;
    match unit_apply(enabled, binary_path, config_dir) {
        Ok(()) => Ok(()),
        Err(error) => {
            // Keep verge.yaml aligned with the real systemd state so the next
            // toggle decision starts from reality.
            gui_config.enable_auto_launch = previous;
            let _ = gui_config.save_file().await;
            Err(error)
        }
    }
}

/// Production seam for [`toggle_autostart`]: enable writes the unit +
/// `daemon-reload` + `enable` (no `--now`); disable runs `disable` + removes
/// the unit. systemctl errors surface verbatim.
fn apply_autostart_unit(enabled: bool, binary_path: &str, config_dir: &str) -> Result<(), String> {
    if enabled {
        crate::autostart::enable(binary_path, config_dir).map_err(|error| error.to_string())
    } else {
        crate::autostart::disable().map_err(|error| error.to_string())
    }
}

/// Outcome of the Settings 'Launch at login' toggle decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AutostartToggle {
    /// Enabling is blocked: the system service is installed+enabled and the
    /// two lifecycle mechanisms are mutually exclusive (both enabled would
    /// spawn a second mihomo at the next boot/login).
    BlockedByService,
    /// The toggle may proceed with `enabled`.
    Proceed { enabled: bool },
}

/// Decide what the Settings 'Launch at login' toggle may do. Enabling is
/// rejected while the system service is installed+enabled; disabling (and
/// toggling with no enabled service) is always allowed.
fn autostart_toggle_decision(app: &App) -> AutostartToggle {
    let enabled = !app.auto_launch_enabled;
    if enabled && app.service_enabled == "enabled" {
        AutostartToggle::BlockedByService
    } else {
        AutostartToggle::Proceed { enabled }
    }
}

fn next_clash_mode(current: &str) -> &'static str {
    match current.to_ascii_lowercase().as_str() {
        "global" => "direct",
        "direct" => "rule",
        _ => "global",
    }
}

async fn apply_clash_mode(
    api: &crate::mihomo_api::MihomoApi,
    mode: &str,
    core_running: bool,
) -> Result<String, String> {
    // Serialize with runtime commits so a stale IClashTemp snapshot cannot
    // overwrite a concurrent profile/TUN write to clash.yaml.
    let previous_mode = {
        let _guard = RUNTIME_CONFIG_IO.lock().await;
        let mut clash = clash_verge_core::config::IClashTemp::new().await;
        let previous = clash.get_mode().unwrap_or_else(|| "rule".into());
        let mut patch = serde_yaml_ng::Mapping::new();
        patch.insert("mode".into(), mode.into());
        clash.patch_config(&patch);
        clash.save_config().await.map_err(|error| error.to_string())?;
        previous
    };
    if core_running && let Err(error) = api.patch_mode(mode).await {
        // Keep disk aligned with the failed API update so the next Start does
        // not silently adopt a mode the UI reported as failed.
        let _guard = RUNTIME_CONFIG_IO.lock().await;
        let mut clash = clash_verge_core::config::IClashTemp::new().await;
        let mut patch = serde_yaml_ng::Mapping::new();
        patch.insert("mode".into(), previous_mode.into());
        clash.patch_config(&patch);
        let _ = clash.save_config().await;
        return Err(error.to_string());
    }
    Ok(mode.to_string())
}

async fn apply_chain_config(
    api: &crate::mihomo_api::MihomoApi,
    chain_nodes: &[String],
    enable_tun: bool,
) -> Result<std::path::PathBuf, String> {
    // Sidecar backup for diagnostics; the commit path also keeps an in-memory rollback copy.
    let path = clash_verge_core::utils::dirs::clash_path().map_err(|error| error.to_string())?;
    if path.exists() {
        let original = tokio::fs::read(&path)
            .await
            .map_err(|error| format!("failed to back up {}: {error}", path.display()))?;
        let backup_path = path.with_extension("yaml.tui-chain-backup");
        tokio::fs::write(&backup_path, &original)
            .await
            .map_err(|error| format!("failed to write {}: {error}", backup_path.display()))?;
    }

    commit_runtime_config(api, enable_tun, true, None, |mut config| {
        let entries = config
            .get("proxies")
            .and_then(Value::as_sequence)
            .ok_or_else(|| "active config has no proxies list".to_string())?;
        let mut proxies = entries
            .iter()
            .enumerate()
            .map(|(index, entry)| {
                entry
                    .as_mapping()
                    .cloned()
                    .ok_or_else(|| format!("proxies[{index}] is not a mapping"))
            })
            .collect::<Result<Vec<_>, _>>()?;

        crate::chain::build_chain_config(chain_nodes, &mut proxies).map_err(|error| error.to_string())?;
        config.insert(
            "proxies".into(),
            Value::Sequence(proxies.into_iter().map(Value::Mapping).collect()),
        );
        Ok(config)
    })
    .await
}

/// Find the (group_name, node_name) at a flat index in proxy groups.
fn find_node_at_index(
    groups: &std::collections::HashMap<String, crate::mihomo_api::types::ProxyGroup>,
    expanded_group: Option<&str>,
    target: usize,
) -> Option<(String, String)> {
    proxy_display_rows(groups, expanded_group)
        .get(target)
        .and_then(|row| row.node_identity())
        .map(|(group, node)| (group.to_string(), node.to_string()))
}

/// Policy pseudo-nodes that must never receive a delay test.
const BATCH_POLICY_PSEUDO_NODES: [&str; 5] = ["DIRECT", "REJECT", "REJECT-DROP", "PASS", "COMPATIBLE"];

/// Maximum number of concurrent delay requests for one batch.
const BATCH_MAX_CONCURRENCY: usize = 4;

/// Collect the deduplicated set of real leaf proxy targets for a batch delay
/// test. A name is a real leaf only if it is not a policy pseudo-node and it
/// is not itself a proxy group (a group is a key whose `all` is present).
/// The result is sorted for a stable, deterministic test order.
fn batch_delay_targets(
    groups: &std::collections::HashMap<String, crate::mihomo_api::types::ProxyGroup>,
) -> Vec<String> {
    let mut targets: Vec<String> = groups
        .values()
        .filter_map(|group| group.all.as_ref().filter(|nodes| !nodes.is_empty()))
        .flatten()
        .filter(|name| {
            let name = name.as_str();
            !BATCH_POLICY_PSEUDO_NODES.contains(&name) && groups.get(name).is_none_or(|group| group.all.is_none())
        })
        .cloned()
        .collect();
    targets.sort_unstable();
    targets.dedup();
    targets
}

/// Outcome of deciding what to do when the user presses the batch-delay shortcut.
#[derive(Debug, Clone, PartialEq, Eq)]
enum BatchDelayOutcome {
    /// No batch is running; `targets` are the freshly computed leaf targets.
    Started { targets: Vec<String> },
    /// A batch is already running; report its current progress instead of
    /// scheduling another one.
    InProgress { done: usize, total: usize },
    /// The testable set is empty; do not create any delay request.
    NoTargets,
}

/// Decide what the batch-delay shortcut does with the current app state.
/// Guarded so a second invocation never schedules a second batch.
fn begin_batch_delay(app: &mut App) -> BatchDelayOutcome {
    if let Some((done, total)) = app.batch_delay {
        return BatchDelayOutcome::InProgress { done, total };
    }
    let targets = batch_delay_targets(&app.proxy_groups);
    if targets.is_empty() {
        return BatchDelayOutcome::NoTargets;
    }
    app.batch_delay = Some((0, targets.len()));
    BatchDelayOutcome::Started { targets }
}

/// Count one finished batch result. Only batch result events call this, so the
/// batch never blocks on any individual node; when the last result arrives the
/// in-progress marker (and duplicate-start guard) is cleared.
fn advance_batch(app: &mut App) {
    let Some((done, total)) = app.batch_delay else {
        return;
    };
    let next = done + 1;
    if next >= total {
        app.batch_delay = None;
    } else {
        app.batch_delay = Some((next, total));
    }
    app.status_msg = Some(format!("{}: {next}/{total}", app.tr("proxies.batch_delay")));
}

/// Record one single-node delay result in the shared delay map and status bar.
/// Never touches batch progress: a single-node `t` result must not advance or
/// clear the active batch.
fn note_delay_result(app: &mut App, name: String, delay: Option<u64>) {
    app.delay_map.insert(name, delay);
    if let Some(delay) = delay {
        app.status_msg = Some(format!("Delay: {delay}ms"));
    }
}

/// Record one single-node delay failure. Same contract as [`note_delay_result`].
fn note_delay_failed(app: &mut App, name: String, error: String) {
    app.delay_map.insert(name.clone(), None);
    app.status_msg = Some(format!("Delay failed for {name}: {error}"));
}

/// Record one batch delay result: identical per-node rendering to the
/// single-node path, then advance the active batch progress.
fn note_batch_delay_result(app: &mut App, name: String, delay: Option<u64>) {
    note_delay_result(app, name, delay);
    advance_batch(app);
}

/// Record one batch delay failure: identical per-node rendering to the
/// single-node path, then advance the active batch progress.
fn note_batch_delay_failed(app: &mut App, name: String, error: String) {
    note_delay_failed(app, name, error);
    advance_batch(app);
}

/// Count total flat items in proxy groups: one per group header + one per node.
fn count_flat_nodes(
    groups: &std::collections::HashMap<String, crate::mihomo_api::types::ProxyGroup>,
    expanded_group: Option<&str>,
) -> usize {
    proxy_display_rows(groups, expanded_group).len()
}

pub async fn run(config_dir: std::path::PathBuf) -> anyhow::Result<()> {
    let guard = std::sync::Arc::new(tokio::sync::Mutex::new(TerminalGuard::new()?));
    let (action_tx, mut action_rx) = mpsc::unbounded_channel::<Action>();

    let manager = crate::commands::build_manager(config_dir).await?;
    manager.set_action_tx(action_tx.clone());

    let mut app = App::new();
    app.gui_config = clash_verge_core::config::IVerge::new().await;
    app.language = Language::from_config(app.gui_config.language.as_deref());
    app.core_config = clash_verge_core::config::IClashTemp::new().await;
    app.clash_mode = app.core_config.get_mode().unwrap_or_else(|| "rule".into());

    // Load profiles on start
    if let Ok(store) = crate::profile_store::store::ProfileStore::snapshot().await {
        app.selected_index = store.selected_index();
        app.profiles = store.items();
        app.status_msg = Some(format!("{} profiles loaded", app.profiles.len()));
    }

    let mut events = EventStream::new();
    let mut render_tick = time::interval(Duration::from_millis(100));
    let mut runtime_refresh_tick = time::interval(Duration::from_secs(1));
    let mut auto_update_tick = time::interval(Duration::from_secs(30));
    auto_update_tick.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
    let mut auto_update_in_flight = false;
    // Re-read profiles.yaml every 5 min so external interval edits (GUI/user)
    // take effect without restarting the TUI.
    let mut profiles_refresh_tick = time::interval(Duration::from_secs(300));
    profiles_refresh_tick.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
    let auto_update_scheduler = std::sync::Arc::new(tokio::sync::Mutex::new(
        crate::subscribe::scheduler::AutoUpdateScheduler::new(),
    ));
    let mut rendered_view = app.view;

    // Detect a core the CLI itself started earlier (standalone socket).
    // The GUI is never probed.
    let api = manager.api();
    let tx = action_tx.clone();
    tokio::spawn(async move {
        if api.version().await.is_ok() {
            let _ = tx.send(Action::CoreStarted {
                version: None,
                binary_path: None,
                binary_source: None,
            });
        }
        // If no controller is available, the user can press s to start one.
    });

    // Read-only TUN capability state for the Settings view (no download,
    // no sudo). Uses the already-resolved binary or the no-download
    // candidate; refreshes again on CoreStarted / after explicit setup.
    {
        let m = manager.clone();
        let tx = action_tx.clone();
        tokio::spawn(async move {
            let binary = m
                .binary_path()
                .or_else(crate::mihomo_manager::binary::candidate_without_install);
            if let Some(path) = binary {
                let _ = tx.send(Action::TunCapabilityState(
                    crate::commands::privilege::has_tun_capability(&path),
                ));
            }
        });
    }

    loop {
        tokio::select! {
            maybe_event = events.next() => {
                match maybe_event {
                    Some(Ok(Event::Resize(_, _))) => {
                        guard.lock().await.reset_screen()?;
                    }
                    Some(Ok(Event::Key(key))) if key.kind != KeyEventKind::Release => {
                        match &app.input_mode {
                            InputMode::Importing(buffer) => {
                                match key.code {
                                    KeyCode::Enter => {
                                        let url = buffer.clone();
                                        app.input_mode = InputMode::Normal;
                                        app.status_msg = Some("Importing...".into());
                                        let tx = action_tx.clone();
                                        tokio::spawn(async move {
                                            let _ = tx.send(Action::ConfirmImport(url));
                                        });
                                    }
                                    KeyCode::Esc => {
                                        app.input_mode = InputMode::Normal;
                                    }
                                    KeyCode::Backspace => {
                                        let mut b = buffer.clone();
                                        b.pop();
                                        app.input_mode = InputMode::Importing(b);
                                    }
                                    KeyCode::Char(c) => {
                                        let mut b = buffer.clone();
                                        b.push(c);
                                        app.input_mode = InputMode::Importing(b);
                                    }
                                    _ => {}
                                }
                            }
                            InputMode::Normal => {
                                if app.overlay == Some(Overlay::Filter) {
                                    match input::map_key(key, key_context(&app)) {
                                        Some(Action::SubmitFilter) => {
                                            let query = app.filter.take().unwrap_or_default();
                                            let submitted = (!query.trim().is_empty()).then_some(query);
                                            match app.view {
                                                View::Connections => app.connection_filter = submitted,
                                                View::Logs => app.log_filter = submitted,
                                                view => {
                                                    app.status_msg = Some(format!(
                                                        "Filtering is not available in {} yet",
                                                        view.label()
                                                    ));
                                                }
                                            }
                                            app.overlay = None;
                                            app.focus = Focus::Content;
                                        }
                                        Some(Action::DismissOverlay) => dismiss_overlay(&mut app),
                                        _ => match key.code {
                                            KeyCode::Backspace => {
                                                if let Some(filter) = app.filter.as_mut() {
                                                    filter.pop();
                                                }
                                            }
                                            KeyCode::Char(character) => {
                                                if let Some(filter) = app.filter.as_mut() {
                                                    filter.push(character);
                                                }
                                            }
                                            _ => {}
                                        },
                                    }
                                } else if let Some(action) = input::map_key(key, key_context(&app)) {
                                    match action {
                                        // Password popup input is handled uniformly in the action
                                        // channel match (buffer updates, submit, cancel).
                                        action if forward_to_channel(&action) => {
                                            let _ = action_tx.send(action);
                                        }
                                        Action::Quit => break,
                                        Action::StartCore => {
                                            // Daily path: resolve the binary and run the
                                            // read-only TUN preflight (no sudo/setcap/askpass
                                            // here — the manager repeats it before spawn). When
                                            // the one-time setup (file capability and/or the DNS
                                            // polkit rule) is missing, the app offers the
                                            // TUI-native setup confirm inline instead of
                                            // hard-blocking or relying on system dialogs.
                                            let enable_tun =
                                                app.gui_config.enable_tun_mode.unwrap_or(false);
                                            app.core_state = CoreState::Starting;
                                            app.status_msg =
                                                Some(app.tr("home.starting_core").into());
                                            let m = manager.clone();
                                            let tx = action_tx.clone();
                                            tokio::spawn(async move {
                                                if enable_tun {
                                                    match crate::mihomo_manager::binary::resolve_or_install()
                                                        .await
                                                    {
                                                        Ok(resolved) => {
                                                            let capable = crate::commands::privilege::has_tun_capability(
                                                                &resolved.path,
                                                            );
                                                            let root = crate::commands::privilege::running_as_root();
                                                            let needs_setup = tun_start_offers_setup(
                                                                capable,
                                                                root,
                                                                crate::commands::privilege::resolve1_rule_needed(true),
                                                            );
                                                            if needs_setup {
                                                                // Record which gate fired: dismissing a
                                                                // capability-missing prompt must cancel the
                                                                // start, while a missing-DNS-rule prompt may
                                                                // start anyway.
                                                                let reason = if root || capable {
                                                                    TunSetupReason::MissingDnsRule
                                                                } else {
                                                                    TunSetupReason::MissingCapability
                                                                };
                                                                let _ = tx.send(Action::TunSetupPrompt {
                                                                    binary: resolved.path,
                                                                    enable_tun,
                                                                    reason,
                                                                });
                                                                return;
                                                            }
                                                        }
                                                        Err(error) => {
                                                            let _ = tx.send(Action::CoreError(
                                                                error.to_string(),
                                                            ));
                                                            return;
                                                        }
                                                    }
                                                }
                                                if let Err(error) = start_core_with_tun(&m, enable_tun).await
                                                {
                                                    let _ = tx.send(Action::CoreError(error));
                                                }
                                                // On success, manager emits CoreStarted.
                                            });
                                        }
                                        Action::StopCore => {
                                            let m = manager.clone();
                                            tokio::spawn(async move { let _ = m.stop().await; });
                                        }
                                        Action::RestartCore => {
                                            app.core_state = CoreState::Starting;
                                            app.status_msg = Some(app.tr("home.starting_core").into());
                                            let m = manager.clone();
                                            let tx = action_tx.clone();
                                            let enable_tun =
                                                app.gui_config.enable_tun_mode.unwrap_or(false);
                                            tokio::spawn(async move {
                                                let config =
                                                    clash_verge_core::config::IClashTemp::new().await.0;
                                                if let Err(error) =
                                                    write_runtime_config(config, enable_tun).await
                                                {
                                                    let _ = tx.send(Action::CoreError(error));
                                                    return;
                                                }
                                                if let Err(error) = m.restart().await {
                                                    let _ = tx.send(Action::CoreError(error.to_string()));
                                                }
                                            });
                                        }
                                        Action::StartImport => {
                                            app.input_mode = InputMode::Importing(String::new());
                                        }
                                        Action::MoveNext => {
                                            if app.focus == Focus::Menu {
                                                menu_move(&mut app, &action_tx, true);
                                            } else {
                                            match app.view {
                                                View::Profiles => {
                                                    let len = app.profiles.len();
                                                    if len > 0 {
                                                    let sel = app.selected_index;
                                                        let next = if sel + 1 >= len { 0 } else { sel + 1 };
                                                        app.selected_index = next;
                                                    }
                                                }
                                                View::Proxies => {
                                                    let total = count_flat_nodes(
                                                        &app.proxy_groups,
                                                        app.expanded_proxy_group.as_deref(),
                                                    );
                                                    if total > 0 {
                                                        app.node_selected_index = if app.node_selected_index + 1 >= total {
                                                            0
                                                        } else {
                                                            app.node_selected_index + 1
                                                        };
                                                    }
                                                }
                                                View::Connections => move_connection_selection(&mut app, true),
                                                View::Logs => move_log_selection(&mut app, true),
                                                View::Settings => {
                                                    app.settings_selected_index =
                                                        (app.settings_selected_index + 1)
                                                            % crate::ui::views::settings::SETTINGS_ROW_COUNT;
                                                }
                                                _ => {}
                                            }
                                            }
                                        }
                                        Action::MovePrevious => {
                                            if app.focus == Focus::Menu {
                                                menu_move(&mut app, &action_tx, false);
                                            } else {
                                            match app.view {
                                                View::Profiles => {
                                                    let len = app.profiles.len();
                                                    if len > 0 {
                                                        let sel = app.selected_index;
                                                        let prev = if sel == 0 { len.saturating_sub(1) } else { app.selected_index.saturating_sub(1) };
                                                        app.selected_index = prev;
                                                    }
                                                }
                                                View::Proxies => {
                                                    let total = count_flat_nodes(
                                                        &app.proxy_groups,
                                                        app.expanded_proxy_group.as_deref(),
                                                    );
                                                    if total > 0 {
                                                        app.node_selected_index = if app.node_selected_index == 0 {
                                                            total.saturating_sub(1)
                                                        } else {
                                                            app.node_selected_index.saturating_sub(1)
                                                        };
                                                    }
                                                }
                                                View::Connections => move_connection_selection(&mut app, false),
                                                View::Logs => move_log_selection(&mut app, false),
                                                View::Settings => {
                                                    let count = crate::ui::views::settings::SETTINGS_ROW_COUNT;
                                                    app.settings_selected_index =
                                                        (app.settings_selected_index + count - 1) % count;
                                                }
                                                _ => {}
                                            }
                                            }
                                        }
                                        Action::Activate => {
                                            if app.focus == Focus::Menu {
                                                app.focus = Focus::Content;
                                                continue;
                                            }
                                            match app.view {
                                                View::Profiles => {
                                                    // Profile tab: switch profile
                                                    if let Some(item) = app.profiles.get(app.selected_index)
                                                        && let Some(uid) = item.uid.clone()
                                                    {
                                                        let name = item.name.clone().unwrap_or_default();
                                                        let itype = item.itype.clone().unwrap_or_default();
                                                        app.status_msg = Some(format!("Switching to {name}..."));
                                                        let api = manager.api();
                                                        let tx = action_tx.clone();
                                                        let enable_tun =
                                                            app.gui_config.enable_tun_mode.unwrap_or(false);
                                                        let core_running = app.core_state == CoreState::Running;
                                                        if itype == "remote" {
                                                            let item = item.clone();
                                                            tokio::spawn(async move {
                                                                let previous_uid = match crate::profile_store::store::ProfileStore::replace_current_locked(
                                                                    uid.as_str(),
                                                                )
                                                                .await
                                                                {
                                                                    Ok(previous) => previous,
                                                                    Err(error) => {
                                                                        let _ = tx.send(Action::CoreError(format!(
                                                                            "profile switch: {error}"
                                                                        )));
                                                                        return;
                                                                    }
                                                                };
                                                                match reload_remote_profile(
                                                                    &api,
                                                                    &item,
                                                                    enable_tun,
                                                                    core_running,
                                                                )
                                                                .await
                                                                {
                                                                    Ok(()) => {
                                                                        if core_running {
                                                                            let _ = tx.send(Action::ProxiesRefresh);
                                                                        }
                                                                    }
                                                                    Err(error) => {
                                                                        let _ = crate::profile_store::store::ProfileStore::restore_current_if_matches(
                                                                            uid.as_str(),
                                                                            previous_uid.as_deref(),
                                                                        )
                                                                        .await;
                                                                        let _ = tx.send(Action::CoreError(format!(
                                                                            "profile reload: {error}"
                                                                        )));
                                                                    }
                                                                }
                                                            });
                                                        } else {
                                                            let item = item.clone();
                                                            tokio::spawn(async move {
                                                                let previous_uid = match crate::profile_store::store::ProfileStore::replace_current_locked(
                                                                    uid.as_str(),
                                                                )
                                                                .await
                                                                {
                                                                    Ok(previous) => previous,
                                                                    Err(error) => {
                                                                        let _ = tx.send(Action::CoreError(format!(
                                                                            "profile switch: {error}"
                                                                        )));
                                                                        return;
                                                                    }
                                                                };
                                                                let profiles_dir =
                                                                    clash_verge_core::utils::dirs::app_profiles_dir()
                                                                        .unwrap_or_default();
                                                                match crate::chain::resolve_chain(&item, &profiles_dir).await
                                                                {
                                                                    Ok(chain) => {
                                                                        match commit_runtime_config(
                                                                            &api,
                                                                            enable_tun,
                                                                            core_running,
                                                                            Some(&item),
                                                                            |mut config| {
                                                                                crate::chain::apply_chain_to_config(
                                                                                    &mut config, &chain,
                                                                                );
                                                                                Ok(config)
                                                                            },
                                                                        )
                                                                        .await
                                                                        {
                                                                            Ok(_) => {
                                                                                if core_running {
                                                                                    let _ = tx.send(Action::ProxiesRefresh);
                                                                                }
                                                                            }
                                                                            Err(error) => {
                                                                                let _ = crate::profile_store::store::ProfileStore::restore_current_if_matches(
                                                                                    uid.as_str(),
                                                                                    previous_uid.as_deref(),
                                                                                )
                                                                                .await;
                                                                                let _ = tx.send(Action::CoreError(
                                                                                    format!("config write: {error}"),
                                                                                ));
                                                                            }
                                                                        }
                                                                    }
                                                                    Err(e) => {
                                                                        let _ = crate::profile_store::store::ProfileStore::restore_current_if_matches(
                                                                            uid.as_str(),
                                                                            previous_uid.as_deref(),
                                                                        )
                                                                        .await;
                                                                        let _ = tx
                                                                            .send(Action::CoreError(format!("chain: {e}")));
                                                                    }
                                                                }
                                                            });
                                                        }
                                                    }
                                                }
                                                View::Proxies => {
                                                    let selected_row = proxy_display_rows(
                                                        &app.proxy_groups,
                                                        app.expanded_proxy_group.as_deref(),
                                                    )
                                                    .get(app.node_selected_index)
                                                    .cloned();
                                                    if let Some(ProxyDisplayRow::Group { name, node_count, .. }) = selected_row {
                                                        app.expanded_proxy_group = Some(name.clone());
                                                        app.node_selected_index = proxy_display_rows(
                                                            &app.proxy_groups,
                                                            Some(&name),
                                                        )
                                                        .iter()
                                                        .position(|row| {
                                                            matches!(row, ProxyDisplayRow::Group { name: row_name, .. } if row_name == &name)
                                                        })
                                                        .unwrap_or_default();
                                                        app.status_msg = Some(format!(
                                                            "Browsing {name}: {node_count} choices"
                                                        ));
                                                    } else if app.chain_mode {
                                                        if let Some((_, name)) = find_node_at_index(
                                                            &app.proxy_groups,
                                                            app.expanded_proxy_group.as_deref(),
                                                            app.node_selected_index,
                                                        )
                                                            && !app.chain_nodes.contains(&name) {
                                                                app.chain_nodes.push(name.clone());
                                                                app.status_msg = Some(format!("Chain: {}", app.chain_nodes.join(" → ")));
                                                            }
                                                    } else if let Some((group, name)) = find_node_at_index(
                                                        &app.proxy_groups,
                                                        app.expanded_proxy_group.as_deref(),
                                                        app.node_selected_index,
                                                    ) {
                                                        app.status_msg = Some(format!("Switching to {name}..."));
                                                        let api = manager.api();
                                                        let g = group.clone();
                                                        let n = name.clone();
                                                        let tx = action_tx.clone();
                                                        tokio::spawn(async move {
                                                            match api.select_proxy(&g, &n).await {
                                                                Ok(()) => {
                                                                    let _ = tx.send(Action::ProxiesRefresh);
                                                                }
                                                                Err(error) => {
                                                                    let _ = tx.send(Action::ProxiesFailed(error.to_string()));
                                                                }
                                                            }
                                                        });
                                                    }
                                                }
                                                View::Connections => begin_connection_close(&mut app),
                                                View::Rules => {
                                                    // Activate on rules view: update selected rule provider.
                                                    if app.rules_focus_providers
                                                        && let Some(provider) = app.rule_providers.get(app.rules_selected_index)
                                                    {
                                                        let name = provider.name.clone();
                                                        app.status_msg = Some(format!("Updating rule provider {name}..."));
                                                        let api = manager.api();
                                                        let tx = action_tx.clone();
                                                        tokio::spawn(async move {
                                                            match api.update_rule_provider(&name).await {
                                                                Ok(()) => {
                                                                    let _ = tx.send(Action::RuleProviderUpdated(name));
                                                                }
                                                                Err(error) => {
                                                                    let _ = tx.send(Action::RuleProviderUpdateFailed {
                                                                        name,
                                                                        error: error.to_string(),
                                                                    });
                                                                }
                                                            }
                                                        });
                                                    }
                                                }
                                                View::Settings => {
                                                    match app.settings_selected_index {
                                                        0 => {
                                                            let next_language = app.language.next();
                                                            let mut updated_config = app.gui_config.clone();
                                                            updated_config.language =
                                                                Some(next_language.config_code().into());
                                                            match updated_config.save_file().await {
                                                                Ok(()) => {
                                                                    app.gui_config = updated_config;
                                                                    app.language = next_language;
                                                                    app.status_msg = Some(format!(
                                                                        "{}: {}",
                                                                        app.tr("settings.language_saved"),
                                                                        next_language.display_name()
                                                                    ));
                                                                }
                                                                Err(error) => {
                                                                    app.status_msg = Some(format!(
                                                                        "{}: {error}",
                                                                        app.tr("settings.language_save_failed")
                                                                    ));
                                                                }
                                                            }
                                                        }
                                                        1 => {
                                                            let enabled = !app
                                                                .gui_config
                                                                .enable_system_proxy
                                                                .unwrap_or(false);
                                                            let previous = app.gui_config.clone();
                                                            let mut updated = previous.clone();
                                                            updated.enable_system_proxy = Some(enabled);
                                                            match updated.save_file().await {
                                                                Ok(()) => {
                                                                    let host = updated
                                                                        .proxy_host
                                                                        .as_deref()
                                                                        .unwrap_or("127.0.0.1");
                                                                    let port = app.core_config.get_mixed_port();
                                                                    let apply_result = if enabled {
                                                                        crate::sys_proxy::set_system_proxy(host, port)
                                                                    } else {
                                                                        crate::sys_proxy::unset_system_proxy()
                                                                    };
                                                                    match apply_result {
                                                                        Ok(()) => {
                                                                            app.gui_config = updated;
                                                                            app.status_msg = Some(
                                                                                if enabled {
                                                                                    app.tr("settings.sysproxy_on")
                                                                                } else {
                                                                                    app.tr("settings.sysproxy_off")
                                                                                }
                                                                                .into(),
                                                                            );
                                                                        }
                                                                        Err(error) => {
                                                                            let _ = previous.save_file().await;
                                                                            app.gui_config = previous;
                                                                            app.status_msg = Some(format!(
                                                                                "{}: {error}",
                                                                                app.tr("settings.save_failed")
                                                                            ));
                                                                        }
                                                                    }
                                                                }
                                                                Err(error) => {
                                                                    app.status_msg = Some(format!(
                                                                        "{}: {error}",
                                                                        app.tr("settings.save_failed")
                                                                    ));
                                                                }
                                                            }
                                                        }
                                                        2 => 'tun_toggle: {
                                                            let enabled =
                                                                !app.gui_config.enable_tun_mode.unwrap_or(false);
                                                            if enabled {
                                                                // Read-only preflight BEFORE persisting: never write a TUN-on
                                                                // config that cannot run. Uses the already-resolved binary or
                                                                // the no-download candidate; no sudo/setcap/askpass here — the
                                                                // spawn preflight repeats the check authoritatively.
                                                                let known = manager.binary_path().or_else(
                                                                    crate::mihomo_manager::binary::candidate_without_install,
                                                                );
                                                                if let Some(binary) = known
                                                                    && let Err(error) = crate::commands::privilege::require_tun_capability(&binary)
                                                                {
                                                                    app.status_msg = Some(format!(
                                                                        "{}: {error}",
                                                                        app.tr("settings.save_failed")
                                                                    ));
                                                                    break 'tun_toggle;
                                                                }
                                                                // Same TUI-native warning as the start path: a missing DNS
                                                                // polkit rule means the next start would hit system dialogs.
                                                                if crate::commands::privilege::resolve1_rule_needed(true) {
                                                                    app.status_msg = Some(format!(
                                                                        "{} — {}",
                                                                        app.tr("settings.tun_dns_rule_missing"),
                                                                        crate::commands::privilege::TUN_SETUP_COMMAND
                                                                    ));
                                                                }
                                                            }
                                                            let mut updated = app.gui_config.clone();
                                                            updated.enable_tun_mode = Some(enabled);
                                                            match updated.save_file().await {
                                                                Ok(()) => {
                                                                    app.gui_config = updated;
                                                                    if app.core_state == CoreState::Running {
                                                                        app.status_msg = Some(
                                                                            if enabled {
                                                                                app.tr("settings.tun_on")
                                                                            } else {
                                                                                app.tr("settings.tun_off")
                                                                            }
                                                                            .into(),
                                                                        );
                                                                        let owns_core = manager.pid().is_some();
                                                                        if owns_core {
                                                                            app.core_state = CoreState::Starting;
                                                                        }
                                                                        let m = manager.clone();
                                                                        let tx = action_tx.clone();
                                                                        let g = guard.clone();
                                                                        tokio::spawn(async move {
                                                                            match apply_tun_runtime(&m, g, owns_core, enabled).await {
                                                                                Ok(_) => {
                                                                                    if !owns_core {
                                                                                        let _ = tx.send(Action::ProxiesRefresh);
                                                                                    }
                                                                                }
                                                                                Err(error) => {
                                                                                    let _ = tx.send(Action::CoreError(error));
                                                                                }
                                                                            }
                                                                        });
                                                                    } else if let Err(error) =
                                                                        write_tun_runtime(enabled).await
                                                                    {
                                                                        app.status_msg = Some(format!(
                                                                            "{}: {error}",
                                                                            app.tr("settings.save_failed")
                                                                        ));
                                                                    } else {
                                                                        // Core is stopped: persist only; apply on next start.
                                                                        app.status_msg = Some(
                                                                            if enabled {
                                                                                app.tr("settings.tun_on")
                                                                            } else {
                                                                                app.tr("settings.tun_off")
                                                                            }
                                                                            .into(),
                                                                        );
                                                                    }
                                                                }
                                                                Err(error) => {
                                                                    app.status_msg = Some(format!(
                                                                        "{}: {error}",
                                                                        app.tr("settings.save_failed")
                                                                    ));
                                                                }
                                                            }
                                                        }
                                                        3 => 'tun_setup: {
                                                            // Explicit TUN setup — the ONLY TUI authorization action.
                                                            // Start/toggle never open the password popup; this row does.
                                                            let known = manager.binary_path().or_else(
                                                                crate::mihomo_manager::binary::candidate_without_install,
                                                            );
                                                            if let Some(binary) = known
                                                                && crate::commands::privilege::has_tun_capability(&binary)
                                                            {
                                                                app.tun_privileged = true;
                                                                app.status_msg =
                                                                    Some(app.tr("settings.tun_setup_present").into());
                                                                break 'tun_setup;
                                                            }
                                                            let tx = action_tx.clone();
                                                            tokio::spawn(async move {
                                                                match crate::mihomo_manager::binary::resolve_or_install()
                                                                    .await
                                                                {
                                                                    Ok(resolved) => {
                                                                        if crate::commands::privilege::has_tun_capability(
                                                                            &resolved.path,
                                                                        ) {
                                                                            let _ = tx.send(
                                                                                Action::TunCapabilityState(true),
                                                                            );
                                                                        } else {
                                                                            let _ = tx.send(
                                                                                Action::TunSetupRequested(resolved.path),
                                                                            );
                                                                        }
                                                                    }
                                                                    Err(error) => {
                                                                        let _ = tx.send(Action::CoreError(
                                                                            error.to_string(),
                                                                        ));
                                                                    }
                                                                }
                                                            });
                                                        }
                                                        4 => {
                                                            let next = next_clash_mode(&app.clash_mode);
                                                            let api = manager.api();
                                                            let tx = action_tx.clone();
                                                            let core_running = app.core_state == CoreState::Running;
                                                            tokio::spawn(async move {
                                                                match apply_clash_mode(&api, next, core_running).await {
                                                                    Ok(mode) => {
                                                                        let _ = tx.send(Action::ModeChanged {
                                                                            mode,
                                                                            announce: true,
                                                                        });
                                                                    }
                                                                    Err(error) => {
                                                                        let _ =
                                                                            tx.send(Action::ModeChangeFailed(error));
                                                                    }
                                                                }
                                                            });
                                                        }
                                                        5 => {
                                                            // System service: install (password popup, sudo -S
                                                            // transaction with start_now=true) when not installed;
                                                            // explicit confirm → password popup → uninstall when
                                                            // installed. Never the CLI's sudo -A/askpass path (the
                                                            // askpass subprocess would fight the raw-mode TUI).
                                                            begin_service_action(&mut app);
                                                        }
                                                        6 => {
                                                            // Launch at login: on/off toggle backed by the
                                                            // systemd --user unit (enable never passes --now, so
                                                            // the running TUI core is not shadowed by a second
                                                            // foreground core). Mutually exclusive with the
                                                            // system service: enabling while the service is
                                                            // installed+enabled is rejected with a clear status
                                                            // (both enabled would collide at the next
                                                            // boot/login). The toggle flips the displayed
                                                            // systemd state; the persisted flag follows it.
                                                            match autostart_toggle_decision(&app) {
                                                                AutostartToggle::BlockedByService => {
                                                                    app.status_msg = Some(
                                                                        app.tr("settings.autostart_conflicts_service")
                                                                            .into(),
                                                                    );
                                                                }
                                                                AutostartToggle::Proceed { enabled } => {
                                                                    let binary_path = std::env::current_exe()
                                                                        .map(|path| path.to_string_lossy().into_owned())
                                                                        .unwrap_or_default();
                                                                    let config_dir =
                                                                        clash_verge_core::utils::dirs::app_home_dir()
                                                                            .map(|path| path.to_string_lossy().into_owned())
                                                                            .unwrap_or_default();
                                                                    let tx = action_tx.clone();
                                                                    tokio::spawn(async move {
                                                                        match toggle_autostart(
                                                                            enabled,
                                                                            &binary_path,
                                                                            &config_dir,
                                                                            &apply_autostart_unit,
                                                                        )
                                                                        .await
                                                                        {
                                                                            Ok(()) => {
                                                                                let _ = tx.send(Action::AutoLaunchChanged {
                                                                                    enabled,
                                                                                });
                                                                            }
                                                                            Err(error) => {
                                                                                let _ = tx.send(Action::AutoLaunchFailed(error));
                                                                            }
                                                                        }
                                                                    });
                                                                }
                                                            }
                                                        }
                                                        _ => {}
                                                    }
                                                }
                                                _ => {}
                                            }
                                        }
                                        Action::NodeDelayTest => {
                                            if let Some((_, name)) = find_node_at_index(
                                                &app.proxy_groups,
                                                app.expanded_proxy_group.as_deref(),
                                                app.node_selected_index,
                                            ) {
                                                let api = manager.api();
                                                let n = name.clone();
                                                let tx = action_tx.clone();
                                                app.status_msg = Some(format!("Testing delay for {name}..."));
                                                tokio::spawn(async move {
                                                    match api.delay_test(&n, "http://www.gstatic.com/generate_204", 5000).await {
                                                        Ok(d) => { let _ = tx.send(Action::DelayResult(n.clone(), Some(d.delay))); }
                                                        Err(error) => { let _ = tx.send(Action::DelayFailed(n, error.to_string())); }
                                                    }
                                                });
                                            }
                                        }
                                        Action::SwitchView(view) => {
                                            // Route through the shared transition so menu
                                            // navigation and the number keys refresh the
                                            // same per-view probes (Settings re-probes the
                                            // read-only service/autostart state).
                                            transition_to_view(&mut app, &action_tx, view);
                                        }
                                        Action::CycleFocus => {
                                            // On Rules view, cycle between Rules ↔ Providers panels.
                                            if app.view == View::Rules && app.focus == Focus::Content {
                                                app.rules_focus_providers = !app.rules_focus_providers;
                                                app.rules_selected_index = 0;
                                            } else {
                                                app.focus = app.focus.cycle();
                                            }
                                        }
                                        Action::FocusMenu => {
                                            app.focus = Focus::Menu;
                                        }
                                        Action::FocusContent => {
                                            app.focus = Focus::Content;
                                        }
                                        Action::StartFilter => {
                                            app.filter = Some(String::new());
                                            app.overlay = Some(Overlay::Filter);
                                            app.focus = Focus::Content;
                                        }
                                        Action::ToggleHelp => {
                                            app.overlay = match app.overlay {
                                                Some(Overlay::Help) => None,
                                                _ => Some(Overlay::Help),
                                            };
                                        }
                                        Action::DismissOverlay => {
                                            dismiss_overlay(&mut app);
                                        }
                                        Action::RequestCloseConnection => {
                                            begin_connection_close(&mut app);
                                        }
                                        Action::RequestCloseAllConnections => {
                                            app.overlay = Some(Overlay::CloseAllConnectionsConfirmation);
                                            app.status_msg =
                                                Some("Close ALL connections? Press Enter to confirm".into());
                                            app.focus = Focus::Content;
                                        }
                                        Action::ConfirmCloseConnection(id) => {
                                            if close_confirmation_is_current(&app, &id) {
                                                let _ = action_tx.send(Action::ConfirmCloseConnection(id));
                                            } else {
                                                app.status_msg = Some("Connection close confirmation expired".into());
                                            }
                                        }
                                        Action::NodeDelayAll => {
                                            let api = std::sync::Arc::new(manager.api());
                                            let tx = action_tx.clone();
                                            match begin_batch_delay(&mut app) {
                                                BatchDelayOutcome::Started { targets } => {
                                                    app.status_msg = Some(format!(
                                                        "{}: 0/{}",
                                                        app.tr("proxies.batch_delay"),
                                                        targets.len()
                                                    ));
                                                    tokio::spawn(async move {
                                                        let semaphore = std::sync::Arc::new(
                                                            tokio::sync::Semaphore::new(BATCH_MAX_CONCURRENCY),
                                                        );
                                                        let mut handles = Vec::new();
                                                        for name in targets {
                                                            let permit =
                                                                match semaphore.clone().acquire_owned().await {
                                                                    Ok(permit) => permit,
                                                                    // Semaphore closed: stop scheduling further requests.
                                                                    Err(_) => break,
                                                                };
                                                            let api = api.clone();
                                                            let tx = tx.clone();
                                                            handles.push(tokio::spawn(async move {
                                                                let _permit = permit;
                                                                match api.delay_test(&name, "http://www.gstatic.com/generate_204", 5000).await {
                                                                    Ok(d) => { let _ = tx.send(Action::BatchDelayResult(name, Some(d.delay))); }
                                                                    Err(error) => { let _ = tx.send(Action::BatchDelayFailed(name, error.to_string())); }
                                                                }
                                                            }));
                                                        }
                                                        for handle in handles {
                                                            let _ = handle.await;
                                                        }
                                                    });
                                                }
                                                BatchDelayOutcome::InProgress { done, total } => {
                                                    app.status_msg = Some(format!(
                                                        "{}: {done}/{total}",
                                                        app.tr("proxies.batch_delay")
                                                    ));
                                                }
                                                BatchDelayOutcome::NoTargets => {
                                                    app.status_msg = Some(app.tr("proxies.no_testable").into());
                                                }
                                            }
                                        }
                                        Action::UpdateProfile => {
                                            // Manual refresh: a host the SSRF
                                            // check blocks (and the profile does
                                            // not already trust) opens the same
                                            // interactive trust prompt as import.
                                            // Background/auto updates never route
                                            // here, so they can never prompt.
                                            let needs_trust = app
                                                .profiles
                                                .get(app.selected_index)
                                                .and_then(update_flow_decision);
                                            if let Some((uid, host)) = needs_trust {
                                                let _ =
                                                    action_tx.send(Action::UpdateNeedsTrust { uid, host });
                                            } else {
                                                app.status_msg = Some("Updating subscriptions...".into());
                                                let selected_uid = app
                                                    .profiles
                                                    .get(app.selected_index)
                                                    .and_then(|item| item.uid.clone())
                                                    .map(|u| u.to_string());
                                                let tx = action_tx.clone();
                                                tokio::spawn(async move {
                                                    let result = if let Some(uid) = selected_uid {
                                                        crate::profile_store::store::ProfileStore::update_remote_locked(
                                                            &uid, None,
                                                        )
                                                        .await
                                                        .map(|is_current| (uid, is_current))
                                                    } else {
                                                        crate::profile_store::store::ProfileStore::update_all_remote_locked(
                                                        )
                                                        .await
                                                        .map(|currents| {
                                                            let uid = currents
                                                                .first()
                                                                .map(|u| u.to_string())
                                                                .unwrap_or_default();
                                                            let is_current = !currents.is_empty();
                                                            (uid, is_current)
                                                        })
                                                    };
                                                    match result {
                                                        Ok((uid, is_current)) => {
                                                            let _ = tx.send(Action::ProfileUpdated {
                                                                uid,
                                                                is_current,
                                                            });
                                                        }
                                                        Err(error) => {
                                                            let _ = tx.send(Action::ProfileUpdateFailed(
                                                                error.to_string(),
                                                            ));
                                                        }
                                                    }
                                                });
                                            }
                                        }
                                        Action::ToggleChainMode => {
                                            app.chain_mode = !app.chain_mode;
                                            app.status_msg = Some(if app.chain_mode {
                                                app.chain_nodes.clear();
                                                String::from("Chain edit ON: Enter add, a apply, x clear")
                                            } else {
                                                app.chain_nodes.clear();
                                                String::from("Chain OFF")
                                            });
                                        }
                                        Action::ApplyChain => {
                                            if app.chain_nodes.len() >= 2 {
                                                let nodes = app.chain_nodes.clone();
                                                let enable_tun =
                                                    app.gui_config.enable_tun_mode.unwrap_or(false);
                                                let api = manager.api();
                                                let tx = action_tx.clone();
                                                app.status_msg = Some("Applying chain...".into());
                                                tokio::spawn(async move {
                                                    match apply_chain_config(&api, &nodes, enable_tun).await {
                                                        Ok(_) => {
                                                            let _ = tx.send(Action::ChainApplied(nodes));
                                                            let _ = tx.send(Action::ProxiesRefresh);
                                                        }
                                                        Err(error) => {
                                                            let _ = tx.send(Action::ChainFailed(error));
                                                        }
                                                    }
                                                });
                                            } else {
                                                app.status_msg = Some("Need >=2 nodes for chain".into());
                                            }
                                        }
                                        Action::ClearChain => {
                                            app.chain_nodes.clear();
                                            app.status_msg = Some("Chain cleared".into());
                                        }
                                        Action::CycleClashMode => {
                                            let _ = action_tx.send(Action::CycleClashMode);
                                        }
                                        Action::OpenEditor(target) => {
                                            let config_path = match target {
                                                EditorTarget::Verge => {
                                                    clash_verge_core::utils::dirs::verge_path().ok()
                                                }
                                                EditorTarget::Dns => None,
                                            };
                                            if let Some(path) = config_path {
                                                let snapshot = crate::editor::snapshot(&path).ok();
                                                let mut guard = guard.lock().await;
                                                let edit_result = crate::editor::edit_file_blocking(&mut guard, &path);
                                                match edit_result {
                                                    Ok(()) => {
                                                        match crate::editor::validate_yaml(&path) {
                                                            Ok(()) => {
                                                                app.gui_config = clash_verge_core::config::IVerge::new().await;
                                                                app.language = Language::from_config(app.gui_config.language.as_deref());
                                                                // Reload DNS enable from the (possibly edited) verge config.
                                                                if let EditorTarget::Dns = target {
                                                                    // DNS editing not yet wired as a separate file.
                                                                }
                                                                app.status_msg = Some("Config saved and validated".into());
                                                            }
                                                            Err(e) => {
                                                                if let Some(data) = snapshot {
                                                                    let _ = crate::editor::restore_snapshot(&path, &data);
                                                                }
                                                                app.gui_config = clash_verge_core::config::IVerge::new().await;
                                                                app.status_msg = Some(format!("Invalid YAML, restored: {e}"));
                                                            }
                                                        }
                                                    }
                                                    Err(e) => {
                                                        app.status_msg = Some(format!("Editor error: {e}"));
                                                    }
                                                }
                                            } else {
                                                app.status_msg = Some("Config file path not available".into());
                                            }
                                        }
                                        _ => {}
                                    }
                                }
                            }
                        }
                    }
                    Some(Err(_)) | None => break,
                    _ => {}
                }
            }

            action = action_rx.recv() => {
                match action {
                    Some(Action::CoreStarted {
                        version,
                        binary_path,
                        binary_source,
                    }) => {
                        app.core_state = CoreState::Running;
                        app.core_pid = manager.pid();
                        // Re-assert the system proxy once the controller actually answers:
                        // the GNOME setting is global and can be clobbered by other tools
                        // (e.g. the GUI's sysproxy guard restoring state on exit), leaving
                        // verge.yaml enabled while the OS mode is 'none'. CoreStarted fires
                        // right after spawn() — before the API/listeners are up — so a
                        // readiness probe arms Action::SysProxyReassert and the event loop
                        // itself performs the final toggle/port re-read and the apply. That
                        // keeps the apply serialized with the user's toggle actions (a
                        // deferred task can never re-enable a proxy they just turned off),
                        // never points the OS at a dead port, and never downgrades PAC.
                        if app.gui_config.enable_system_proxy.unwrap_or(false)
                            && !app.gui_config.proxy_auto_config.unwrap_or(false)
                        {
                            let api = manager.api();
                            let tx = action_tx.clone();
                            let probe_manager = manager.clone();
                            tokio::spawn(async move {
                                // Attached cores (no managed pid) are probed once: their
                                // CoreStarted already implies an answered controller.
                                // Spawned cores are probed for as long as they live.
                                loop {
                                    if api.version().await.is_ok() {
                                        let _ = tx.send(Action::SysProxyReassert);
                                        break;
                                    }
                                    if probe_manager.pid().is_none() {
                                        break;
                                    }
                                    tokio::time::sleep(Duration::from_millis(500)).await;
                                }
                            });
                        }
                        // Keep the Settings capability state in sync with the
                        // binary that actually got spawned (may have changed
                        // after an upgrade or a fresh setup).
                        if let Some(path) = manager.binary_path() {
                            app.tun_privileged =
                                crate::commands::privilege::has_tun_capability(&path);
                        }
                        if let Some(version) = version.clone() {
                            app.core_version = Some(version);
                        } else if app.core_version.is_none() {
                            // Attached to an existing controller — ask the API for version.
                            let api = manager.api();
                            let tx = action_tx.clone();
                            tokio::spawn(async move {
                                if let Ok(v) = api.version().await {
                                    let _ = tx.send(Action::CoreStarted {
                                        version: Some(v.version),
                                        binary_path: None,
                                        binary_source: None,
                                    });
                                }
                            });
                        }

                        let pid = app.core_pid.map(|p| p.to_string()).unwrap_or_else(|| "-".into());
                        app.status_msg = Some(match binary_source.as_deref() {
                            Some("downloaded") => format!(
                                "{} · {} · {} · pid {pid}",
                                app.tr("home.core_started"),
                                version.as_deref().unwrap_or("mihomo"),
                                app.tr("home.core_downloaded"),
                            ),
                            Some("cached") => format!(
                                "{} · {} · {} · pid {pid}",
                                app.tr("home.core_started"),
                                version.as_deref().unwrap_or("mihomo"),
                                app.tr("home.core_cached"),
                            ),
                            Some("system") => format!(
                                "{} · {} · {} · pid {pid}",
                                app.tr("home.core_started"),
                                version.as_deref().unwrap_or("mihomo"),
                                app.tr("home.core_system"),
                            ),
                            _ if version.is_some() => format!(
                                "{} · {} · pid {pid}",
                                app.tr("home.core_started"),
                                version.as_deref().unwrap_or("mihomo"),
                            ),
                            _ => app.tr("home.core_attached").into(),
                        });
                        if let Some(path) = binary_path {
                            tracing::info!(target: "mihomo", "core binary: {path}");
                        }
                        let _ = action_tx.send(Action::ProxiesRefresh);
                        let _ = action_tx.send(Action::TrafficRefresh);
                        let _ = action_tx.send(Action::ConnectionsRefresh);
                        let _ = action_tx.send(Action::LogsRefresh);

                        let api = manager.api();
                        let tx = action_tx.clone();
                        tokio::spawn(async move {
                            if let Ok(mode) = api.get_mode().await {
                                let _ = tx.send(Action::ModeChanged {
                                    mode,
                                    announce: false,
                                });
                            }
                        });
                    }
                    Some(Action::CoreExited(0)) => {
                        app.core_state = CoreState::Stopped;
                        app.core_pid = None;
                        app.clear_runtime_caches();
                    }
                    Some(Action::CoreError(msg)) => {
                        app.core_state = CoreState::Error(msg.clone());
                        app.core_pid = None;
                        app.status_msg = Some(format!("{}: {msg}", app.tr("status.error")));
                        app.clear_runtime_caches();
                    }
                    Some(Action::ProfileImported) => {
                        app.status_msg = Some("Profile imported successfully".into());
                        if let Ok(store) = crate::profile_store::store::ProfileStore::snapshot().await {
                            app.profiles = store.items();
                            // Auto-select last (newly imported) profile
                            if !app.profiles.is_empty() {
                                let last = app.profiles.len().saturating_sub(1);
                                app.selected_index = last;
                                // Write runtime config for the imported profile, then start if needed.
                                if let Some(item) = app.profiles.get(last).cloned() {
                                    let api = manager.api();
                                    let tx = action_tx.clone();
                                    let enable_tun =
                                        app.gui_config.enable_tun_mode.unwrap_or(false);
                                    let core_running = app.core_state == CoreState::Running;
                                    let should_start = !core_running;
                                    let manager = manager.clone();
                                    tokio::spawn(async move {
                                        if let Err(error) = reload_remote_profile(
                                            &api,
                                            &item,
                                            enable_tun,
                                            core_running,
                                        )
                                        .await
                                        {
                                            let _ = tx.send(Action::CoreError(format!(
                                                "profile reload: {error}"
                                            )));
                                            return;
                                        }
                                        if should_start {
                                            let _ = manager.start().await;
                                        } else {
                                            let _ = tx.send(Action::ProxiesRefresh);
                                        }
                                    });
                                }
                            }
                        }
                    }
                    Some(Action::ProxiesFetched(groups)) => {
                        app.runtime_loading.proxies = false;
                        app.runtime_errors.proxies = None;
                        app.proxy_groups = groups;
                        let expanded_is_available = app
                            .expanded_proxy_group
                            .as_deref()
                            .and_then(|name| app.proxy_groups.get(name))
                            .and_then(|group| group.all.as_ref())
                            .is_some_and(|nodes| !nodes.is_empty());
                        if !expanded_is_available {
                            app.expanded_proxy_group = first_selectable_proxy_group(&app.proxy_groups);
                            app.node_selected_index = 0;
                        }
                        let rows = proxy_display_rows(&app.proxy_groups, app.expanded_proxy_group.as_deref());
                        app.node_selected_index = app.node_selected_index.min(rows.len().saturating_sub(1));
                        let group_count = rows
                            .iter()
                            .filter(|row| matches!(row, ProxyDisplayRow::Group { .. }))
                            .count();
                        let choice_count: usize = app
                            .proxy_groups
                            .values()
                            .filter_map(|group| group.all.as_ref().filter(|nodes| !nodes.is_empty()))
                            .map(Vec::len)
                            .sum();
                        app.status_msg = Some(format!(
                            "{group_count} selectable groups, {choice_count} choices loaded"
                        ));
                    }
                    Some(Action::ProxiesRefresh)
                        if !app.runtime_loading.proxies => {
                            app.runtime_loading.proxies = true;
                            app.runtime_errors.proxies = None;
                            let api = manager.api();
                            let tx = action_tx.clone();
                            tokio::spawn(async move {
                                match api.get_proxies().await {
                                    Ok(data) => {
                                        let _ = tx.send(Action::ProxiesFetched(data.proxies));
                                    }
                                    Err(error) => {
                                        let _ = tx.send(Action::ProxiesFailed(error.to_string()));
                                    }
                                }
                            });
                        }
                    Some(Action::ProxiesFailed(error)) => {
                        app.runtime_loading.proxies = false;
                        app.runtime_errors.proxies = Some(error);
                    }
                    Some(Action::DelayResult(name, delay)) => {
                        note_delay_result(&mut app, name, delay);
                    }
                    Some(Action::DelayFailed(name, error)) => {
                        note_delay_failed(&mut app, name, error);
                    }
                    Some(Action::BatchDelayResult(name, delay)) => {
                        note_batch_delay_result(&mut app, name, delay);
                    }
                    Some(Action::BatchDelayFailed(name, error)) => {
                        note_batch_delay_failed(&mut app, name, error);
                    }
                    Some(Action::ChainApplied(nodes)) => {
                        app.chain_mode = false;
                        app.chain_nodes.clear();
                        app.status_msg = Some(format!("Chain applied: {}", nodes.join(" -> ")));
                    }
                    Some(Action::ChainFailed(error)) => {
                        app.status_msg = Some(format!("Chain not applied: {error}"));
                    }
                    Some(Action::TrafficRefresh)
                        if !app.runtime_loading.traffic => {
                            app.runtime_loading.traffic = true;
                            app.runtime_errors.traffic = None;
                            let api = manager.api();
                            let tx = action_tx.clone();
                            tokio::spawn(async move {
                                if let Err(error) = receive_traffic_stream(api, tx.clone()).await {
                                    let _ = tx.send(Action::TrafficFailed(error));
                                }
                            });
                        }
                    Some(Action::TrafficFetched(traffic)) => {
                        app.runtime_errors.traffic = None;
                        app.traffic = Some(traffic);
                    }
                    Some(Action::TrafficFailed(error)) => {
                        app.runtime_loading.traffic = false;
                        app.runtime_errors.traffic = Some(error);
                    }
                    Some(Action::ConnectionsRefresh)
                        if !app.runtime_loading.connections => {
                            app.runtime_loading.connections = true;
                            app.runtime_errors.connections = None;
                            let api = manager.api();
                            let tx = action_tx.clone();
                            tokio::spawn(async move {
                                match api.get_connections().await {
                                    Ok(data) => {
                                        let _ = tx.send(Action::ConnectionsFetched(data.connections));
                                    }
                                    Err(error) => {
                                        let _ = tx.send(Action::ConnectionsFailed(error.to_string()));
                                    }
                                }
                            });
                        }
                    Some(Action::ConnectionsFetched(connections)) => {
                        app.runtime_loading.connections = false;
                        app.runtime_errors.connections = None;
                        if app
                            .selected_connection_id
                            .as_ref()
                            .is_some_and(|id| !connections.iter().any(|connection| &connection.id == id))
                        {
                            app.selected_connection_id = None;
                            app.pending_connection_close = None;
                        }
                        app.connections = connections;
                        let ids = visible_connection_ids(&app);
                        if let Some(id) = ids.first() {
                            if app.selected_connection_id.is_none() {
                                app.selected_connection_id = Some(id.clone());
                            }
                            app.connection_selected_index = app
                                .selected_connection_id
                                .as_deref()
                                .and_then(|selected| ids.iter().position(|id| id == selected))
                                .unwrap_or_default();
                        } else {
                            app.selected_connection_id = None;
                            app.pending_connection_close = None;
                            app.connection_selected_index = 0;
                        }
                    }
                    Some(Action::ConnectionsFailed(error)) => {
                        app.runtime_loading.connections = false;
                        app.runtime_errors.connections = Some(error);
                    }
                    Some(Action::LogsRefresh)
                        if !app.runtime_loading.logs => {
                            app.runtime_loading.logs = true;
                            app.runtime_errors.logs = None;
                            let api = manager.api();
                            let tx = action_tx.clone();
                            tokio::spawn(async move {
                                if let Err(error) = receive_log_stream(api, tx.clone()).await {
                                    let _ = tx.send(Action::LogsFailed(error));
                                }
                            });
                        }
                    Some(Action::LogReceived(log)) => {
                        app.runtime_errors.logs = None;
                        app.logs.push(log);
                        const MAX_LOG_ENTRIES: usize = 1_000;
                        if app.logs.len() > MAX_LOG_ENTRIES {
                            let excess = app.logs.len() - MAX_LOG_ENTRIES;
                            app.logs.drain(..excess);
                        }
                        app.log_selected_index = app
                            .log_selected_index
                            .min(visible_log_count(&app).saturating_sub(1));
                    }
                    Some(Action::LogsFailed(error)) => {
                        app.runtime_loading.logs = false;
                        app.runtime_errors.logs = Some(error);
                    }
                    Some(Action::ConfirmCloseConnection(id))
                        if close_confirmation_is_current(&app, &id) => {
                            app.pending_connection_close = None;
                            app.overlay = None;
                            let api = manager.api();
                            let tx = action_tx.clone();
                            tokio::spawn(async move {
                                match api.close_connection(&id).await {
                                    Ok(()) => {
                                        let _ = tx.send(Action::ConnectionClosed(id));
                                    }
                                    Err(error) => {
                                        let _ = tx.send(Action::CloseConnectionFailed {
                                            id,
                                            error: error.to_string(),
                                        });
                                    }
                                }
                            });
                        }
                    Some(Action::ConnectionClosed(id)) => {
                        app.status_msg = Some(format!("Closed connection {id}"));
                        app.selected_connection_id = None;
                        let _ = action_tx.send(Action::ConnectionsRefresh);
                    }
                    Some(Action::CloseConnectionFailed { id, error }) => {
                        app.runtime_errors.connections = Some(format!("Could not close {id}: {error}"));
                    }
                    Some(Action::ConfirmCloseAllConnections)
                        if app.overlay == Some(Overlay::CloseAllConnectionsConfirmation) =>
                    {
                        app.overlay = None;
                        app.status_msg = Some("Closing all connections...".into());
                        let api = manager.api();
                        let tx = action_tx.clone();
                        tokio::spawn(async move {
                            match api.close_all_connections().await {
                                Ok(()) => {
                                    let _ = tx.send(Action::AllConnectionsClosed);
                                }
                                Err(error) => {
                                    let _ = tx.send(Action::CloseAllConnectionsFailed(error.to_string()));
                                }
                            }
                        });
                    }
                    Some(Action::AllConnectionsClosed) => {
                        app.status_msg = Some("All connections closed".into());
                        let _ = action_tx.send(Action::ConnectionsRefresh);
                    }
                    Some(Action::CloseAllConnectionsFailed(error)) => {
                        app.status_msg = Some(format!("Close all failed: {error}"));
                    }
                    Some(Action::RulesRefresh) if !app.rules_loading => {
                        app.rules_loading = true;
                        app.rules_error = None;
                        let api = manager.api();
                        let tx = action_tx.clone();
                        tokio::spawn(async move {
                            match api.get_rules().await {
                                Ok(resp) => {
                                    let _ = tx.send(Action::RulesFetched(resp.rules));
                                }
                                Err(error) => {
                                    let _ = tx.send(Action::RulesFailed(error.to_string()));
                                }
                            }
                        });
                    }
                    Some(Action::RulesFetched(rules)) => {
                        app.rules_loading = false;
                        app.rules = rules;
                        app.rules_selected_index = app.rules_selected_index.min(app.rules.len().saturating_sub(1));
                    }
                    Some(Action::RulesFailed(error)) => {
                        app.rules_loading = false;
                        app.rules_error = Some(error);
                    }
                    Some(Action::RuleProvidersRefresh) if !app.rule_providers_loading => {
                        app.rule_providers_loading = true;
                        app.rule_providers_error = None;
                        let api = manager.api();
                        let tx = action_tx.clone();
                        tokio::spawn(async move {
                            match api.get_rule_providers().await {
                                Ok(resp) => {
                                    let providers: Vec<_> = resp.providers.into_values().collect();
                                    let _ = tx.send(Action::RuleProvidersFetched(providers));
                                }
                                Err(error) => {
                                    let _ = tx.send(Action::RuleProvidersFailed(error.to_string()));
                                }
                            }
                        });
                    }
                    Some(Action::RuleProvidersFetched(providers)) => {
                        app.rule_providers_loading = false;
                        app.rule_providers = providers;
                        app.rules_selected_index = app.rules_selected_index.min(app.rule_providers.len().saturating_sub(1));
                    }
                    Some(Action::RuleProvidersFailed(error)) => {
                        app.rule_providers_loading = false;
                        app.rule_providers_error = Some(error);
                    }
                    Some(Action::RuleProviderUpdated(name)) => {
                        app.status_msg = Some(format!("Rule provider updated: {name}"));
                        let _ = action_tx.send(Action::RuleProvidersRefresh);
                    }
                    Some(Action::RuleProviderUpdateFailed { name, error }) => {
                        app.status_msg = Some(format!("Failed to update {name}: {error}"));
                    }
                    Some(Action::ConfirmImport(url)) => {
                        // Ordinary import path: SSRF protection stays enabled.
                        // The safety check runs off the event loop; if the host
                        // is blocked, the user gets an explicit trust prompt
                        // instead of a silent failure.
                        let tx = action_tx.clone();
                        tokio::spawn(async move {
                            match ssrf_blocked_host(&url) {
                                Some(host) => {
                                    let _ = tx.send(Action::ImportNeedsTrust { url, host });
                                }
                                None => {
                                    let tx = tx.clone();
                                    spawn_import(&tx, url, None);
                                }
                            }
                        });
                    }
                    Some(Action::ImportNeedsTrust { url, host }) => {
                        app.pending_trust = Some(TrustPending {
                            url,
                            host: host.clone(),
                            uid: None,
                        });
                        app.overlay = Some(Overlay::TrustConfirmation);
                        app.focus = Focus::Content;
                        app.status_msg = Some(format!(
                            "{} — {}: {host}",
                            app.tr("dialog.trust_title"),
                            app.tr("dialog.target")
                        ));
                    }
                    Some(Action::UpdateNeedsTrust { uid, host }) => {
                        begin_update_trust(&mut app, uid, host);
                    }
                    Some(Action::ConfirmTrustImport) => {
                        handle_confirm_trust(&mut app, &action_tx);
                    }
                    Some(Action::CancelTrustImport) => {
                        handle_cancel_trust(&mut app);
                    }
                    Some(Action::ProfileImportFailed(error)) => {
                        app.status_msg = Some(format!("Import failed: {error}"));
                    }
                    Some(Action::ProfileUpdated { uid, is_current }) => {
                        app.status_msg = Some(if uid.is_empty() {
                            "Subscriptions updated".into()
                        } else {
                            format!("Updated profile {uid}")
                        });
                        if let Ok(store) = crate::profile_store::store::ProfileStore::snapshot().await {
                            app.profiles = store.items();
                        }
                        if is_current {
                            let api = manager.api();
                            let tx = action_tx.clone();
                            let reload_uid = uid.clone();
                            let enable_tun = app.gui_config.enable_tun_mode.unwrap_or(false);
                            let core_running = app.core_state == CoreState::Running;
                            tokio::spawn(async move {
                                match crate::subscribe::scheduler::reload_current_profile(
                                    &api, &reload_uid, enable_tun, core_running,
                                )
                                .await
                                {
                                    Ok(()) => {
                                        if core_running {
                                            let _ = tx.send(Action::ProxiesRefresh);
                                        }
                                    }
                                    Err(error) => {
                                        let _ = tx.send(Action::CoreError(format!(
                                            "profile reload: {error}"
                                        )));
                                    }
                                }
                            });
                        }
                    }
                    Some(Action::ProfileUpdateFailed(error)) => {
                        app.status_msg = Some(format!("Update failed: {error}"));
                    }
                    Some(Action::CycleClashMode) => {
                        let next = next_clash_mode(&app.clash_mode);
                        let api = manager.api();
                        let tx = action_tx.clone();
                        let core_running = app.core_state == CoreState::Running;
                        tokio::spawn(async move {
                            match apply_clash_mode(&api, next, core_running).await {
                                Ok(mode) => {
                                    let _ = tx.send(Action::ModeChanged {
                                        mode,
                                        announce: true,
                                    });
                                }
                                Err(error) => {
                                    let _ = tx.send(Action::ModeChangeFailed(error));
                                }
                            }
                        });
                    }
                    Some(Action::ModeChanged { mode, announce }) => {
                        app.clash_mode = mode.clone();
                        app.core_config = clash_verge_core::config::IClashTemp::new().await;
                        if announce {
                            app.status_msg = Some(format!("{}: {mode}", app.tr("settings.mode_set")));
                        }
                    }
                    Some(Action::ModeChangeFailed(error)) => {
                        app.status_msg = Some(format!("{}: {error}", app.tr("common.failed")));
                    }
                    Some(Action::ProbeNotice(message)) => {
                        app.status_msg = Some(message);
                    }
                    Some(Action::TunSetupPrompt {
                        binary,
                        enable_tun,
                        reason,
                    }) => {
                        // Preflight found a missing capability or DNS polkit
                        // rule: offer the TUI-native setup confirm inline so a
                        // system polkit dialog is never the first notice.
                        begin_tun_setup_confirm(&mut app, binary, enable_tun, reason);
                    }
                    Some(Action::ConfirmTunSetup) => {
                        confirm_tun_setup(&mut app);
                    }
                    Some(Action::SkipTunSetupStart) => {
                        skip_tun_setup_start(&mut app, &action_tx);
                    }
                    Some(Action::TunSetupSucceeded { resume_start }) => {
                        note_tun_setup_succeeded(&mut app, resume_start, &action_tx);
                    }
                    Some(Action::ResumeCoreStart { enable_tun }) => {
                        let m = manager.clone();
                        let tx = action_tx.clone();
                        tokio::spawn(async move {
                            if let Err(error) = start_core_with_tun(&m, enable_tun).await {
                                let _ = tx.send(Action::CoreError(error));
                            }
                            // On success, manager emits CoreStarted.
                        });
                    }
                    Some(Action::TunCapabilityState(privileged)) => {
                        app.tun_privileged = privileged;
                    }
                    Some(Action::TunSetupRequested(binary)) => {
                        app.password_prompt = Some(app.tr("settings.tun_setup_prompt").into());
                        app.password_buffer.clear();
                        app.pending_sudo = Some(PendingSudoAction::TunSetup {
                            binary,
                            resume_start: None,
                            reason: TunSetupReason::MissingCapability,
                        });
                        app.overlay = Some(Overlay::PasswordInput);
                    }
                    Some(Action::PasswordChar(c)) => {
                        app.password_buffer.push(c);
                    }
                    Some(Action::PasswordBackspace) => {
                        app.password_buffer.pop();
                    }
                    Some(Action::PasswordCancel) => {
                        handle_password_cancel(&mut app);
                    }
                    Some(Action::PasswordSubmit) => {
                        handle_password_submit(&mut app, &action_tx, &manager);
                    }
                    Some(Action::ConfirmServiceUninstall) => {
                        confirm_service_uninstall(&mut app);
                    }
                    Some(Action::CancelServiceUninstall) => {
                        cancel_service_uninstall(&mut app);
                    }
                    Some(Action::ServiceInstalled) => {
                        app.status_msg = Some(app.tr("settings.service_installed").into());
                        let _ = action_tx.send(Action::ServiceStatusRefresh);
                    }
                    Some(Action::ServiceUninstalled) => {
                        app.status_msg = Some(app.tr("settings.service_uninstalled").into());
                        let _ = action_tx.send(Action::ServiceStatusRefresh);
                    }
                    Some(Action::ServiceActionFailed(error)) => {
                        app.status_msg = Some(format!("{}: {error}", app.tr("settings.service_failed")));
                        let _ = action_tx.send(Action::ServiceStatusRefresh);
                    }
                    Some(Action::SysProxyReassert)
                        // Final decision on the event loop against LIVE state: a toggle
                        // processed before this action can never be clobbered by a deferred
                        // task. The port is re-read fallibly — a config that fails to parse
                        // must not substitute the template's default port, and the startup
                        // snapshot may be equally stale, so the apply is skipped rather than
                        // guessed. The sync socket check runs immediately before the apply
                        // with no awaits in between, so a queued CoreStopped cannot
                        // interleave and reopen the dead-port race.
                        if app.gui_config.enable_system_proxy.unwrap_or(false)
                            && !app.gui_config.proxy_auto_config.unwrap_or(false)
                        => {
                            let host = app
                                .gui_config
                                .proxy_host
                                .clone()
                                .unwrap_or_else(|| "127.0.0.1".into());
                            match clash_verge_core::config::IClashTemp::try_read().await {
                                Ok(config) => {
                                    let core_listening = std::os::unix::net::UnixStream::connect(
                                        manager.socket_path().clone(),
                                    )
                                    .is_ok();
                                    if core_listening
                                        && let Err(error) = crate::sys_proxy::set_system_proxy(
                                            &host,
                                            config.get_mixed_port(),
                                        )
                                    {
                                        app.status_msg = Some(format!(
                                            "system proxy re-apply failed: {error}"
                                        ));
                                    }
                                }
                                Err(_) => {
                                    app.status_msg = Some(
                                        "system proxy re-apply skipped: clash config unreadable".into(),
                                    );
                                }
                            }
                        }
                    Some(Action::ServiceStatusRefresh) => {
                        let tx = action_tx.clone();
                        tokio::spawn(async move {
                            // All read-only probes — no sudo/askpass anywhere on this path.
                            let active = crate::service_cmd::service_active_state();
                            let enabled = crate::service_cmd::service_enabled_state();
                            // Installation is unit-file presence, NOT `is-enabled`
                            // (an installed-but-disabled unit must still offer
                            // uninstall; `is-enabled` alone would say 'disabled').
                            let installed = crate::service_cmd::service_installed_state();
                            let auto_launch = crate::autostart::is_enabled();
                            let _ = tx.send(Action::ServiceStatus {
                                active,
                                enabled,
                                installed,
                                auto_launch,
                            });
                        });
                    }
                    Some(Action::ServiceStatus {
                        active,
                        enabled,
                        installed,
                        auto_launch,
                    }) => {
                        app.service_active = active;
                        app.service_enabled = enabled;
                        app.service_installed = installed;
                        app.auto_launch_enabled = auto_launch;
                    }
                    Some(Action::AutoLaunchChanged { enabled }) => {
                        app.gui_config.enable_auto_launch = Some(enabled);
                        app.auto_launch_enabled = enabled;
                        app.status_msg = Some(if enabled {
                            app.tr("settings.auto_launch_on_msg").into()
                        } else {
                            app.tr("settings.auto_launch_off_msg").into()
                        });
                    }
                    Some(Action::AutoLaunchFailed(error)) => {
                        app.status_msg = Some(format!("{}: {error}", app.tr("settings.auto_launch_failed")));
                        // Re-sync the row with the real systemd state (the
                        // failed unit apply means it did not change).
                        let _ = action_tx.send(Action::ServiceStatusRefresh);
                    }
                    Some(Action::AutoUpdateFinished) => {
                        auto_update_in_flight = false;
                    }
                    None => break,
                    _ => {}
                }
            }

            _ = render_tick.tick() => {
                let mut guard = guard.lock().await;
                // Self-heal missed resize events (Orca embed) before painting.
                guard.sync_size_if_changed()?;
                if app.view != rendered_view {
                    // Orca's terminal renderer can retain differential cells across
                    // alternate-screen view changes. Force one clean repaint per route.
                    guard.reset_screen()?;
                    rendered_view = app.view;
                }
                guard.terminal_mut().draw(|f| crate::ui::draw(f, &app))?;
            }

            _ = runtime_refresh_tick.tick(), if app.core_state == CoreState::Running => {
                let _ = action_tx.send(Action::TrafficRefresh);
                match app.view {
                    View::Connections => {
                        let _ = action_tx.send(Action::ConnectionsRefresh);
                    }
                    View::Logs => {
                        let _ = action_tx.send(Action::LogsRefresh);
                    }
                    _ => {}
                }
            }

            _ = auto_update_tick.tick(), if !auto_update_in_flight => {
                auto_update_in_flight = true;
                let scheduler = auto_update_scheduler.clone();
                let tx = action_tx.clone();
                let api = manager.api();
                let enable_tun = app.gui_config.enable_tun_mode.unwrap_or(false);
                let core_running = app.core_state == CoreState::Running;
                tokio::spawn(async move {
                    let (outcome, probe) = {
                        let mut scheduler = scheduler.lock().await;
                        (
                            scheduler.tick().await,
                            scheduler.probe(&api, enable_tun, core_running).await,
                        )
                    };
                    for (uid, is_current) in outcome.updated {
                        let _ = tx.send(Action::ProfileUpdated { uid, is_current });
                    }
                    for (_uid, error) in outcome.failed {
                        let _ = tx.send(Action::ProfileUpdateFailed(error));
                    }
                    if let Some(error) = outcome.errored {
                        let _ = tx.send(Action::ProfileUpdateFailed(error));
                    }
                    if probe.forced_refresh {
                        let notice = if probe.rolled_back {
                            "probe: selected node vanished — refresh rolled back".to_string()
                        } else if probe.may_be_down {
                            "probe: subscription may be down".to_string()
                        } else {
                            "probe: node recovered — subscription refreshed".to_string()
                        };
                        let _ = tx.send(Action::ProbeNotice(notice));
                    }
                    let _ = tx.send(Action::AutoUpdateFinished);
                });
            }

            _ = profiles_refresh_tick.tick() => {
                // Re-read profiles.yaml so external interval edits (GUI/user)
                // take effect without restarting the TUI.
                if let Ok(store) = crate::profile_store::store::ProfileStore::snapshot().await {
                    app.profiles = store.items();
                }
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn proxy_group(all: Option<Vec<String>>) -> crate::mihomo_api::types::ProxyGroup {
        crate::mihomo_api::types::ProxyGroup {
            group_type: "Selector".to_string(),
            now: None,
            all,
            history: None,
        }
    }

    #[test]
    fn batch_delay_targets_deduplicates_leaf_nodes_in_stable_order() {
        let mut groups = HashMap::new();
        groups.insert(
            "first".to_string(),
            proxy_group(Some(vec!["Tokyo".to_string(), "Singapore".to_string()])),
        );
        groups.insert(
            "second".to_string(),
            proxy_group(Some(vec!["Tokyo".to_string(), "Los Angeles".to_string()])),
        );

        assert_eq!(batch_delay_targets(&groups), vec!["Los Angeles", "Singapore", "Tokyo"]);
    }

    #[test]
    fn batch_delay_targets_excludes_pseudo_nodes_and_nested_group_names() {
        let mut groups = HashMap::new();
        // "nested" is itself a group key, so it must never be a target.
        groups.insert("nested".to_string(), proxy_group(Some(vec!["Tokyo".to_string()])));
        groups.insert(
            "root".to_string(),
            proxy_group(Some(vec![
                "Tokyo".to_string(),
                "nested".to_string(),
                "DIRECT".to_string(),
                "REJECT".to_string(),
                "REJECT-DROP".to_string(),
                "PASS".to_string(),
                "COMPATIBLE".to_string(),
            ])),
        );
        // A leaf node that is also a key with `all: None` stays testable.
        groups.insert("Tokyo".to_string(), proxy_group(None));

        assert_eq!(batch_delay_targets(&groups), vec!["Tokyo"]);
    }

    #[test]
    fn batch_delay_targets_is_empty_when_nothing_is_testable() {
        let mut groups = HashMap::new();
        groups.insert("DIRECT".to_string(), proxy_group(Some(Vec::new())));
        groups.insert("only-group".to_string(), proxy_group(Some(vec!["DIRECT".to_string()])));

        assert!(batch_delay_targets(&groups).is_empty());
    }

    #[test]
    fn begin_batch_delay_rejects_duplicate_starts_and_reports_progress() {
        let mut app = App::new();
        app.batch_delay = Some((2, 7));

        match begin_batch_delay(&mut app) {
            BatchDelayOutcome::InProgress { done, total } => {
                assert_eq!((done, total), (2, 7));
            }
            outcome => panic!("expected InProgress, got {outcome:?}"),
        }
        assert_eq!(app.batch_delay, Some((2, 7)), "in-flight state must stay untouched");
    }

    #[test]
    fn begin_batch_delay_reports_no_targets_without_creating_a_task() {
        let mut app = App::new();

        assert_eq!(begin_batch_delay(&mut app), BatchDelayOutcome::NoTargets);
        assert_eq!(app.batch_delay, None);
    }

    #[test]
    fn begin_batch_delay_starts_a_batch_with_filtered_targets() {
        let mut app = App::new();
        app.proxy_groups.insert(
            "root".to_string(),
            proxy_group(Some(vec!["DIRECT".to_string(), "Tokyo".to_string()])),
        );
        app.proxy_groups.insert("Tokyo".to_string(), proxy_group(None));

        match begin_batch_delay(&mut app) {
            BatchDelayOutcome::Started { targets } => {
                assert_eq!(targets, vec!["Tokyo"]);
            }
            outcome => panic!("expected Started, got {outcome:?}"),
        }
        assert_eq!(app.batch_delay, Some((0, 1)));
    }

    #[test]
    fn advance_batch_counts_results_and_clears_on_completion() {
        let mut app = App::new();
        app.batch_delay = Some((0, 3));

        advance_batch(&mut app);
        assert_eq!(app.batch_delay, Some((1, 3)));

        advance_batch(&mut app);
        assert_eq!(app.batch_delay, Some((2, 3)));

        advance_batch(&mut app);
        assert_eq!(app.batch_delay, None, "last result clears the in-progress marker");
    }

    #[test]
    fn single_node_result_during_batch_does_not_advance_or_clear_the_guard() {
        let mut app = App::new();
        app.batch_delay = Some((1, 5));
        app.proxy_groups
            .insert("root".to_string(), proxy_group(Some(vec!["Tokyo".to_string()])));
        app.proxy_groups.insert("Tokyo".to_string(), proxy_group(None));

        // A single-node `t` result lands while the batch is still running.
        note_delay_result(&mut app, "Tokyo".to_string(), Some(42));
        assert_eq!(
            app.delay_map.get("Tokyo"),
            Some(&Some(42)),
            "single-node result still renders"
        );
        assert_eq!(
            app.batch_delay,
            Some((1, 5)),
            "single-node result must not advance batch progress"
        );
        assert_eq!(
            begin_batch_delay(&mut app),
            BatchDelayOutcome::InProgress { done: 1, total: 5 },
            "the batch guard must stay armed so a second batch cannot start early"
        );

        // The batch's own result still advances progress.
        note_batch_delay_result(&mut app, "Tokyo".to_string(), Some(43));
        assert_eq!(app.batch_delay, Some((2, 5)));
    }

    #[test]
    fn single_node_failure_during_batch_does_not_advance_or_clear_the_guard() {
        let mut app = App::new();
        app.batch_delay = Some((3, 4));

        note_delay_failed(&mut app, "Tokyo".to_string(), "timeout".to_string());
        assert_eq!(
            app.delay_map.get("Tokyo"),
            Some(&None),
            "failure state still renders as failed"
        );
        assert_eq!(
            app.batch_delay,
            Some((3, 4)),
            "single-node failure must not advance batch progress"
        );

        // The batch's own failure completes the batch and clears the guard.
        note_batch_delay_failed(&mut app, "Singapore".to_string(), "timeout".to_string());
        assert_eq!(app.batch_delay, None, "batch failure on the last node clears the guard");
    }

    #[test]
    fn close_connection_confirmation_stays_bound_to_the_selected_target() {
        let mut app = App::new();
        app.selected_connection_id = Some("connection-a".to_string());

        begin_connection_close(&mut app);

        assert_eq!(app.pending_connection_close.as_deref(), Some("connection-a"));
        assert_eq!(app.overlay, Some(Overlay::CloseConfirmation));
        assert!(close_confirmation_is_current(&app, "connection-a"));
        assert!(!close_confirmation_is_current(&app, "connection-b"));

        app.selected_connection_id = Some("connection-b".to_string());
        assert!(!close_confirmation_is_current(&app, "connection-a"));
    }

    #[test]
    fn ndjson_parser_buffers_partial_records_and_preserves_order() {
        let mut buffer = Vec::new();

        let initial = match drain_ndjson::<TrafficData>(&mut buffer, br#"{"up":1,"down":2}"#) {
            Ok(records) => records,
            Err(error) => panic!("partial record: {error}"),
        };
        assert!(initial.is_empty());

        let entries = match drain_ndjson::<TrafficData>(&mut buffer, b"\n{\"up\":3,\"down\":4}\n") {
            Ok(records) => records,
            Err(error) => panic!("complete records: {error}"),
        };
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].up, 1);
        assert_eq!(entries[1].down, 4);
    }

    #[test]
    fn ndjson_parser_ignores_blank_lines() {
        let mut buffer = Vec::new();
        let entries = match drain_ndjson::<LogEntry>(&mut buffer, b"\n {\"type\":\"info\",\"payload\":\"ready\"}\n\n") {
            Ok(records) => records,
            Err(error) => panic!("valid log record: {error}"),
        };

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].level, "info");
        assert_eq!(entries[0].payload, "ready");
    }

    #[test]
    fn duplicate_password_submit_without_pending_is_ignored() {
        // Regression: a stale second Enter after the popup closed used to hit
        // `break` and exit the whole TUI loop. Now it must return without
        // spawning anything (no tokio runtime here) and leave state intact.
        let mut app = App::new();
        app.overlay = Some(Overlay::PasswordInput); // stale overlay from a closed popup
        app.password_buffer = vec!['x'];
        app.pending_sudo = None;

        let (tx, _rx) = mpsc::unbounded_channel::<Action>();
        let manager = crate::mihomo_manager::manager::MihomoManager::new(std::path::PathBuf::from("/tmp/fake"));
        handle_password_submit(&mut app, &tx, &manager);

        assert!(app.pending_sudo.is_none(), "nothing may be created by a stale submit");
        assert_eq!(
            app.overlay,
            Some(Overlay::PasswordInput),
            "stale overlay is left untouched"
        );
    }

    #[test]
    fn tun_start_offers_setup_when_capability_or_dns_rule_is_missing() {
        // Ready: capable + non-root + rule present → no prompt.
        assert!(!tun_start_offers_setup(true, false, false));
        // Root bypasses the capability gate entirely (rule_needed is already
        // false for root) → no prompt.
        assert!(!tun_start_offers_setup(false, true, false));
        // Missing file capability (non-root) → prompt.
        assert!(tun_start_offers_setup(false, false, false));
        // Capability present but DNS polkit rule missing → prompt.
        assert!(tun_start_offers_setup(true, false, true));
    }

    #[test]
    fn tun_setup_prompt_opens_confirm_with_resume_context() {
        // Preflight found a missing capability/rule → the TUI-native confirm
        // dialog opens carrying the resolved binary, the resume settings,
        // and the gate that fired.
        let mut app = App::new();
        begin_tun_setup_confirm(
            &mut app,
            std::path::PathBuf::from("/fake/mihomo"),
            true,
            TunSetupReason::MissingCapability,
        );

        assert_eq!(app.overlay, Some(Overlay::TunSetupConfirmation));
        let PendingSudoAction::TunSetup {
            binary,
            resume_start,
            reason,
        } = app.pending_sudo.as_ref().expect("pending setup must be set")
        else {
            panic!("pending action must be a TunSetup");
        };
        assert_eq!(
            *resume_start,
            Some(true),
            "confirm must carry enable_tun for the resume"
        );
        assert_eq!(
            *reason,
            TunSetupReason::MissingCapability,
            "confirm must carry the gate that fired"
        );
        assert_eq!(*binary, std::path::PathBuf::from("/fake/mihomo"));
        assert_eq!(
            app.status_msg.as_deref(),
            Some(app.tun_setup_confirm_hint()),
            "status hint must match the hard-gate choice text"
        );
    }

    #[test]
    fn confirm_tun_setup_opens_the_password_popup_keeping_resume_context() {
        // `y` on the confirm reuses the existing password popup; the resume
        // context stays in pending_tun so the submit can resume the start.
        let mut app = App::new();
        begin_tun_setup_confirm(
            &mut app,
            std::path::PathBuf::from("/fake/mihomo"),
            true,
            TunSetupReason::MissingDnsRule,
        );
        confirm_tun_setup(&mut app);

        assert_eq!(app.overlay, Some(Overlay::PasswordInput));
        assert!(app.password_prompt.is_some(), "prompt label must be set");
        assert!(
            matches!(
                app.pending_sudo.as_ref(),
                Some(PendingSudoAction::TunSetup {
                    resume_start: Some(true),
                    ..
                })
            ),
            "password popup must keep the pending resume"
        );
    }

    #[test]
    fn tun_setup_success_with_resume_requests_the_pending_start() {
        // Success after `s` → y → password: the start must resume. The resume
        // request is observed on the action channel (no tokio needed); the
        // explicit Settings flow (None) never emits one.
        let mut app = App::new();
        let (tx, mut rx) = mpsc::unbounded_channel::<Action>();
        note_tun_setup_succeeded(&mut app, Some(true), &tx);

        assert!(app.tun_privileged, "setup success must mark the TUI privileged");
        match rx.try_recv() {
            Ok(Action::ResumeCoreStart { enable_tun }) => assert!(enable_tun),
            other => panic!("expected ResumeCoreStart, got {other:?}"),
        }

        let mut app = App::new();
        let (tx, mut rx) = mpsc::unbounded_channel::<Action>();
        note_tun_setup_succeeded(&mut app, None, &tx);
        assert!(rx.try_recv().is_err(), "Settings flow must not resume any start");
    }

    #[test]
    fn skip_tun_setup_start_dismisses_and_resumes_the_start() {
        // `n`/Esc/q on the confirm when only the DNS rule is missing (soft
        // gate, capability present): dismiss and start anyway, preserving
        // the current behavior (no setup transaction runs).
        let mut app = App::new();
        app.core_state = CoreState::Starting;
        begin_tun_setup_confirm(
            &mut app,
            std::path::PathBuf::from("/fake/mihomo"),
            true,
            TunSetupReason::MissingDnsRule,
        );
        let (tx, mut rx) = mpsc::unbounded_channel::<Action>();
        skip_tun_setup_start(&mut app, &tx);

        assert_eq!(app.overlay, None, "skip must dismiss the confirm dialog");
        assert!(app.pending_sudo.is_none(), "skip must drop the pending setup");
        match rx.try_recv() {
            Ok(Action::ResumeCoreStart { enable_tun }) => assert!(enable_tun),
            other => panic!("expected ResumeCoreStart, got {other:?}"),
        }
    }

    #[test]
    fn skip_tun_setup_start_when_capability_missing_cancels_the_start() {
        // `n`/Esc/q when the prompt fired because the binary lacks the TUN
        // capability (hard gate): starting anyway would only hit the spawn
        // preflight hard-fail a moment later. The skip must CANCEL the
        // start, reset the transient Starting state, and point at TUN setup.
        let mut app = App::new();
        app.core_state = CoreState::Starting;
        begin_tun_setup_confirm(
            &mut app,
            std::path::PathBuf::from("/fake/mihomo"),
            true,
            TunSetupReason::MissingCapability,
        );
        let (tx, mut rx) = mpsc::unbounded_channel::<Action>();
        skip_tun_setup_start(&mut app, &tx);

        assert_eq!(app.overlay, None, "skip must dismiss the confirm dialog");
        assert!(app.pending_sudo.is_none(), "skip must drop the pending setup");
        assert!(
            rx.try_recv().is_err(),
            "capability-missing skip must NOT resume the start"
        );
        assert_eq!(
            app.core_state,
            CoreState::Stopped,
            "cancelled start must not stay Starting"
        );
        let message = app.status_msg.as_deref().expect("status message set");
        assert!(
            message.contains(crate::commands::privilege::TUN_SETUP_COMMAND),
            "cancel message must point at the TUN setup command: {message}"
        );
    }

    #[test]
    fn tun_setup_confirm_hint_reflects_the_gate_that_fired() {
        // The prompt text must reflect the two cases: the hard gate cancels
        // on n/Esc/q, the soft gate starts without setup.
        let mut hard = App::new();
        begin_tun_setup_confirm(
            &mut hard,
            std::path::PathBuf::from("/fake/mihomo"),
            true,
            TunSetupReason::MissingCapability,
        );
        let mut soft = App::new();
        begin_tun_setup_confirm(
            &mut soft,
            std::path::PathBuf::from("/fake/mihomo"),
            true,
            TunSetupReason::MissingDnsRule,
        );

        let hard_hint = hard.tun_setup_confirm_hint();
        let soft_hint = soft.tun_setup_confirm_hint();
        assert_ne!(hard_hint, soft_hint, "the two cases must read differently");
        assert!(hard_hint.contains("cancel"), "hard hint must say cancel: {hard_hint}");
        assert!(
            soft_hint.contains("start without setup"),
            "soft hint must offer start: {soft_hint}"
        );
    }

    #[test]
    fn password_cancel_with_pending_start_leaves_no_stale_state() {
        // Esc on the password popup after `s` → y: the setup was abandoned, so
        // the pending start must not resume and the transient Starting state
        // is reset — no stale flag can fire a resume later.
        let mut app = App::new();
        app.core_state = CoreState::Starting;
        app.pending_sudo = Some(PendingSudoAction::TunSetup {
            binary: std::path::PathBuf::from("/fake/mihomo"),
            resume_start: Some(true),
            reason: TunSetupReason::MissingCapability,
        });
        app.overlay = Some(Overlay::PasswordInput);
        app.password_buffer = vec!['x'];

        handle_password_cancel(&mut app);

        assert!(app.pending_sudo.is_none());
        assert_eq!(app.overlay, None);
        assert!(app.password_buffer.is_empty());
        assert_eq!(
            app.core_state,
            CoreState::Stopped,
            "cancelled start setup must not stay Starting"
        );
        assert!(app.status_msg.is_some());
    }

    #[test]
    fn password_cancel_from_settings_keeps_plain_cancel_message() {
        // The explicit Settings flow has nothing to resume: cancel just closes
        // the popup with the plain message and no state reset.
        let mut app = App::new();
        app.core_state = CoreState::Stopped;
        app.pending_sudo = Some(PendingSudoAction::TunSetup {
            binary: std::path::PathBuf::from("/fake/mihomo"),
            resume_start: None,
            reason: TunSetupReason::MissingCapability,
        });
        app.overlay = Some(Overlay::PasswordInput);

        handle_password_cancel(&mut app);

        assert!(app.pending_sudo.is_none());
        assert_eq!(app.overlay, None);
        assert_eq!(app.status_msg.as_deref(), Some("TUN setup cancelled"));
    }

    #[test]
    fn service_row_not_installed_opens_password_popup_with_install_pending() {
        // Enter on 'System service' when the probes report no service: the
        // existing password popup opens with a ServiceInstall pending action
        // (no separate confirm — installing is the only next step).
        let mut app = App::new();
        app.service_installed = false;
        app.service_active = "inactive".into();
        app.service_enabled = "disabled".into();
        app.auto_launch_enabled = false;
        assert!(!service_installed(&app));

        begin_service_action(&mut app);

        assert_eq!(app.overlay, Some(Overlay::PasswordInput));
        assert!(app.password_prompt.is_some(), "install prompt label must be set");
        match &app.pending_sudo {
            Some(PendingSudoAction::ServiceInstall {
                binary_path,
                config_dir,
            }) => {
                assert!(!binary_path.is_empty(), "ExecStart binary must be carried");
                assert!(!config_dir.is_empty(), "--config-dir must be carried");
            }
            other => panic!("expected ServiceInstall pending, got {other:?}"),
        }
    }

    #[test]
    fn service_install_is_rejected_while_autostart_is_enabled() {
        // P1b: the system service and login autostart are mutually exclusive
        // lifecycle mechanisms. Installing the service while autostart is on
        // must be rejected with a clear status pointing at the row to disable
        // first — no install popup, no pending action.
        let mut app = App::new();
        app.service_installed = false;
        app.service_active = "inactive".into();
        app.service_enabled = "disabled".into();
        app.auto_launch_enabled = true;

        begin_service_action(&mut app);

        assert_eq!(app.overlay, None, "no install popup may open");
        assert!(app.pending_sudo.is_none(), "no pending install may be created");
        let message = app.status_msg.as_deref().expect("status message set");
        assert!(
            message.contains("Launch at login"),
            "rejection must point at the autostart row: {message}"
        );
    }

    #[test]
    fn service_row_installed_opens_the_uninstall_confirm() {
        // Enter on 'System service' when a unit is installed (enabled or
        // active): an explicit confirm overlay opens; the password popup
        // must NOT open until `y`.
        let mut app = App::new();
        app.service_installed = true;
        app.service_active = "active".into();
        app.service_enabled = "enabled".into();
        assert!(service_installed(&app));

        begin_service_action(&mut app);

        assert_eq!(app.overlay, Some(Overlay::ServiceUninstallConfirmation));
        assert!(app.pending_sudo.is_none(), "no password flow before confirm");
    }

    #[test]
    fn service_row_installed_by_enabled_alone_also_confirms() {
        // Installed-but-stopped (enabled, inactive) still routes to the
        // uninstall confirm, not the install popup.
        let mut app = App::new();
        app.service_installed = true;
        app.service_active = "inactive".into();
        app.service_enabled = "enabled".into();
        assert!(service_installed(&app));

        begin_service_action(&mut app);

        assert_eq!(app.overlay, Some(Overlay::ServiceUninstallConfirmation));
    }

    #[test]
    fn service_row_installed_but_disabled_still_confirms_uninstall() {
        // P2b regression: an installed-but-disabled/inactive unit reports
        // `is-enabled` == 'disabled'. The installation probe (unit file
        // presence) must still classify it as installed so uninstall stays
        // offered — never the install popup.
        let mut app = App::new();
        app.service_installed = true;
        app.service_active = "inactive".into();
        app.service_enabled = "disabled".into();
        assert!(service_installed(&app));

        begin_service_action(&mut app);

        assert_eq!(app.overlay, Some(Overlay::ServiceUninstallConfirmation));
        assert!(app.pending_sudo.is_none(), "no password flow before confirm");
    }

    #[test]
    fn confirm_service_uninstall_opens_password_popup_with_uninstall_pending() {
        // `y` on the confirm reuses the existing password popup; the pending
        // action switches to ServiceUninstall so the submit runs the
        // uninstall transaction.
        let mut app = App::new();
        app.overlay = Some(Overlay::ServiceUninstallConfirmation);

        confirm_service_uninstall(&mut app);

        assert_eq!(app.overlay, Some(Overlay::PasswordInput));
        assert_eq!(app.pending_sudo, Some(PendingSudoAction::ServiceUninstall));
        assert!(app.password_prompt.is_some(), "uninstall prompt label must be set");
    }

    #[test]
    fn every_overlay_dismiss_action_is_forwarded_to_the_channel() {
        // Regression for the stuck service-uninstall dialog: the key path
        // used to discard Confirm/CancelServiceUninstall via its `_ => {}`
        // arm, so no key could ever close the overlay. Every overlay-
        // confirm/cancel action that input::map_key can produce must be
        // forwarded to the action channel where its handler lives.
        for action in [
            Action::ConfirmServiceUninstall,
            Action::CancelServiceUninstall,
            Action::ConfirmTunSetup,
            Action::SkipTunSetupStart,
            Action::ConfirmTrustImport,
            Action::CancelTrustImport,
            Action::PasswordSubmit,
            Action::PasswordCancel,
        ] {
            assert!(forward_to_channel(&action), "{action:?} must be forwarded");
        }
        // Inline-handled key actions must not be forwarded.
        assert!(!forward_to_channel(&Action::Quit));
        assert!(!forward_to_channel(&Action::StartCore));
        // DismissOverlay stays inline: the channel match has no arm for it.
        assert!(!forward_to_channel(&Action::DismissOverlay));
    }

    #[test]
    fn cancel_service_uninstall_closes_the_confirm_overlay() {
        // `n`/Esc/q on the confirm: overlay closes, no transaction runs.
        let mut app = App::new();
        app.overlay = Some(Overlay::ServiceUninstallConfirmation);

        cancel_service_uninstall(&mut app);

        assert_eq!(app.overlay, None);
        assert!(app.pending_sudo.is_none());
        assert!(app.status_msg.is_some(), "cancel must report in the status bar");
    }

    #[test]
    fn service_password_cancel_reports_a_service_cancel() {
        // Esc inside the password popup for a service action: the message
        // names the service, not TUN.
        let mut app = App::new();
        app.pending_sudo = Some(PendingSudoAction::ServiceUninstall);
        app.overlay = Some(Overlay::PasswordInput);
        app.password_buffer = vec!['x'];

        handle_password_cancel(&mut app);

        assert!(app.pending_sudo.is_none());
        assert_eq!(app.overlay, None);
        assert!(app.password_buffer.is_empty());
        let message = app.status_msg.as_deref().expect("status message set");
        assert!(
            message.contains("cancel"),
            "service cancel message must mention cancel: {message}"
        );
    }

    #[tokio::test]
    async fn service_install_handoff_stops_the_core_before_installing() {
        // P1a ordering: when a TUI-owned core is running, the handoff stop
        // must complete BEFORE the install transaction runs — a second mihomo
        // on the same config/socket/ports must never be spawned.
        let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let stop_events = events.clone();
        let install_events = events.clone();
        let result = handoff_then_install_service(
            true,
            async move {
                stop_events.lock().expect("lock").push("stop");
                Ok(())
            },
            async move {
                install_events.lock().expect("lock").push("install");
                Ok(())
            },
        )
        .await;

        assert!(result.is_ok(), "handoff + install must succeed");
        let events = events.lock().expect("lock");
        assert_eq!(
            *events,
            vec!["stop", "install"],
            "core stop must strictly precede the install transaction"
        );
    }

    #[tokio::test]
    async fn service_install_handoff_skips_stop_when_no_core_is_running() {
        // No running TUI core → no handoff stop; only the install runs.
        let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let install_events = events.clone();
        let result = handoff_then_install_service(
            false,
            async {
                panic!("stop must not run without a running core");
                #[allow(unreachable_code)]
                Ok(())
            },
            async move {
                install_events.lock().expect("lock").push("install");
                Ok(())
            },
        )
        .await;

        assert!(result.is_ok(), "install without a core must succeed");
        assert_eq!(*events.lock().expect("lock"), vec!["install"]);
    }

    #[tokio::test]
    async fn service_install_handoff_aborts_install_when_stop_fails() {
        // A failed handoff stop must abort the install: the core is still up,
        // so installing would conflict on the same socket/ports.
        let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let stop_events = events.clone();
        let result = handoff_then_install_service(
            true,
            async move {
                stop_events.lock().expect("lock").push("stop");
                Err(anyhow::anyhow!("stop failed"))
            },
            async {
                panic!("install must not run after a failed handoff stop");
                #[allow(unreachable_code)]
                Ok(())
            },
        )
        .await;

        assert_eq!(
            result,
            Err(ServiceInstallError::HandoffStop("stop failed".to_string())),
            "stop failure must be classified and surfaced"
        );
        assert_eq!(
            *events.lock().expect("lock"),
            vec!["stop"],
            "install must never run after a failed handoff"
        );
    }

    #[tokio::test]
    async fn service_install_handoff_transaction_failure_is_classified() {
        // After a successful handoff, a failing install transaction surfaces
        // as Transaction (the caller records that the core was stopped).
        let result = handoff_then_install_service(true, async { Ok(()) }, async {
            Err(std::io::Error::other("systemctl start failed"))
        })
        .await;
        assert_eq!(
            result,
            Err(ServiceInstallError::Transaction("systemctl start failed".to_string()))
        );
    }

    #[test]
    fn autostart_enable_is_rejected_while_service_is_installed_and_enabled() {
        // P1b: enabling 'Launch at login' while the system service is
        // installed+enabled must be rejected — both enabled would collide at
        // the next boot/login.
        let mut app = App::new();
        app.auto_launch_enabled = false;
        app.service_enabled = "enabled".into();
        app.service_installed = true;
        assert_eq!(autostart_toggle_decision(&app), AutostartToggle::BlockedByService);

        // A disabled (or absent) service does not block enabling.
        app.service_enabled = "disabled".into();
        app.service_installed = false;
        assert_eq!(
            autostart_toggle_decision(&app),
            AutostartToggle::Proceed { enabled: true }
        );

        // Disabling autostart is never blocked.
        app.auto_launch_enabled = true;
        app.service_enabled = "enabled".into();
        app.service_installed = true;
        assert_eq!(
            autostart_toggle_decision(&app),
            AutostartToggle::Proceed { enabled: false }
        );
    }

    #[test]
    fn autostart_toggle_blocked_by_service_sets_a_clear_status() {
        // The blocked decision renders a status message pointing at the
        // service row, and the row is NOT flipped.
        let mut app = App::new();
        app.auto_launch_enabled = false;
        app.service_enabled = "enabled".into();
        app.service_installed = true;
        assert!(matches!(
            autostart_toggle_decision(&app),
            AutostartToggle::BlockedByService
        ));

        let message = app.tr("settings.autostart_conflicts_service");
        assert!(
            message.contains("system service"),
            "status must point at the service row: {message}"
        );
    }

    #[test]
    fn menu_navigation_into_settings_refreshes_service_probes() {
        // P2a: entering Settings by menu j/k (MoveNext/MovePrevious — which
        // change app.view directly, NOT via SwitchView) must still trigger the
        // lifecycle probe refresh, or the rows go stale (install offered on an
        // installed service, etc.).
        let mut app = App::new();
        app.focus = Focus::Menu;
        app.view = View::Unlock; // Unlock is the view right before Settings.
        let (tx, mut rx) = mpsc::unbounded_channel::<Action>();

        menu_move(&mut app, &tx, true);

        assert_eq!(app.view, View::Settings, "j must move into Settings");
        match rx.try_recv() {
            Ok(Action::ServiceStatusRefresh) => {}
            other => panic!("expected ServiceStatusRefresh, got {other:?}"),
        }

        // Backwards menu navigation into Settings fires the same probe.
        let mut app = App::new();
        app.focus = Focus::Menu;
        app.view = View::Home; // Home wraps around to Settings on k.
        let (tx, mut rx) = mpsc::unbounded_channel::<Action>();
        menu_move(&mut app, &tx, false);
        assert_eq!(app.view, View::Settings, "k must wrap into Settings");
        match rx.try_recv() {
            Ok(Action::ServiceStatusRefresh) => {}
            other => panic!("expected ServiceStatusRefresh, got {other:?}"),
        }
    }

    #[test]
    fn switch_view_into_settings_still_refreshes_service_probes() {
        // The number-key path (SwitchView) keeps the probe refresh: both
        // entry paths now share transition_to_view.
        let mut app = App::new();
        let (tx, mut rx) = mpsc::unbounded_channel::<Action>();
        transition_to_view(&mut app, &tx, View::Settings);
        assert_eq!(app.view, View::Settings);
        match rx.try_recv() {
            Ok(Action::ServiceStatusRefresh) => {}
            other => panic!("expected ServiceStatusRefresh, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn autostart_toggle_success_persists_flag_and_applies_unit() {
        use crate::profile_store::store::tests::{claim_test_app_home, test_app_home_root};

        let root = test_app_home_root();
        let _dir_guard = claim_test_app_home(root.clone()).await;

        let calls = std::sync::Mutex::new(Vec::new());
        let result = toggle_autostart(
            true,
            "/usr/bin/clash-verge-cli",
            "/home/u/.config/clash-verge-cli",
            &|enabled, binary, config| {
                calls
                    .lock()
                    .expect("lock calls")
                    .push((enabled, binary.to_string(), config.to_string()));
                Ok(())
            },
        )
        .await;

        assert!(result.is_ok(), "successful unit apply must succeed");
        let recorded = calls.into_inner().expect("unlock calls");
        assert_eq!(
            recorded,
            vec![(
                true,
                "/usr/bin/clash-verge-cli".to_string(),
                "/home/u/.config/clash-verge-cli".to_string()
            )],
            "the unit seam must receive the exact toggle and paths"
        );
        let persisted = clash_verge_core::config::IVerge::new().await;
        assert_eq!(persisted.enable_auto_launch, Some(true), "flag must persist");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn autostart_toggle_failure_surfaces_error_and_rolls_back_flag() {
        use crate::profile_store::store::tests::{claim_test_app_home, test_app_home_root};

        let root = test_app_home_root();
        let _dir_guard = claim_test_app_home(root.clone()).await;

        // Seed a prior state so the rollback is observable.
        let mut seeded = clash_verge_core::config::IVerge::new().await;
        seeded.enable_auto_launch = Some(true);
        seeded.save_file().await.expect("seed verge.yaml");

        let result = toggle_autostart(
            false,
            "/usr/bin/clash-verge-cli",
            "/home/u/.config/clash-verge-cli",
            &|_enabled, _binary, _config| Err("Failed to connect to bus: No such file or directory".to_string()),
        )
        .await;

        assert_eq!(
            result,
            Err("Failed to connect to bus: No such file or directory".to_string()),
            "systemctl failure must surface verbatim (headless/no-session)"
        );
        let persisted = clash_verge_core::config::IVerge::new().await;
        assert_eq!(
            persisted.enable_auto_launch,
            Some(true),
            "failed unit apply must roll back the persisted flag"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn ssrf_blocked_host_detects_private_host_and_ignores_unrelated_failures() {
        // A literal private IP needs no DNS: the default allowlist blocks it,
        // and trusting the host makes the check pass — genuine SSRF block.
        assert_eq!(
            ssrf_blocked_host("http://192.168.1.1/sub"),
            Some("192.168.1.1".to_string())
        );
        assert_eq!(
            ssrf_blocked_host("http://127.0.0.1:8080/x"),
            Some("127.0.0.1".to_string())
        );

        // A literal public IP passes the default check: no trust needed.
        assert_eq!(ssrf_blocked_host("http://1.1.1.1/sub"), None);

        // Malformed URLs are NOT trust prompts — they surface as import errors.
        assert_eq!(ssrf_blocked_host("not a url"), None);
        assert_eq!(ssrf_blocked_host(""), None);
    }

    #[test]
    fn ssrf_blocked_host_dns_failure_is_not_trust_offered() {
        // `.invalid` (RFC 6761) never resolves. A DNS/no-address failure must
        // NOT produce a trust prompt: trusting could never unblock it, so
        // offering (and persisting) trust would be wrong.
        assert_eq!(ssrf_blocked_host("http://host.invalid/sub"), None);
    }

    #[test]
    fn ssrf_blocked_host_ula_v6_is_trust_offered() {
        // A literal IPv6 unique-local address resolves without DNS and is a
        // genuine block: the trust prompt must still fire for it.
        assert!(ssrf_blocked_host("http://[fd00::1]/sub").is_some());
    }

    #[test]
    fn cancel_trust_leaves_no_trust_state_and_no_overlay() {
        let mut app = App::new();
        app.pending_trust = Some(TrustPending {
            url: "http://192.168.1.1/sub".to_string(),
            host: "192.168.1.1".to_string(),
            uid: None,
        });
        app.overlay = Some(Overlay::TrustConfirmation);

        handle_cancel_trust(&mut app);

        assert!(app.pending_trust.is_none(), "cancel must drop the pending trust");
        assert_eq!(app.overlay, None, "cancel must close the trust overlay");
        // Nothing was written: cancelling only mutated in-memory state, so no
        // exception reached profiles.yaml and the host stays blocked.
    }

    #[test]
    fn dismiss_overlay_also_drops_a_pending_trust() {
        // `q` on the trust prompt routes through the generic DismissOverlay
        // fallback; it must cancel exactly like `n`/Esc (no trust saved).
        let mut app = App::new();
        app.pending_trust = Some(TrustPending {
            url: "http://192.168.1.1/sub".to_string(),
            host: "192.168.1.1".to_string(),
            uid: None,
        });
        app.overlay = Some(Overlay::TrustConfirmation);

        dismiss_overlay(&mut app);

        assert!(app.pending_trust.is_none());
        assert_eq!(app.overlay, None);
    }

    #[test]
    fn stale_trust_confirm_without_pending_is_ignored() {
        // A duplicate `y` after the prompt already closed must not spawn a
        // retry import. No tokio runtime here, so a spawn would panic — the
        // test passing proves the early return.
        let mut app = App::new();
        app.overlay = Some(Overlay::TrustConfirmation); // stale overlay
        app.pending_trust = None;

        let (tx, _rx) = mpsc::unbounded_channel::<Action>();
        handle_confirm_trust(&mut app, &tx);

        assert!(app.pending_trust.is_none());
        assert_eq!(app.overlay, Some(Overlay::TrustConfirmation));
    }

    #[test]
    fn update_flow_decision_prompts_only_for_a_genuinely_blocked_refresh() {
        use clash_verge_core::config::PrfOption;

        // The live-case shape: a remote profile whose URL host has no
        // trusted_hosts and resolves to a private address → the refresh must be
        // routed to the trust prompt, carrying the profile uid. A literal
        // private IP keeps the test DNS-free and deterministic.
        let blocked = clash_verge_core::config::PrfItem {
            uid: Some("R7iHvBBicAOz".into()),
            itype: Some("remote".into()),
            url: Some("http://192.168.1.1/sub".into()),
            option: None,
            ..Default::default()
        };
        assert_eq!(
            update_flow_decision(&blocked),
            Some(("R7iHvBBicAOz".to_string(), "192.168.1.1".to_string()))
        );

        // A public host passes the SSRF check: no prompt, plain update.
        let public = clash_verge_core::config::PrfItem {
            url: Some("http://1.1.1.1/sub".into()),
            ..blocked.clone()
        };
        assert_eq!(update_flow_decision(&public), None);

        // A host the profile already trusts never prompts again: the stored
        // allowlist covers it, so the refresh would succeed on its own.
        let already_trusted = clash_verge_core::config::PrfItem {
            option: Some(PrfOption {
                trusted_hosts: Some(vec!["192.168.1.1".into()]),
                ..Default::default()
            }),
            ..blocked.clone()
        };
        assert_eq!(update_flow_decision(&already_trusted), None);

        // Profiles without a URL are not trust prompts — they surface as
        // ordinary update errors (same as before).
        let no_url = clash_verge_core::config::PrfItem {
            url: None,
            ..blocked.clone()
        };
        assert_eq!(update_flow_decision(&no_url), None);
    }

    #[test]
    fn update_flow_decision_ignores_dns_failures() {
        // A host that cannot resolve must NOT open a trust prompt: trusting
        // could never unblock it. `.invalid` (RFC 6761) never resolves.
        let dns_failure = clash_verge_core::config::PrfItem {
            uid: Some("Rdn".into()),
            itype: Some("remote".into()),
            url: Some("http://host.invalid/sub".into()),
            ..Default::default()
        };
        assert_eq!(update_flow_decision(&dns_failure), None);
    }

    #[test]
    fn begin_update_trust_carries_the_profile_uid_and_opens_the_overlay() {
        let mut app = App::new();
        begin_update_trust(&mut app, "R7iHvBBicAOz".into(), "8ry1xfih.doggygosubs.com".into());

        assert_eq!(app.overlay, Some(Overlay::TrustConfirmation));
        let pending = app.pending_trust.as_ref().expect("pending trust must be set");
        assert_eq!(pending.uid.as_deref(), Some("R7iHvBBicAOz"));
        assert_eq!(pending.host, "8ry1xfih.doggygosubs.com");
    }

    #[test]
    fn cancel_update_trust_persists_nothing() {
        // `n`/Esc on the update prompt must leave the stored option untouched:
        // the pending state and overlay close, the update keeps its failure.
        let mut app = App::new();
        app.pending_trust = Some(TrustPending {
            url: "http://192.168.1.1/sub".to_string(),
            host: "192.168.1.1".to_string(),
            uid: Some("R7iHvBBicAOz".to_string()),
        });
        app.overlay = Some(Overlay::TrustConfirmation);

        handle_cancel_trust(&mut app);

        assert!(app.pending_trust.is_none(), "cancel must drop the pending trust");
        assert_eq!(app.overlay, None, "cancel must close the trust overlay");
        // Cancel only mutates in-memory state: no trusted_hosts entry is ever
        // written to profiles.yaml for the profile.
    }

    #[tokio::test]
    async fn confirm_trust_update_persists_trust_and_retries_end_to_end() {
        use tokio::io::AsyncWriteExt;

        let root = crate::profile_store::store::tests::test_app_home_root();
        let _dir_guard = crate::profile_store::store::tests::claim_test_app_home(root.clone()).await;

        // Serve a valid subscription body on loopback so the trusted retry can
        // complete without any external network. The URL host is loopback → the
        // SSRF check blocks it until the host is trusted.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback listener");
        let addr = listener.local_addr().expect("local addr");
        let url = format!("http://{addr}/sub.yaml");
        let serve = tokio::spawn(async move {
            if let Ok((mut stream, _)) = listener.accept().await {
                let _ = stream
                    .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 12\r\n\r\nproxies: []\n")
                    .await;
            }
        });

        // Existing remote profile with NO trusted_hosts (the live-case shape).
        let mut store = crate::profile_store::store::tests::empty_store();
        let uid = "R7iHvBBicAOz";
        let bundle = crate::subscribe::from_url::RemoteProfileBundle {
            item: clash_verge_core::config::PrfItem {
                uid: Some(uid.into()),
                itype: Some("remote".into()),
                name: Some("update-trust-demo".into()),
                file: Some(format!("{uid}.yaml").into()),
                url: Some(url.clone().into()),
                file_data: Some("proxies: []\n".into()),
                ..Default::default()
            },
            fragments: vec![match clash_verge_core::config::PrfItem::from_merge(None) {
                Ok(item) => item,
                Err(error) => panic!("merge fragment: {error}"),
            }],
        };
        store.append_bundle(bundle).await.expect("append existing profile");

        // User pressed `y` on the refresh trust prompt (blocked → prompt state).
        let mut app = App::new();
        app.pending_trust = Some(TrustPending {
            url,
            host: "127.0.0.1".to_string(),
            uid: Some(uid.to_string()),
        });
        app.overlay = Some(Overlay::TrustConfirmation);
        let (tx, mut rx) = mpsc::unbounded_channel::<Action>();
        handle_confirm_trust(&mut app, &tx);

        // Confirm must persist the host into the EXISTING profile's stored
        // option and retry the update; the allowlist lets the loopback fetch
        // through, so the refresh reports success.
        let received = tokio::time::timeout(std::time::Duration::from_secs(10), rx.recv())
            .await
            .expect("update result must arrive")
            .expect("action channel stays open");
        match received {
            Action::ProfileUpdated { uid: updated_uid, .. } => assert_eq!(updated_uid, uid),
            other => panic!("expected ProfileUpdated, got {other:?}"),
        }
        let _ = serve.await;

        // The persisted option now carries the normalized trusted host.
        let snapshot = crate::profile_store::store::ProfileStore::snapshot()
            .await
            .expect("snapshot");
        let item = snapshot
            .items()
            .into_iter()
            .find(|item| item.uid.as_deref() == Some(uid))
            .expect("profile still present");
        assert_eq!(
            item.option.and_then(|option| option.trusted_hosts),
            Some(vec!["127.0.0.1".into()])
        );

        // The prompt closed and the pending state is consumed.
        assert!(app.pending_trust.is_none());
        assert_eq!(app.overlay, None);

        let _ = std::fs::remove_dir_all(&root);
    }
}
