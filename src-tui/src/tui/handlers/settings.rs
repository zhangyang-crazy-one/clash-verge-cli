//! Settings view rows and the external config editor.

use crate::app::{Action, App, CoreState, EditorTarget};
use crate::i18n::Language;

use super::Ctx;
use crate::services::tun::{apply_tun_runtime, write_tun_runtime};

/// `Enter` on a Settings row (order matches `ui::views::settings`).
pub(super) async fn activate_row(app: &mut App, ctx: &Ctx) {
    match app.settings_selected_index {
        0 => cycle_language(app).await,
        1 => toggle_system_proxy(app).await,
        2 => toggle_tun(app, ctx).await,
        3 => begin_tun_setup(app, ctx),
        4 => super::proxy::cycle_clash_mode(app, ctx),
        _ => {}
    }
}

fn save_failed(app: &mut App, error: impl std::fmt::Display) {
    app.status_msg = Some(format!("{}: {error}", app.tr("settings.save_failed")));
}

async fn cycle_language(app: &mut App) {
    let next_language = app.language.next();
    let mut updated = app.gui_config.clone();
    updated.language = Some(next_language.config_code().into());
    match updated.save_file().await {
        Ok(()) => {
            app.gui_config = updated;
            app.language = next_language;
            app.status_msg = Some(format!(
                "{}: {}",
                app.tr("settings.language_saved"),
                next_language.display_name()
            ));
        }
        Err(error) => {
            app.status_msg = Some(format!("{}: {error}", app.tr("settings.language_save_failed")));
        }
    }
}

async fn toggle_system_proxy(app: &mut App) {
    let enabled = !app.gui_config.enable_system_proxy.unwrap_or(false);
    let previous = app.gui_config.clone();
    let mut updated = previous.clone();
    updated.enable_system_proxy = Some(enabled);
    if let Err(error) = updated.save_file().await {
        save_failed(app, error);
        return;
    }
    let settings = crate::sys_proxy::ProxySettings::from_config(&updated, app.core_config.get_mixed_port());
    // With the core stopped, only persist the setting: it is applied when the
    // core starts rather than pointing at a closed port.
    let apply_result = if !enabled {
        crate::sys_proxy::unset_system_proxy()
    } else if app.core_state == CoreState::Running {
        crate::sys_proxy::set_system_proxy(&settings)
    } else {
        Ok(())
    };
    match apply_result {
        Ok(()) => {
            app.gui_config = updated;
            let key = if enabled {
                "settings.sysproxy_on"
            } else {
                "settings.sysproxy_off"
            };
            app.status_msg = Some(app.tr(key).into());
        }
        Err(error) => {
            let _ = previous.save_file().await;
            app.gui_config = previous;
            save_failed(app, error);
        }
    }
}

async fn toggle_tun(app: &mut App, ctx: &Ctx) {
    let enabled = !app.gui_config.enable_tun_mode.unwrap_or(false);
    if enabled {
        // Read-only preflight BEFORE persisting: never write a TUN-on config
        // that cannot run. Uses the already-resolved binary or the
        // no-download candidate; no sudo/setcap/askpass here — the spawn
        // preflight repeats the check authoritatively.
        if let Err(error) = crate::services::tun::preflight_enable(&ctx.manager) {
            save_failed(app, error);
            return;
        }
        // Same TUI-native warning as the start path: a missing DNS polkit
        // rule means the next start would hit system dialogs.
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
    if let Err(error) = updated.save_file().await {
        save_failed(app, error);
        return;
    }
    app.gui_config = updated;
    let toggled = if enabled { "settings.tun_on" } else { "settings.tun_off" };

    if app.core_state != CoreState::Running {
        // Core is stopped: persist only; applied on the next start.
        match write_tun_runtime(enabled).await {
            Ok(_) => app.status_msg = Some(app.tr(toggled).into()),
            Err(error) => save_failed(app, error),
        }
        return;
    }

    app.status_msg = Some(app.tr(toggled).into());
    let owns_core = ctx.manager.pid().is_some();
    if owns_core {
        app.core_state = CoreState::Starting;
    }
    let manager = ctx.manager.clone();
    ctx.spawn(|tx| async move {
        match apply_tun_runtime(&manager, owns_core, enabled).await {
            Ok(_) if !owns_core => {
                let _ = tx.send(Action::ProxiesRefresh);
            }
            Ok(_) => {}
            Err(error) => {
                let _ = tx.send(Action::CoreError(error));
            }
        }
    });
}

/// Explicit TUN setup — the ONLY TUI authorization action. Start/toggle
/// never open the password popup; this row does.
fn begin_tun_setup(app: &mut App, ctx: &Ctx) {
    let known = ctx
        .manager
        .binary_path()
        .or_else(crate::mihomo_manager::binary::candidate_without_install);
    if let Some(binary) = known
        && crate::commands::privilege::has_tun_capability(&binary)
    {
        app.tun_privileged = true;
        app.status_msg = Some(app.tr("settings.tun_setup_present").into());
        return;
    }
    ctx.spawn(|tx| async move {
        let _ = tx.send(match crate::mihomo_manager::binary::resolve_or_install().await {
            Ok(resolved) if crate::commands::privilege::has_tun_capability(&resolved.path) => {
                Action::TunCapabilityState(true)
            }
            Ok(resolved) => Action::TunSetupRequested(resolved.path),
            Err(error) => Action::CoreError(error.to_string()),
        });
    });
}

/// `e` on Settings: edit `verge.yaml` in `$EDITOR`, restoring the previous
/// file when the result is not valid YAML.
pub(super) async fn open_editor(app: &mut App, ctx: &Ctx, target: EditorTarget) {
    let config_path = match target {
        EditorTarget::Verge => clash_verge_core::utils::dirs::verge_path().ok(),
        // DNS editing is not wired as a separate file yet.
        EditorTarget::Dns => None,
    };
    let Some(path) = config_path else {
        app.status_msg = Some("Config file path not available".into());
        return;
    };
    let snapshot = crate::editor::snapshot(&path).ok();
    let edit_result = {
        let mut guard = ctx.guard.lock().await;
        crate::editor::edit_file_blocking(&mut guard, &path)
    };
    if let Err(e) = edit_result {
        app.status_msg = Some(format!("Editor error: {e}"));
        return;
    }
    match crate::editor::validate_yaml(&path) {
        Ok(()) => {
            app.gui_config = clash_verge_core::config::IVerge::new().await;
            app.language = Language::from_config(app.gui_config.language.as_deref());
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
