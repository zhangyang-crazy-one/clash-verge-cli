//! Profiles: switching, import (with the SSRF trust prompt), manual and
//! automatic subscription updates.

use crossterm::event::KeyCode;
use tokio::sync::mpsc;

use crate::app::{Action, App, CoreState, Focus, InputMode, Overlay, TrustPending};
use crate::mihomo_manager::CoreKind;
use crate::profile_store::store::ProfileStore;
use crate::runtime_config::reload_remote_profile;

use super::Ctx;

/// Keys while the import URL prompt is open.
pub(super) fn import_input(app: &mut App, ctx: &Ctx, mut buffer: String, code: KeyCode) {
    match code {
        KeyCode::Enter => {
            app.input_mode = InputMode::Normal;
            app.status_msg = Some("Importing...".into());
            ctx.send(Action::ConfirmImport(buffer));
        }
        KeyCode::Esc => app.input_mode = InputMode::Normal,
        KeyCode::Backspace => {
            buffer.pop();
            app.input_mode = InputMode::Importing(buffer);
        }
        KeyCode::Char(c) => {
            buffer.push(c);
            app.input_mode = InputMode::Importing(buffer);
        }
        _ => {}
    }
}

/// The profile under the cursor, unless a filter hides it.
fn selected_profile(app: &App) -> Option<&clash_verge_core::config::PrfItem> {
    app.visible_profile_indices()
        .contains(&app.selected_index)
        .then(|| app.profiles.get(app.selected_index))
        .flatten()
}

/// `Enter` on Profiles: make the selected profile current.
pub(super) fn switch_selected(app: &mut App, ctx: &Ctx) {
    let Some(item) = selected_profile(app).cloned() else {
        return;
    };
    if item.uid.is_none() {
        return;
    }
    let name = item.name.clone().unwrap_or_default();
    app.status_msg = Some(format!("Switching to {name}..."));
    let manager = ctx.manager.clone();
    let enable_tun = app.gui_config.enable_tun_mode.unwrap_or(false);
    let core_running = app.core_state == CoreState::Running;
    let uid = item.uid.as_deref().unwrap_or_default().to_string();
    ctx.spawn(|tx| async move {
        // Dispatch on the manager's core kind: `switch_profile` is the
        // mihomo PUT /configs path, which sing-box's controller silently
        // ignores. `switch_profile_for_core` routes the sing-box branch
        // through `apply_singbox_restart` (regenerate JSON config,
        // prevalidate, restart) so the running core actually picks up
        // the switch.
        match crate::services::profile::switch_profile_for_core(&manager, &item, enable_tun, core_running).await {
            Ok(()) => {
                let _ = tx.send(Action::ProfileSwitched(uid));
            }
            Err(error) => {
                let _ = tx.send(Action::CoreError(error));
            }
        }
    });
}

/// `u` on Profiles: refresh the selected remote profile (or all of them).
///
/// A host the SSRF check blocks (and the profile does not already trust)
/// opens the same interactive trust prompt as import. Background/auto
/// updates never route here, so they can never prompt.
pub(super) fn update_selected(app: &mut App, ctx: &Ctx) {
    let selected = selected_profile(app).cloned();
    if selected.is_none() && !app.profiles.is_empty() {
        // Only an empty list means "update all"; a filter hiding the
        // selection must not turn `u` into a bulk update.
        app.status_msg = Some(app.tr("profiles.no_selection").into());
        return;
    }
    if let Some((uid, host)) = selected.as_ref().and_then(update_flow_decision) {
        ctx.send(Action::UpdateNeedsTrust { uid, host });
        return;
    }
    app.status_msg = Some("Updating subscriptions...".into());
    let selected_uid = selected.and_then(|item| item.uid.clone()).map(|u| u.to_string());
    ctx.spawn_result(
        async move {
            match selected_uid {
                Some(uid) => ProfileStore::update_remote_locked(&uid, None)
                    .await
                    .map(|is_current| (uid, is_current)),
                None => ProfileStore::update_all_remote_locked().await.map(|currents| {
                    let uid = currents.first().map(|u| u.to_string()).unwrap_or_default();
                    (uid, !currents.is_empty())
                }),
            }
        },
        |(uid, is_current)| Action::ProfileUpdated { uid, is_current },
        |error| Action::ProfileUpdateFailed(error.to_string()),
    );
}

