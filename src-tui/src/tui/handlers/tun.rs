//! TUN setup flow: the core-start setup confirm and the password popup.
//!
//! The same password popup is reused by the service install / uninstall
//! flows: each spawns exactly one `sudo -S` transaction whose contents come
//! from `PendingSudoAction`. The TUI-native setup confirm is still owned by
//! `begin_tun_setup_confirm` / `skip_tun_setup_start`; the service path
//! opens the password popup directly through `begin_service_install` and
//! `confirm_service_uninstall`.

use tokio::sync::mpsc;

use crate::app::{Action, App, CoreState, Focus, Overlay, PendingSudoAction, TunSetupReason};

/// Settings → TUN setup found an uncapped binary: ask for the sudo password.
pub(super) fn open_password_prompt(app: &mut App, binary: std::path::PathBuf) {
    app.password_prompt = Some(app.tr("settings.tun_setup_prompt").into());
    app.password_buffer.clear();
    app.pending_sudo = Some(PendingSudoAction::TunSetup {
        binary,
        resume_start: None,
        reason: TunSetupReason::MissingCapability,
    });
    app.overlay = Some(Overlay::PasswordInput);
}

/// Open the password popup with a `ServiceInstall` pending action. The
/// transaction writes the systemd unit using the captured password
/// (`sudo -S`, no askpass) and starts the service in the same boundary.
pub(super) fn begin_service_install(app: &mut App, binary_path: String, config_dir: String) {
    app.password_prompt = Some(app.tr("settings.service_install_prompt").into());
    app.password_buffer.clear();
    app.pending_sudo = Some(PendingSudoAction::ServiceInstall {
        binary_path,
        config_dir,
    });
    app.overlay = Some(Overlay::PasswordInput);
}

/// `y` on the `ServiceUninstallConfirmation` overlay: open the password
/// popup with a `ServiceUninstall` pending action. The transaction runs the
/// whole uninstall (stop + disable + remove unit + reload systemd) under a
/// single `sudo -S` boundary.
pub(super) fn confirm_service_uninstall(app: &mut App) {
    app.password_prompt = Some(app.tr("settings.service_uninstall_prompt").into());
    app.password_buffer.clear();
    app.pending_sudo = Some(PendingSudoAction::ServiceUninstall);
    app.overlay = Some(Overlay::PasswordInput);
}

/// `n` / Esc / `q` on the `ServiceUninstallConfirmation` overlay: cancel.
/// The pending state was never opened (the service uninstall confirmation
/// does not pre-stage a transaction), so dropping the overlay is enough —
/// no transaction runs, no service state is touched.
pub(super) fn cancel_service_uninstall(app: &mut App) {
    app.overlay = None;
    app.status_msg = Some(app.tr("settings.service_uninstall_cancelled").into());
}

/// Handle a submitted password for the active `PendingSudoAction`. The
/// popup is shared by three flows — TUN capability setup, service install,
/// service uninstall — and dispatches exactly one transaction:
///
/// - A submit with no pending action is a stale duplicate Enter (e.g. the
///   second Enter of a double-press after the popup already closed): it is
///   ignored instead of aborting the TUI event loop. The spawned task only
///   runs when a pending action actually exists.
/// - On success the resume context (`resume_start`, set when the TUN setup
///   was offered from the core-start prompt) travels with
///   `TunSetupSucceeded` so the pending core start resumes automatically;
///   the explicit Settings flow passes `None`. Service install / uninstall
///   surface their own terminal actions so the Settings row can refresh
///   its cached `service_installed` probe.
pub(super) fn handle_password_submit(app: &mut App, action_tx: &mpsc::UnboundedSender<Action>) {
    let Some(pending) = app.pending_sudo.take() else {
        return;
    };
    let password: String = app.password_buffer.drain(..).collect();
    app.overlay = None;
    let tx = action_tx.clone();
    match pending {
        PendingSudoAction::TunSetup {
            binary,
            resume_start,
            reason: _,
        } => {
            tokio::spawn(async move {
                match crate::commands::privilege::apply_tun_capability_with_password(&binary, &password) {
                    Ok(()) => {
                        let _ = tx.send(Action::TunSetupSucceeded { resume_start });
                    }
                    Err(error) => {
                        let _ = tx.send(Action::CoreError(error.to_string()));
                    }
                }
            });
        }
        PendingSudoAction::ServiceInstall {
            binary_path,
            config_dir,
        } => {
            tokio::spawn(async move {
                match crate::service_cmd::install_service_with_password(&binary_path, &config_dir, true, &password) {
                    Ok(()) => {
                        let _ = tx.send(Action::ServiceInstalled);
                    }
                    Err(error) => {
                        let _ = tx.send(Action::ServiceActionFailed(error.to_string()));
                    }
                }
            });
        }
        PendingSudoAction::ServiceUninstall => {
            tokio::spawn(async move {
                match crate::service_cmd::uninstall_service_with_password(&password) {
                    Ok(()) => {
                        let _ = tx.send(Action::ServiceUninstalled);
                    }
                    Err(error) => {
                        let _ = tx.send(Action::ServiceActionFailed(error.to_string()));
                    }
                }
            });
        }
    }
}

