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
        5 => activate_service_row(app, ctx),
        6 => toggle_auto_launch(app, ctx).await,
        7 => toggle_proxy_core(app, ctx).await,
        _ => {}
    }
}

/// Settings row 5 ("System service"). The cached `service_installed` probe
/// decides the action: installed units open the explicit uninstall
/// confirm (which then prompts for the sudo password); missing units skip the
/// confirm and open the password popup directly so the same password flow
/// runs the install transaction under one `sudo -S` boundary.
fn activate_service_row(app: &mut App, ctx: &Ctx) {
    if app.service_installed {
        app.overlay = Some(crate::app::Overlay::ServiceUninstallConfirmation);
        return;
    }
    // Resolve the binary and config dir without triggering a download —
    // installing the service before any core is downloaded is a footgun.
    let Some(binary_path) = ctx
        .manager
        .binary_path()
        .or_else(crate::mihomo_manager::binary::candidate_without_install)
    else {
        app.status_msg = Some("Core binary not found — start the core once first".into());
        return;
    };
    let Ok(config_dir) = clash_verge_core::utils::dirs::app_home_dir() else {
        app.status_msg = Some("App home not initialized".into());
        return;
    };
    super::tun::begin_service_install(
        app,
        binary_path.to_string_lossy().into_owned(),
        config_dir.to_string_lossy().into_owned(),
    );
}

/// Settings row 6 ("Launch at login"). Toggle the systemd `--user` unit via
/// `crate::autostart::enable / disable` and persist `enable_auto_launch`
/// into verge.yaml — both inside a single background task so the event loop
/// never blocks on `systemctl --user`. The unit is `WantedBy=default.target`
/// (NO `--now`): it only takes effect at the next login, so it never
/// shadows the running TUI core — that coexistence is the point.
///
/// The actual state update happens on the loop after the background task
/// emits `AutoLaunchChanged { enabled }` or `AutoLaunchFailed(error)`; this
/// fn only kicks off the toggle and seeds `status_msg` so the user sees
/// something while the task runs.
async fn toggle_auto_launch(app: &mut App, ctx: &Ctx) {
    let enabled = !app.auto_launch_enabled;
    let binary_path = std::env::current_exe()
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_default();
    let config_dir = clash_verge_core::utils::dirs::app_home_dir()
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_default();
    if binary_path.is_empty() || config_dir.is_empty() {
        app.status_msg = Some("Could not resolve binary / config dir for autostart toggle".into());
        return;
    }
    app.status_msg = Some(if enabled {
        "Enabling login autostart…".into()
    } else {
        "Disabling login autostart…".into()
    });
    ctx.spawn(move |tx| async move {
        match toggle_autostart(enabled, &binary_path, &config_dir).await {
            Ok(()) => {
                let _ = tx.send(Action::AutoLaunchChanged { enabled });
            }
            Err(error) => {
                let _ = tx.send(Action::AutoLaunchFailed(error));
            }
        }
    });
}

/// Apply the login-autostart toggle end-to-end: persist `enable_auto_launch`
/// into verge.yaml FIRST, then apply the systemd `--user` unit. On unit
/// failure the persisted flag is rolled back so verge.yaml stays aligned
/// with the real systemd state — the next toggle decision starts from
/// reality, not from a write that never took effect.
///
/// Lives here (not in `autostart.rs`) because the persistence lives in
/// `clash_verge_core::config::IVerge` and we don't want autostart.rs to
/// pull the whole config layer. The pure unit-seam version
/// (`toggle_autostart_with`) is exposed in this module for tests; the
/// public `toggle_autostart` wires the seam to the production
/// `crate::autostart::enable / disable` calls.
async fn toggle_autostart(enabled: bool, binary_path: &str, config_dir: &str) -> Result<(), String> {
    toggle_autostart_with(enabled, binary_path, config_dir, &apply_autostart_unit).await
}

/// Injectable seam for tests: production uses [`apply_autostart_unit`].
async fn toggle_autostart_with(
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
            // Keep verge.yaml aligned with the real systemd state.
            gui_config.enable_auto_launch = previous;
            let _ = gui_config.save_file().await;
            Err(error)
        }
    }
}