/// Import URL submitted. Ordinary import path: SSRF protection stays
/// enabled. The safety check runs off the event loop; if the host is
/// blocked, the user gets an explicit trust prompt instead of a silent
/// failure.
pub(super) fn confirm_import(ctx: &Ctx, url: String) {
    ctx.spawn(|tx| async move {
        match ssrf_blocked_host(&url) {
            Some(host) => {
                let _ = tx.send(Action::ImportNeedsTrust { url, host });
            }
            None => spawn_import(&tx, url, None),
        }
    });
}

/// Open the SSRF trust prompt for an import blocked on `host`.
pub(super) fn begin_import_trust(app: &mut App, url: String, host: String) {
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

/// A profile was imported: select it, write its runtime config, and start
/// the core if it is not running yet.
pub(super) async fn note_imported(app: &mut App, ctx: &Ctx) {
    app.status_msg = Some("Profile imported successfully".into());
    let Ok(store) = ProfileStore::snapshot().await else {
        return;
    };
    app.load_profiles(&store);
    let Some(last) = app.profiles.len().checked_sub(1) else {
        return;
    };
    // Auto-select the newly imported (last) profile.
    app.selected_index = last;
    let Some(item) = app.profiles.get(last).cloned() else {
        return;
    };
    let api = ctx.manager.api();
    let manager = ctx.manager.clone();
    let enable_tun = app.gui_config.enable_tun_mode.unwrap_or(false);
    let core_running = app.core_state == CoreState::Running;
    let uid = item.uid.as_deref().unwrap_or_default().to_string();
    ctx.spawn(|tx| async move {
        // Dispatch on the manager's core kind. The mihomo path keeps
        // its PUT /configs hot reload; sing-box's controller would
        // silently accept the request and drop the payload, so we route
        // through `apply_singbox_restart`, which writes the JSON config,
        // prevalidates with `sing-box check`, and restarts the process.
        // For sing-box we read the imported YAML from disk — the import
        // path (`append_bundle` → `append_item` → `fs::write`) already
        // persisted it before this action fires.
        let apply_result = match manager.core_kind() {
            CoreKind::Mihomo => reload_remote_profile(&api, &item, enable_tun, core_running).await,
            CoreKind::SingBox => apply_imported_profile_to_singbox(&manager, &item, enable_tun).await,
        };
        if let Err(error) = apply_result {
            let _ = tx.send(Action::CoreError(error));
            return;
        }
        // The core now runs the imported profile: record it as current so
        // profiles.yaml, Home, and the status bar agree with the core.
        match ProfileStore::replace_current_locked(&uid).await {
            // Refreshes the proxies when the core is running.
            Ok(_) => {
                let _ = tx.send(Action::ProfileSwitched(uid));
            }
            Err(error) => {
                let _ = tx.send(Action::CoreError(format!("profile switch: {error}")));
            }
        }
        if !core_running {
            let _ = manager.start().await;
        }
    });
}

/// Apply a freshly imported profile to a sing-box core: read the body
/// the import just wrote and regenerate the JSON runtime config through
/// [`crate::runtime_config::apply_singbox_restart`]. Extracted so the
/// `note_imported` branch reads at a glance.
async fn apply_imported_profile_to_singbox(
    manager: &crate::mihomo_manager::MihomoManager,
    item: &clash_verge_core::config::PrfItem,
    enable_tun: bool,
) -> Result<(), String> {
    let file = item.file.as_deref().ok_or_else(|| {
        format!(
            "profile reload: imported profile {} has no file",
            item.uid.as_deref().unwrap_or("?")
        )
    })?;
    let dir = clash_verge_core::utils::dirs::app_profiles_dir().map_err(|error| format!("profile reload: {error}"))?;
    let path = dir.join(file);
    let yaml = tokio::fs::read_to_string(&path)
        .await
        .map_err(|error| format!("profile reload: failed to read {}: {error}", path.display()))?;
    crate::runtime_config::apply_singbox_restart(manager, Some(yaml.as_str()), enable_tun)
        .await
        .map(|_report| ())
        .map_err(|error| format!("profile reload: {error}"))
}

/// A profile refresh finished: re-read the list and reload the core when the
/// current profile changed.
pub(super) async fn note_updated(app: &mut App, ctx: &Ctx, uid: String, is_current: bool) {
    app.status_msg = Some(if uid.is_empty() {
        "Subscriptions updated".into()
    } else {
        format!("Updated profile {uid}")
    });
    if let Ok(store) = ProfileStore::snapshot().await {
        app.load_profiles(&store);
    }
    if !is_current {
        return;
    }
    let manager = ctx.manager.clone();
    let enable_tun = app.gui_config.enable_tun_mode.unwrap_or(false);
    let core_running = app.core_state == CoreState::Running;
    ctx.spawn(|tx| async move {
        // Use the dispatching wrapper so a sing-box subscription refresh
        // regenerates the JSON config and restarts the core instead of
        // issuing a no-op `PUT /configs` against sing-box's controller.
        match crate::subscribe::scheduler::reload_current_profile_for_core(&manager, &uid, enable_tun, core_running)
            .await
        {
            Ok(()) if core_running => {
                let _ = tx.send(Action::ProxiesRefresh);
            }
            Ok(()) => {}
            Err(error) => {
                let _ = tx.send(Action::CoreError(format!("profile reload: {error}")));
            }
        }
    });
}

/// Run one auto-update round (interval refreshes plus the exit-node probe)
/// in the background. Results arrive as profile actions; `AutoUpdateFinished`
/// always closes the round.
pub(in crate::tui) fn spawn_auto_update(
    app: &App,
    ctx: &Ctx,
    scheduler: std::sync::Arc<tokio::sync::Mutex<crate::subscribe::scheduler::AutoUpdateScheduler>>,
) {
    let manager = ctx.manager.clone();
    let enable_tun = app.gui_config.enable_tun_mode.unwrap_or(false);
    let core_running = app.core_state == CoreState::Running;
    ctx.spawn(|tx| async move {
        let (outcome, probe) = {
            let mut scheduler = scheduler.lock().await;
            (
                scheduler.tick().await,
                // `probe_with_manager` dispatches the forced refresh by
                // core kind: the old `probe` only knew the API and would
                // hand sing-box a no-op `PUT /configs` on a sustained
                // node failure.
                scheduler.probe_with_manager(&manager, enable_tun, core_running).await,
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
                "probe: selected node vanished — refresh rolled back"
            } else if probe.may_be_down {
                "probe: subscription may be down"
            } else {
                "probe: node recovered — subscription refreshed"
            };
            let _ = tx.send(Action::ProbeNotice(notice.to_string()));
        }
        let _ = tx.send(Action::AutoUpdateFinished);
    });
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
pub(super) fn ssrf_blocked_host(url: &str) -> Option<String> {
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
pub(super) fn spawn_import(
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
pub(super) fn handle_confirm_trust(app: &mut App, action_tx: &mpsc::UnboundedSender<Action>) {
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
pub(super) fn handle_cancel_trust(app: &mut App) {
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
pub(super) fn begin_update_trust(app: &mut App, uid: String, host: String) {
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
pub(super) fn update_flow_decision(item: &clash_verge_core::config::PrfItem) -> Option<(String, String)> {
    let uid = item.uid.as_deref()?;
    let url = item.url.as_deref()?;
    let allowlist = crate::subscribe::from_url::trusted_hosts_allowlist(item.option.as_ref());
    if crate::subscribe::ssrf::check_url_host(url, &allowlist).is_ok() {
        return None;
    }
    let host = ssrf_blocked_host(url)?;
    Some((uid.to_string(), host))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::handlers::navigation::dismiss_overlay;

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