/// Cancel the password popup. When a core start depended on this setup, the
/// start is abandoned: the transient `Starting` state is reset to `Stopped`
/// and nothing stale remains (no resume can fire). Service install /
/// uninstall pipelines abandon cleanly — no transaction runs, the cached
/// service probe stays at its pre-action value, and the status bar reports
/// the cancellation localized through `settings.service_cancelled`.
pub(super) fn handle_password_cancel(app: &mut App) {
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
        Some(PendingSudoAction::ServiceInstall { .. } | PendingSudoAction::ServiceUninstall) => {
            app.status_msg = Some(app.tr("settings.service_cancelled").into());
        }
        _ => {
            app.status_msg = Some("TUN setup cancelled".into());
        }
    }
}

/// Pure decision for the TUI-native setup gate on core start: the inline
/// confirm is offered when the binary lacks the TUN file capability (and the
/// process is not root) OR the systemd-resolved DNS polkit rule is missing.
/// Injectable so all four combinations are testable without getcap/polkit.
pub(super) fn tun_start_offers_setup(capable: bool, root: bool, rule_needed: bool) -> bool {
    let cap_ok = root || capable;
    !cap_ok || rule_needed
}

/// Open the TUI-native setup confirm dialog for a TUN-enabled core start
/// that needs the one-time setup. The pending state carries the resolved
/// binary and `resume_start: Some(enable_tun)` so a confirmed setup resumes
/// the start on success, plus the gate that fired (`reason`) so the skip key
/// knows whether starting anyway is safe.
pub(super) fn begin_tun_setup_confirm(
    app: &mut App,
    binary: std::path::PathBuf,
    enable_tun: bool,
    reason: TunSetupReason,
) {
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
pub(super) fn confirm_tun_setup(app: &mut App) {
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
pub(super) fn skip_tun_setup_start(app: &mut App, action_tx: &mpsc::UnboundedSender<Action>) {
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
pub(super) fn note_tun_setup_succeeded(
    app: &mut App,
    resume_start: Option<bool>,
    action_tx: &mpsc::UnboundedSender<Action>,
) {
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

#[cfg(test)]
mod tests {
    use super::*;

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
        handle_password_submit(&mut app, &tx);

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
        let pending = match app.pending_sudo.as_ref() {
            Some(PendingSudoAction::TunSetup {
                binary,
                resume_start,
                reason,
            }) => (binary.clone(), *resume_start, *reason),
            other => panic!("pending setup must be a TunSetup variant, got {other:?}"),
        };
        assert_eq!(pending.1, Some(true), "confirm must carry enable_tun for the resume");
        assert_eq!(
            pending.2,
            TunSetupReason::MissingCapability,
            "confirm must carry the gate that fired"
        );
        assert_eq!(pending.0, std::path::PathBuf::from("/fake/mihomo"));
        assert_eq!(
            app.status_msg.as_deref(),
            Some(app.tun_setup_confirm_hint()),
            "status hint must match the hard-gate choice text"
        );
    }

    #[test]
    fn confirm_tun_setup_opens_the_password_popup_keeping_resume_context() {
        // `y` on the confirm reuses the existing password popup; the resume
        // context stays in pending_sudo so the submit can resume the start.
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
        let resume = match app.pending_sudo.as_ref() {
            Some(PendingSudoAction::TunSetup { resume_start, .. }) => *resume_start,
            other => panic!("pending must remain a TunSetup variant, got {other:?}"),
        };
        assert_eq!(resume, Some(true), "password popup must keep the pending resume");
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

    // --- Service install / uninstall password flow ----------------------

    fn begin_service_install_bare(app: &mut App) {
        begin_service_install(
            app,
            "/usr/bin/clash-verge-cli".to_string(),
            "/home/u/.config/clash-verge-cli".to_string(),
        );
    }

    #[test]
    fn begin_service_install_opens_password_popup_with_pending_install() {
        // Settings row 5 (service, not installed) → `Enter` must stage the
        // password popup with the ServiceInstall pending action. No
        // transaction has run yet (the popup hasn't been submitted), so the
        // service probe is unchanged.
        let mut app = App::new();
        begin_service_install_bare(&mut app);

        assert_eq!(app.overlay, Some(Overlay::PasswordInput));
        assert!(app.password_prompt.is_some(), "prompt label must be set");
        let pending = match app.pending_sudo.as_ref() {
            Some(PendingSudoAction::ServiceInstall {
                binary_path,
                config_dir,
            }) => (binary_path.clone(), config_dir.clone()),
            other => panic!("pending must be a ServiceInstall variant, got {other:?}"),
        };
        assert_eq!(pending.0, "/usr/bin/clash-verge-cli");
        assert_eq!(pending.1, "/home/u/.config/clash-verge-cli");
    }

    #[test]
    fn confirm_service_uninstall_opens_password_popup_with_pending_uninstall() {
        // `y` on ServiceUninstallConfirmation → password popup with the
        // ServiceUninstall pending action. The pre-action service probe is
        // preserved so the row's status text stays honest about what changed.
        let mut app = App::new();
        app.overlay = Some(Overlay::ServiceUninstallConfirmation);
        app.service_installed = true;
        confirm_service_uninstall(&mut app);

        assert_eq!(app.overlay, Some(Overlay::PasswordInput));
        assert!(app.password_prompt.is_some(), "prompt label must be set");
        assert!(
            matches!(app.pending_sudo.as_ref(), Some(PendingSudoAction::ServiceUninstall)),
            "pending action must be ServiceUninstall"
        );
        assert!(app.service_installed, "probe is only refreshed on success");
    }

    #[test]
    fn cancel_service_uninstall_drops_the_overlay_without_touching_state() {
        // `n`/Esc/q on the uninstall confirmation drops the overlay and
        // reports the localized cancellation. No password popup was opened
        // (cancel happens before y), so pending_sudo stays empty.
        let mut app = App::new();
        app.overlay = Some(Overlay::ServiceUninstallConfirmation);
        app.service_installed = true;
        cancel_service_uninstall(&mut app);

        assert_eq!(app.overlay, None, "cancel must dismiss the confirm overlay");
        assert!(app.pending_sudo.is_none(), "cancel never pre-stages a transaction");
        assert!(app.service_installed, "probe stays at the pre-action value");
        assert_eq!(
            app.status_msg.as_deref(),
            Some(app.tr("settings.service_uninstall_cancelled")),
            "cancel must report the localized uninstall-cancelled message"
        );
    }

    #[test]
    fn password_cancel_during_service_install_uses_service_message() {
        // Esc on the password popup while a ServiceInstall is pending: the
        // cancel path must use the localized service-cancelled message and
        // must NOT reset core_state (no resume was ever started).
        let mut app = App::new();
        app.core_state = CoreState::Stopped;
        begin_service_install_bare(&mut app);
        app.password_buffer = vec!['x'];

        handle_password_cancel(&mut app);

        assert!(app.pending_sudo.is_none(), "pending action is dropped on cancel");
        assert_eq!(app.overlay, None);
        assert!(app.password_buffer.is_empty());
        assert_eq!(
            app.core_state,
            CoreState::Stopped,
            "service-install cancel must NOT reset core_state"
        );
        assert_eq!(
            app.status_msg.as_deref(),
            Some(app.tr("settings.service_cancelled")),
            "cancel must use the localized service-cancelled message"
        );
    }

    #[test]
    fn password_cancel_during_service_uninstall_uses_service_message() {
        // Esc on the password popup while a ServiceUninstall is pending:
        // cancel must drop the pending action, leave the service probe at
        // its pre-action value, and report the localized cancellation.
        let mut app = App::new();
        app.overlay = Some(Overlay::ServiceUninstallConfirmation);
        app.service_installed = true;
        confirm_service_uninstall(&mut app);
        app.password_buffer = vec!['x'];

        handle_password_cancel(&mut app);

        assert!(app.pending_sudo.is_none(), "pending action is dropped on cancel");
        assert_eq!(app.overlay, None);
        assert!(app.password_buffer.is_empty());
        assert!(app.service_installed, "probe stays at the pre-action value");
        assert_eq!(
            app.status_msg.as_deref(),
            Some(app.tr("settings.service_cancelled")),
            "cancel must use the localized service-cancelled message"
        );
    }
}