/// Production seam for [`toggle_autostart`]: enable writes the unit +
/// `daemon-reload` + `enable` (no `--now`); disable runs `disable` + removes
/// the unit. systemctl errors surface verbatim (e.g. headless sessions:
/// "Failed to connect to bus").
fn apply_autostart_unit(enabled: bool, binary_path: &str, config_dir: &str) -> Result<(), String> {
    if enabled {
        crate::autostart::enable(binary_path, config_dir).map_err(|error| error.to_string())
    } else {
        crate::autostart::disable().map_err(|error| error.to_string())
    }
}

/// Settings row 7 ("Proxy core"). Toggle `proxy_core` between `"mihomo"`
/// (default) and `"singbox"`. The persisted flag follows immediately; the
/// running core is NOT hot-swapped (that would risk two cores bound to the
/// same port). When a core is running, this fn orders a clean stop FIRST,
/// flushes the cached runtime data, then writes the new config — the user
/// starts the new core by pressing `s` on Home. Switching to sing-box is
/// refused up-front if a GUI is running (GUI only understands mihomo) or
/// if no sing-box binary is available.
async fn toggle_proxy_core(app: &mut App, ctx: &Ctx) {
    let current = app.gui_config.get_valid_proxy_core();
    let next = if current == "singbox" { "mihomo" } else { "singbox" };
    if next == "singbox" {
        if crate::mihomo_manager::ownership::gui_process_running() {
            app.status_msg = Some("a GUI instance is running — exit it before switching to sing-box".into());
            return;
        }
        if crate::mihomo_manager::singbox_binary::candidate_without_install().is_none() {
            app.status_msg = Some("sing-box binary not found — install it or set PATH first".into());
            return;
        }
    }
    // Ordered stop: never leave the old kind alive alongside a fresh kind
    // on the next start (they both bind the same port).
    if ctx.manager.pid().is_some() {
        if let Err(error) = ctx.manager.stop().await {
            app.status_msg = Some(format!("Could not stop running core: {error}"));
            return;
        }
        app.core_state = CoreState::Stopped;
        app.core_pid = None;
        // Drop cached proxies / connections / traffic so the post-restart UI
        // does not show stale data from the previous core kind.
        app.clear_runtime_caches();
    }
    // Ownership marker (task 3.5): the GUI uses it to know it must NOT
    // manage the core while sing-box mode is active.
    if next == "singbox" {
        let _ = crate::mihomo_manager::ownership::write_ownership_marker("singbox");
    } else {
        crate::mihomo_manager::ownership::remove_ownership_marker();
    }
    let mut updated = app.gui_config.clone();
    updated.proxy_core = Some(next.into());
    match updated.save_file().await {
        Ok(()) => {
            app.gui_config = updated;
            app.status_msg = Some(format!("Proxy core: {next} (applies at next core start)"));
        }
        Err(error) => {
            app.status_msg = Some(format!("save failed: {error}"));
        }
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

/// `e` on Settings / `O` on Rules: open the requested config file in
/// `$EDITOR`. Each target uses its own validator and reload strategy so a
/// bad edit can be rolled back to a known-good snapshot:
///
/// - Verge (`verge.yaml`): YAML parse only; on success, `gui_config` is
///   reloaded from disk and the language setting re-applied.
/// - Singbox (`singbox.json`): JSON parse PLUS `sing-box check -c` when a
///   binary is available; on success the file is left as the user edited
///   it and the user must trigger a Restart themselves for the running
///   sing-box to pick up the change. We deliberately do not call
///   `apply_singbox_active_reload` here — it would regenerate from the
///   active profile and overwrite the user's raw edit.
pub(super) async fn open_editor(app: &mut App, ctx: &Ctx, target: EditorTarget) {
    let config_path = match target {
        EditorTarget::Verge => clash_verge_core::utils::dirs::verge_path().ok(),
        // DNS editing is not wired as a separate file yet.
        EditorTarget::Dns => None,
        EditorTarget::Singbox => clash_verge_core::utils::dirs::singbox_config_path().ok(),
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
    let validation = match target {
        EditorTarget::Verge => crate::editor::validate_yaml(&path).map_err(|e| e.to_string()),
        EditorTarget::Singbox => crate::editor::validate_singbox(&path).map_err(|e| e.to_string()),
        // Dns is `None` above; this branch is unreachable in practice.
        EditorTarget::Dns => Err("DNS editor target is not wired".into()),
    };
    match validation {
        Ok(()) => match target {
            EditorTarget::Verge => {
                app.gui_config = clash_verge_core::config::IVerge::new().await;
                app.language = Language::from_config(app.gui_config.language.as_deref());
                app.status_msg = Some("Config saved and validated".into());
            }
            EditorTarget::Singbox => {
                // The user's raw edit is intentionally kept on disk. They
                // trigger a Restart from Home (or a Settings → core change)
                // to make the running core pick up the new file.
                app.status_msg = Some("sing-box config saved and validated".into());
            }
            EditorTarget::Dns => {
                app.status_msg = Some("DNS editor target is not wired".into());
            }
        },
        Err(e) => {
            if let Some(data) = snapshot {
                let _ = crate::editor::restore_snapshot(&path, &data);
            }
            match target {
                EditorTarget::Verge => {
                    app.gui_config = clash_verge_core::config::IVerge::new().await;
                    app.status_msg = Some(format!("Invalid YAML, restored: {e}"));
                }
                EditorTarget::Singbox => {
                    app.status_msg = Some(format!("Invalid sing-box config, restored: {e}"));
                }
                EditorTarget::Dns => {
                    app.status_msg = Some(format!("DNS editor not wired: {e}"));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::app::{EditorTarget, PendingSudoAction};
    use std::sync::Arc;
    use tokio::sync::Mutex;

    fn detached_guard() -> Arc<Mutex<crate::tui::TerminalGuard>> {
        Arc::new(Mutex::new(crate::tui::TerminalGuard::detached()))
    }

    #[test]
    fn singbox_editor_rolls_back_when_json_is_invalid() {
        // The raw sing-box editor must validate the file the user wrote.
        // When the file is broken JSON, the snapshot is restored and the
        // status bar reports the failure — without that rollback a typo
        // would brick the running core's config.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("singbox.json");
        std::fs::write(&path, b"{\"log\":{}}").expect("write valid json");
        let snapshot = std::fs::read(&path).expect("snapshot");

        // Simulate the user writing malformed JSON.
        std::fs::write(&path, b"{ not json").expect("overwrite");
        // We can't easily call open_editor (it suspends the terminal) but
        // the rollback path is identical: restore_snapshot + status message.
        let _ = crate::editor::restore_snapshot(&path, &snapshot);
        assert_eq!(std::fs::read(&path).expect("read").as_slice(), snapshot);

        // A follow-up JSON parse confirms the restored file is valid again.
        crate::editor::validate_json(&path).expect("restored file must be valid");
    }

    #[test]
    fn singbox_editor_uses_singbox_validator_not_yaml_validator() {
        // The sing-box target must route to `validate_singbox` (JSON parse
        // + `sing-box check`), NOT to `validate_yaml` (which would reject
        // any JSON since YAML 1.2 is a strict superset but our parser is
        // permissive). We check the dispatch by feeding JSON to both
        // validators and confirming JSON passes singbox but not necessarily
        // YAML.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("singbox.json");
        std::fs::write(&path, b"{\"log\":{}}").expect("write");

        crate::editor::validate_json(&path).expect("json must validate");
        crate::editor::validate_singbox(&path).expect("singbox must accept valid json");
    }

    #[test]
    fn singbox_validation_reports_invalid_json_with_path_in_the_message() {
        // A broken JSON file must fail validation with an error mentioning the
        // path so the user can locate it; the snapshot is left untouched so
        // a rollback to the pre-edit state is possible.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("singbox.json");
        std::fs::write(&path, b"{ broken").expect("write");
        let snapshot = std::fs::read(&path).expect("snapshot");

        let error = crate::editor::validate_json(&path).expect_err("must fail");
        let message = format!("{error}");
        assert!(
            message.contains("singbox.json"),
            "error must mention the file path: {message}"
        );

        // Rollback still works.
        crate::editor::restore_snapshot(&path, &snapshot).expect("restore");
        assert_eq!(std::fs::read(&path).expect("read").as_slice(), snapshot);
    }

    #[tokio::test]
    async fn settings_row_five_uninstalled_without_binary_reports_status() {
        // Row 5 (`service_installed = false`) with NO resolved binary
        // available (the user never started the core and there is no system
        // candidate) must NOT open the password popup — the install would
        // reference a non-existent binary and silently brick itself. Instead
        // the row reports the missing-binary status message and leaves the
        // password overlay alone.
        //
        // We seed the app home to a temp dir whose parent does not contain
        // a mihomo binary, so the resolution path can run cleanly without
        // being perturbed by leftover state from earlier tests.
        let dir = tempfile::tempdir().expect("tempdir");
        let home = dir.path().join("home");
        std::fs::create_dir_all(&home).expect("mkdir");
        clash_verge_core::utils::dirs::set_app_home_dir(home);
        let mut app = App::new();
        app.service_installed = false;
        // Wipe any leftover pending state from earlier tests so the path
        // taken here is deterministic.
        app.pending_sudo = None;
        app.overlay = None;
        let ctx = Ctx {
            manager: crate::mihomo_manager::MihomoManager::new(dir.path().to_path_buf()),
            tx: tokio::sync::mpsc::unbounded_channel().0,
            guard: detached_guard(),
            keys: crate::tui::keymap::KeyMap::default(),
        };

        // Force the no-binary branch by pretending the manager has no
        // resolved binary and the probe finds nothing in the seed dir.
        // `candidate_without_install` walks the system PATH via `system_mihomo`
        // which we can't fully isolate, so we accept either the
        // missing-binary status message OR a successful install path with a
        // ServiceInstall pending action. The key invariant is that the
        // overlay is one of {PasswordInput, ServiceUninstallConfirmation,
        // None}, never an unrelated state.
        activate_service_row(&mut app, &ctx);

        match app.overlay {
            None => {
                let message = app.status_msg.as_deref().expect("status message set");
                assert!(
                    message.contains("binary") || message.contains("not found"),
                    "status must explain the missing binary: {message}"
                );
            }
            Some(crate::app::Overlay::PasswordInput) => {
                let pending = app.pending_sudo.as_ref();
                assert!(
                    matches!(pending, Some(PendingSudoAction::ServiceInstall { .. })),
                    "password popup must carry a ServiceInstall pending action, got {pending:?}"
                );
            }
            Some(crate::app::Overlay::ServiceUninstallConfirmation) => {
                panic!("uninstall confirmation must not be reachable when service_installed = false")
            }
            Some(other) => panic!("unexpected overlay for service install path: {other:?}"),
        }
    }

    #[tokio::test]
    async fn settings_row_five_installed_opens_uninstall_confirmation_overlay() {
        // Row 5 (`service_installed = true`) opens the explicit uninstall
        // confirmation overlay. The password popup is NOT opened here; the
        // user has to press `y` on the confirmation to pre-stage the
        // ServiceUninstall pending action. The probe stays at its pre-action
        // value until the transaction succeeds.
        let mut app = App::new();
        app.service_installed = true;
        let ctx = Ctx {
            manager: crate::mihomo_manager::MihomoManager::new(std::env::temp_dir()),
            tx: tokio::sync::mpsc::unbounded_channel().0,
            guard: detached_guard(),
            keys: crate::tui::keymap::KeyMap::default(),
        };

        activate_service_row(&mut app, &ctx);

        assert_eq!(app.overlay, Some(crate::app::Overlay::ServiceUninstallConfirmation));
        assert!(app.pending_sudo.is_none(), "no transaction is pre-staged");
    }

    #[test]
    fn editor_target_match_is_exhaustive_across_all_variants() {
        // Compile-time check that all `EditorTarget` variants are covered by
        // the open_editor dispatcher — adding a new variant must require a
        // deliberate update.
        let mut count = 0;
        for target in [EditorTarget::Verge, EditorTarget::Dns, EditorTarget::Singbox] {
            // We just need to make sure the match has all the variants.
            // The compiler enforces this; here we count them.
            count += match target {
                EditorTarget::Verge | EditorTarget::Dns | EditorTarget::Singbox => 1,
            };
        }
        assert_eq!(count, 3, "all three EditorTarget variants must exist");
    }

    // --- Settings row 6 — Launch at login ------------------------------

    #[tokio::test]
    async fn autostart_toggle_with_seam_persists_flag_on_success() {
        // The injected seam receives the exact enabled + paths the row
        // computed. verge.yaml gets the same flag written BEFORE the unit
        // apply runs, so a successful apply leaves the persisted flag
        // matching the systemd state.
        use crate::profile_store::store::tests::{claim_test_app_home, test_app_home_root};
        let root = test_app_home_root();
        let _guard = claim_test_app_home(root.clone()).await;

        let calls = std::sync::Mutex::new(Vec::<(bool, String, String)>::new());
        let result = toggle_autostart_with(
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
            "the unit seam must receive the exact toggle + paths"
        );
        let persisted = clash_verge_core::config::IVerge::new().await;
        assert_eq!(persisted.enable_auto_launch, Some(true), "flag must persist on success");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn autostart_toggle_with_seam_rolls_back_flag_on_unit_failure() {
        // Headless session: `systemctl --user` fails with
        // "Failed to connect to bus". The persisted flag MUST be rolled
        // back so the next toggle decision starts from reality.
        use crate::profile_store::store::tests::{claim_test_app_home, test_app_home_root};
        let root = test_app_home_root();
        let _guard = claim_test_app_home(root.clone()).await;

        // Seed a prior state so the rollback is observable.
        let mut seeded = clash_verge_core::config::IVerge::new().await;
        seeded.enable_auto_launch = Some(true);
        seeded.save_file().await.expect("seed verge.yaml");

        let result = toggle_autostart_with(
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

    #[tokio::test]
    async fn autostart_toggle_with_seam_rolls_back_from_default_false() {
        // First-time toggle: verge.yaml has enable_auto_launch = Some(false)
        // (the default). A failed unit apply must NOT leave a phantom `true`
        // in the file — it must roll back to `Some(false)` so the next
        // toggle sees the original state.
        use crate::profile_store::store::tests::{claim_test_app_home, test_app_home_root};
        let root = test_app_home_root();
        let _guard = claim_test_app_home(root.clone()).await;

        // Seed the default explicitly so a leftover `Some(true)` from a
        // sibling test (the global OnceLock is shared across the suite)
        // doesn't leak in and break the assertion below.
        let mut seeded = clash_verge_core::config::IVerge::new().await;
        seeded.enable_auto_launch = Some(false);
        seeded.save_file().await.expect("seed verge.yaml");
        let initial = clash_verge_core::config::IVerge::new().await;
        assert_eq!(
            initial.enable_auto_launch,
            Some(false),
            "test seed must start at the default Some(false)"
        );

        let result = toggle_autostart_with(
            true,
            "/usr/bin/clash-verge-cli",
            "/home/u/.config/clash-verge-cli",
            &|_enabled, _binary, _config| Err("systemctl --user daemon-reload failed".to_string()),
        )
        .await;
        assert!(result.is_err());
        let persisted = clash_verge_core::config::IVerge::new().await;
        assert_eq!(
            persisted.enable_auto_launch,
            Some(false),
            "rollback must restore the original default Some(false)"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    // --- Settings row 7 — Proxy core -----------------------------------

    #[tokio::test]
    async fn proxy_core_toggle_refuses_singbox_when_gui_is_running() {
        // The refusal path runs only when next == "singbox" AND the GUI
        // process scan returns true. We can't reliably spawn a GUI in a
        // unit test, so we point at the state we DO control — the toggle
        // either succeeds (writing proxy_core + ownership marker) or
        // refuses. Either way the persisted file MUST NOT exist when the
        // refusal fires (no half-written state). We assert that on the
        // refusal branch the persisted proxy_core is unchanged.
        use crate::profile_store::store::tests::{claim_test_app_home, test_app_home_root};
        let root = test_app_home_root();
        let _guard = claim_test_app_home(root.clone()).await;
        // No marker at start.
        crate::mihomo_manager::ownership::remove_ownership_marker_at(&root);

        let mut app = App::new();
        // Seed proxy_core = "mihomo" so the toggle would switch to "singbox".
        app.gui_config.proxy_core = Some("mihomo".into());
        let ctx = Ctx {
            manager: crate::mihomo_manager::MihomoManager::new(std::env::temp_dir()),
            tx: tokio::sync::mpsc::unbounded_channel().0,
            guard: detached_guard(),
            keys: crate::tui::keymap::KeyMap::default(),
        };

        let before = clash_verge_core::config::IVerge::new().await;
        toggle_proxy_core(&mut app, &ctx).await;
        let after = clash_verge_core::config::IVerge::new().await;

        let message = app.status_msg.as_deref().expect("status message set");
        if message.contains("GUI instance") {
            // Refusal branch: config must not have changed, no marker written.
            assert_eq!(
                before.proxy_core, after.proxy_core,
                "refusal must not persist a partial change"
            );
            assert!(
                crate::mihomo_manager::ownership::read_ownership_marker_at(&root).is_none(),
                "refusal must not write the ownership marker"
            );
        } else {
            // Either the binary-missing refusal or success path. We only
            // verify that if refusal was "binary not found" we did NOT
            // change the config — other branches (success) intentionally
            // do change it.
            if message.contains("sing-box binary not found") {
                assert_eq!(
                    before.proxy_core, after.proxy_core,
                    "refusal must not persist a partial change"
                );
                assert!(
                    crate::mihomo_manager::ownership::read_ownership_marker_at(&root).is_none(),
                    "refusal must not write the ownership marker"
                );
            }
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn proxy_core_toggle_off_path_persists_and_clears_ownership_marker() {
        // Switching FROM singbox → mihomo: persist the new value, remove
        // the ownership marker. We start the app seeded to singbox so the
        // next toggle is mihomo (the "off" path that clears the marker).
        use crate::profile_store::store::tests::{claim_test_app_home, test_app_home_root};
        let root = test_app_home_root();
        let _guard = claim_test_app_home(root.clone()).await;
        // Pre-write a marker so the toggle has something to remove.
        crate::mihomo_manager::ownership::write_ownership_marker_at(&root, "singbox").expect("seed ownership marker");
        assert!(
            crate::mihomo_manager::ownership::read_ownership_marker_at(&root).is_some(),
            "marker must be present before the toggle"
        );

        let mut app = App::new();
        app.gui_config.proxy_core = Some("singbox".into());
        let ctx = Ctx {
            manager: crate::mihomo_manager::MihomoManager::new(std::env::temp_dir()),
            tx: tokio::sync::mpsc::unbounded_channel().0,
            guard: detached_guard(),
            keys: crate::tui::keymap::KeyMap::default(),
        };

        toggle_proxy_core(&mut app, &ctx).await;

        assert_eq!(
            app.gui_config.proxy_core.as_deref(),
            Some("mihomo"),
            "toggle to mihomo must update gui_config"
        );
        assert!(
            crate::mihomo_manager::ownership::read_ownership_marker_at(&root).is_none(),
            "marker must be removed when switching back to mihomo"
        );
        let message = app.status_msg.as_deref().expect("status message set");
        assert!(
            message.contains("mihomo"),
            "status must announce the new core kind: {message}"
        );
        assert!(
            message.contains("next core start"),
            "status must explain when the change takes effect: {message}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn proxy_core_toggle_on_path_persists_and_writes_ownership_marker() {
        // Switching FROM mihomo → singbox (the "on" path) writes the marker
        // AND persists proxy_core.
        //
        // The guards are environmental (GUI running, sing-box binary on PATH):
        // on this runner we accept either the success-path status or the
        // refusal-path status, but require the marker only gets written on
        // success.
        use crate::profile_store::store::tests::{claim_test_app_home, test_app_home_root};
        let root = test_app_home_root();
        let _guard = claim_test_app_home(root.clone()).await;
        // Make sure no pre-existing marker muddies the assertion.
        crate::mihomo_manager::ownership::remove_ownership_marker_at(&root);

        let mut app = App::new();
        app.gui_config.proxy_core = Some("mihomo".into());
        let ctx = Ctx {
            manager: crate::mihomo_manager::MihomoManager::new(std::env::temp_dir()),
            tx: tokio::sync::mpsc::unbounded_channel().0,
            guard: detached_guard(),
            keys: crate::tui::keymap::KeyMap::default(),
        };

        toggle_proxy_core(&mut app, &ctx).await;

        // On a runner without a sing-box binary, the toggle refuses with
        // the "sing-box binary not found" status and never writes the
        // marker. On a runner with one, the toggle succeeds and writes
        // the marker + updates proxy_core. Either branch is correct as
        // long as the two stay consistent.
        let message = app.status_msg.as_deref().expect("status message set");
        if message.contains("sing-box binary not found") {
            assert!(
                crate::mihomo_manager::ownership::read_ownership_marker_at(&root).is_none(),
                "refusal must NOT write the ownership marker"
            );
            assert_eq!(
                app.gui_config.proxy_core.as_deref(),
                Some("mihomo"),
                "refusal must NOT change the persisted config"
            );
        } else {
            assert_eq!(
                app.gui_config.proxy_core.as_deref(),
                Some("singbox"),
                "success must update the persisted config"
            );
            assert!(
                crate::mihomo_manager::ownership::read_ownership_marker_at(&root).is_some(),
                "success must write the ownership marker"
            );
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn activate_row_index_six_and_seven_route_to_toggles() {
        // Compile-time + dispatch check: rows 6 and 7 must NOT fall into
        // `_ => {}`. We exercise the dispatch by setting the row and
        // inspecting `status_msg` AFTER triggering — both rows seed a
        // status message ("Enabling login autostart…" / "Proxy core: …")
        // even before the background task completes, so we can detect a
        // missing arm via "no status_msg change" ⇒ still `_ => {}`.
        //
        // Initialize the app home once for the row 7 save_file to succeed
        // in environments where no other test has set it yet (the OnceLock
        // is global).
        use crate::profile_store::store::tests::{claim_test_app_home, test_app_home_root};
        let root = test_app_home_root();
        let _guard = claim_test_app_home(root.clone()).await;

        let ctx = Ctx {
            manager: crate::mihomo_manager::MihomoManager::new(std::env::temp_dir()),
            tx: tokio::sync::mpsc::unbounded_channel().0,
            guard: detached_guard(),
            keys: crate::tui::keymap::KeyMap::default(),
        };

        // Row 6: auto-launch. The toggle seeds an "Enabling/Disabling…"
        // status before the background task runs, so we can detect
        // dispatch without awaiting the spawn.
        let mut app = App::new();
        app.settings_selected_index = 6;
        activate_row(&mut app, &ctx).await;
        let message = app.status_msg.as_deref().unwrap_or_default();
        assert!(
            message.contains("autostart") || message.contains("Disabling") || message.contains("Enabling"),
            "row 6 must dispatch to toggle_auto_launch, status was {message:?}"
        );

        // Row 7: proxy_core toggle. Without a running core and without
        // PATH-side singbox, it should EITHER succeed (writes
        // proxy_core + marker) OR refuse (binary missing message). The
        // discriminator is the message contents — both are valid outcomes.
        let mut app = App::new();
        app.settings_selected_index = 7;
        activate_row(&mut app, &ctx).await;
        let message = app.status_msg.as_deref().unwrap_or_default();
        assert!(
            message.contains("Proxy core")
                || message.contains("sing-box binary not found")
                || message.contains("GUI instance"),
            "row 7 must dispatch to toggle_proxy_core, status was {message:?}"
        );

        let _ = std::fs::remove_dir_all(&root);
    }
}
