//! Core lifecycle: start / stop / restart and the manager's lifecycle notices.

use crate::app::{Action, App, CoreState, TunSetupReason};
use crate::runtime_config::write_runtime_config;

use super::Ctx;
use super::tun::tun_start_offers_setup;

/// `s` on Home.
///
/// Daily path: resolve the binary and run the read-only TUN preflight (no
/// sudo/setcap/askpass here — the manager repeats it before spawn). When the
/// one-time setup (file capability and/or the DNS polkit rule) is missing,
/// the TUI-native setup confirm is offered inline instead of hard-blocking or
/// relying on system dialogs.
pub(super) fn start(app: &mut App, ctx: &Ctx) {
    let enable_tun = app.gui_config.enable_tun_mode.unwrap_or(false);
    app.core_state = CoreState::Starting;
    app.status_msg = Some(app.tr("home.starting_core").into());
    let manager = ctx.manager.clone();
    ctx.spawn(|tx| async move {
        if enable_tun {
            match crate::mihomo_manager::binary::resolve_or_install().await {
                Ok(resolved) => {
                    let capable = crate::commands::privilege::has_tun_capability(&resolved.path);
                    let root = crate::commands::privilege::running_as_root();
                    let needs_setup =
                        tun_start_offers_setup(capable, root, crate::commands::privilege::resolve1_rule_needed(true));
                    if needs_setup {
                        // Record which gate fired: dismissing a capability-missing
                        // prompt must cancel the start, while a missing-DNS-rule
                        // prompt may start anyway.
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
                    let _ = tx.send(Action::CoreError(error.to_string()));
                    return;
                }
            }
        }
        if let Err(error) = start_core_with_tun(&manager, enable_tun).await {
            let _ = tx.send(Action::CoreError(error));
        }
        // On success, the manager emits CoreStarted.
    });
}

/// `S` on Home.
pub(super) fn stop(ctx: &Ctx) {
    let manager = ctx.manager.clone();
    tokio::spawn(async move {
        let _ = manager.stop().await;
    });
}

/// `r` on Home.
pub(super) fn restart(app: &mut App, ctx: &Ctx) {
    app.core_state = CoreState::Starting;
    app.status_msg = Some(app.tr("home.starting_core").into());
    let manager = ctx.manager.clone();
    let enable_tun = app.gui_config.enable_tun_mode.unwrap_or(false);
    ctx.spawn(|tx| async move {
        let config = clash_verge_core::config::IClashTemp::new().await.0;
        if let Err(error) = write_runtime_config(config, enable_tun).await {
            let _ = tx.send(Action::CoreError(error));
            return;
        }
        if let Err(error) = manager.restart().await {
            let _ = tx.send(Action::CoreError(error.to_string()));
        }
    });
}

/// Start the core after a TUN setup prompt resolved.
pub(super) fn resume_start(ctx: &Ctx, enable_tun: bool) {
    let manager = ctx.manager.clone();
    ctx.spawn(|tx| async move {
        if let Err(error) = start_core_with_tun(&manager, enable_tun).await {
            let _ = tx.send(Action::CoreError(error));
        }
        // On success, the manager emits CoreStarted.
    });
}

/// The core is running: either spawned by us (version and binary known) or
/// an already running controller we attached to.
pub(super) fn note_started(
    app: &mut App,
    ctx: &Ctx,
    version: Option<String>,
    binary_path: Option<String>,
    binary_source: Option<String>,
) {
    app.core_state = CoreState::Running;
    app.core_pid = ctx.manager.pid();
    // A core this TUI (re)started logs at its configured level again.
    if binary_path.is_some() {
        app.log_level = app.configured_log_level();
    }
    // Keep the Settings capability state in sync with the binary that
    // actually got spawned (may have changed after an upgrade or a fresh
    // setup).
    if let Some(path) = ctx.manager.binary_path() {
        app.tun_privileged = crate::commands::privilege::has_tun_capability(&path);
    }
    if let Some(version) = version.clone() {
        app.core_version = Some(version);
    } else if app.core_version.is_none() {
        // Attached to an existing controller — ask the API for the version.
        let api = ctx.manager.api();
        ctx.spawn(|tx| async move {
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
    let source_key = match binary_source.as_deref() {
        Some("downloaded") => Some("home.core_downloaded"),
        Some("cached") => Some("home.core_cached"),
        Some("system") => Some("home.core_system"),
        _ => None,
    };
    let version_label = version.as_deref().unwrap_or("mihomo");
    app.status_msg = Some(match source_key {
        Some(key) => format!(
            "{} · {version_label} · {} · pid {pid}",
            app.tr("home.core_started"),
            app.tr(key),
        ),
        None if version.is_some() => format!("{} · {version_label} · pid {pid}", app.tr("home.core_started")),
        None => app.tr("home.core_attached").into(),
    });
    if let Some(path) = binary_path {
        tracing::info!(target: "mihomo", "core binary: {path}");
    }
    for action in [
        Action::ProxiesRefresh,
        Action::TrafficRefresh,
        Action::ConnectionsRefresh,
        Action::LogsRefresh,
    ] {
        ctx.send(action);
    }

    let api = ctx.manager.api();
    ctx.spawn(|tx| async move {
        if let Ok(mode) = api.get_mode().await {
            let _ = tx.send(Action::ModeChanged { mode, announce: false });
        }
    });
}

pub(super) fn note_stopped(app: &mut App) {
    app.core_state = CoreState::Stopped;
    app.core_pid = None;
    app.clear_runtime_caches();
}

pub(super) fn note_error(app: &mut App, msg: String) {
    app.status_msg = Some(format!("{}: {msg}", app.tr("status.error")));
    app.core_state = CoreState::Error(msg);
    app.core_pid = None;
    app.clear_runtime_caches();
}

/// Write the runtime config (with TUN flag) and start the core. Shared by
/// the StartCore key path and the resolve-then-start path.
pub(super) async fn start_core_with_tun(
    manager: &crate::mihomo_manager::manager::MihomoManager,
    enable_tun: bool,
) -> Result<(), String> {
    let config = clash_verge_core::config::IClashTemp::new().await.0;
    write_runtime_config(config, enable_tun).await?;
    manager.start().await.map(|_| ()).map_err(|error| error.to_string())
}
