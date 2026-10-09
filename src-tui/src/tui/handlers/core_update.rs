//! Confirmation-gated preparation and owned-core switching. This operation
//! has its own cancellation token and join handle, independent of live traffic.
use super::Ctx;
use crate::app::{Action, App, CoreIntent, CoreUpdate, CoreUpdatePhase, GuidedTunContext, Overlay, PendingSudoAction};
use crate::mihomo_manager::{
    CoreKind,
    binary::{self, CoreInspection, CoreProgressPhase, DownloadAuthorization},
};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

pub(super) fn begin(app: &mut App, ctx: &Ctx, kind: CoreKind, intent: CoreIntent) {
    begin_with(app, ctx, kind, intent, async move {
        // Inspection and confirmed acquisition only touch CLI-owned candidates.
        // GUI/foreign ownership is checked at the lifecycle boundary, allowing
        // users to prepare an update while their current network still works.
        Ok(binary::inspect(kind).await)
    });
}

pub(super) fn begin_with<F>(app: &mut App, ctx: &Ctx, kind: CoreKind, intent: CoreIntent, work: F)
where
    F: std::future::Future<Output = anyhow::Result<CoreInspection>> + Send + 'static,
{
    if app.core_operation_task.as_ref().is_some_and(|task| !task.is_finished())
        || app.core_update.as_ref().is_some_and(|update| {
            !matches!(
                update.phase,
                CoreUpdatePhase::Failed | CoreUpdatePhase::Success | CoreUpdatePhase::Cancelled
            )
        })
    {
        app.overlay = Some(Overlay::CoreUpdate);
        return;
    }
    app.core_operation_sequence += 1;
    let id = app.core_operation_sequence;
    let cancelled = Arc::new(AtomicBool::new(false));
    app.core_update = Some(CoreUpdate {
        id,
        generation: ctx.manager.current_generation(),
        kind,
        intent,
        phase: CoreUpdatePhase::Checking,
        request: None,
        prepared: None,
        message: String::new(),
        cancelled: cancelled.clone(),
    });
    app.overlay = Some(Overlay::CoreUpdate);
    app.status_msg = Some(app.tr("core_update.checking").into());
    let tx = ctx.tx.for_operation();
    app.core_operation_task = Some(tokio::spawn(async move {
        let inspection = tokio::select! {
            value = work => value,
            _ = wait_cancel(&cancelled) => return,
        };
        let action = match inspection {
            Ok(result) => Action::CoreInspected { id, result },
            Err(error) => Action::CoreUpdateFinished {
                id,
                result: Err(error.to_string()),
            },
        };
        let _ = tx.send(action).await;
    }));
}

async fn wait_cancel(cancelled: &AtomicBool) {
    while !cancelled.load(Ordering::SeqCst) {
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
}

pub(super) fn confirm(app: &mut App, ctx: &Ctx) {
    let Some(update) = app.core_update.as_mut() else {
        return;
    };
    if update.cancelled.load(Ordering::SeqCst) {
        return;
    }
    let id = update.id;
    let kind = update.kind;
    let cancelled = update.cancelled.clone();
    let tx = ctx.tx.for_operation();
    match update.phase {
        CoreUpdatePhase::Consent => {
            update.phase = CoreUpdatePhase::Downloading;
            app.core_operation_task = Some(tokio::spawn(async move {
                let (progress_tx, mut progress_rx) = tokio::sync::mpsc::channel(8);
                let work = binary::acquire(kind, DownloadAuthorization::confirmed(kind, id), Some(progress_tx));
                tokio::pin!(work);
                loop {
                    tokio::select! {
                        result = &mut work => {
                            if !cancelled.load(Ordering::SeqCst) {
                                let action = match result {
                                    Ok(prepared) => Action::CorePrepared { id, prepared },
                                    Err(error) => Action::CoreUpdateFinished { id, result: Err(error.to_string()) },
                                };
                                let _ = tx.send(action).await;
                            }
                            break;
                        }
                        Some(progress) = progress_rx.recv() => { let _ = tx.send(Action::CoreUpdateProgress(progress)).await; }
                        _ = wait_cancel(&cancelled) => break,
                    }
                }
            }));
        }
        CoreUpdatePhase::Ready => {
            if update.generation != ctx.manager.current_generation() {
                update.phase = CoreUpdatePhase::Failed;
                update.message = "Core ownership/generation changed since checking; retry the operation".into();
                return;
            }
            let Some(prepared) = update.prepared.clone() else {
                return;
            };
            let context = GuidedTunContext {
                id,
                generation: update.generation,
                prepared,
                intent: update.intent,
                enable_tun: app.gui_config.enable_tun_mode.unwrap_or(false),
            };
            if context.intent == CoreIntent::Switch && ctx.manager.state() != crate::app::CoreState::Running {
                // Selecting a stopped core never requests TUN privileges or
                // prepares a profile. The manager rechecks ownership/epoch
                // and commits only the CLI selection and marker.
                start_apply(app, ctx, context);
                return;
            }
            update.phase = CoreUpdatePhase::TunChecking;
            let manager = ctx.manager.clone();
            app.core_operation_task = Some(tokio::spawn(async move {
                let result = check_tun_with(
                    &context,
                    &cancelled,
                    || manager.guided_preflight(context.generation, context.prepared.kind),
                    crate::commands::privilege::running_as_root,
                    crate::commands::privilege::has_tun_capability,
                );
                let _ = tx.send(Action::CoreTunChecked { context, result }).await;
            }));
        }
        CoreUpdatePhase::TunConsent => super::tun::confirm_tun_setup(app),
        CoreUpdatePhase::Failed | CoreUpdatePhase::Success | CoreUpdatePhase::Cancelled => app.overlay = None,
        _ => {}
    }
}

fn check_tun_with(
    context: &GuidedTunContext,
    cancelled: &AtomicBool,
    owner: impl FnOnce() -> anyhow::Result<()>,
    root: impl FnOnce() -> bool,
    capable: impl FnOnce(&std::path::Path) -> bool,
) -> Result<bool, String> {
    owner().map_err(|error| format!("{error:#}"))?;
    if cancelled.load(Ordering::SeqCst) {
        return Err("Core operation cancelled".into());
    }
    Ok((context.enable_tun || context.intent == CoreIntent::TunSetup) && !root() && !capable(&context.prepared.path))
}

fn start_apply(app: &mut App, ctx: &Ctx, context: GuidedTunContext) {
    if context.intent == CoreIntent::TunSetup {
        let message = app.tr("settings.tun_setup_present").to_string();
        app.tun_privileged = true;
        if let Some(update) = app.core_update.as_mut() {
            update.phase = CoreUpdatePhase::Success;
            update.message = message;
        }
        app.overlay = Some(Overlay::CoreUpdate);
        return;
    }
    if !crate::mihomo_manager::core_policy::is_compatible(
        context.prepared.kind.as_str(),
        &context.prepared.version,
        binary::target_version(context.prepared.kind),
    )
    .unwrap_or(false)
    {
        if let Some(update) = app.core_update.as_mut() {
            update.phase = CoreUpdatePhase::Failed;
            update.message = "Unreviewed core version cannot be started through permission setup".into();
        }
        app.overlay = Some(Overlay::CoreUpdate);
        return;
    }
    let Some(update) = app.core_update.as_mut() else {
        return;
    };
    update.phase = CoreUpdatePhase::Preparing;
    let cancelled = update.cancelled.clone();
    let manager = ctx.manager.clone();
    let tx = ctx.tx.for_operation();
    app.core_operation_task = Some(tokio::spawn(async move {
        let (stages, mut stage_rx) = tokio::sync::mpsc::channel(1);
        let work = manager.apply_prepared_core(
            &context.prepared,
            context.generation,
            context.intent != CoreIntent::Switch,
            context.enable_tun,
            &cancelled,
            Some(stages),
        );
        tokio::pin!(work);
        let result = loop {
            tokio::select! {
                result = &mut work => break result.map_err(|error| format!("{error:#}")),
                Some(()) = stage_rx.recv() => { let _ = tx.send(Action::CoreUpdateSwitching { id: context.id }).await; }
            }
        };
        let _ = tx.send(Action::CoreUpdateFinished { id: context.id, result }).await;
    }));
}

fn context_matches(app: &App, ctx: &Ctx, context: &GuidedTunContext, phase: CoreUpdatePhase) -> bool {
    app.core_update.as_ref().is_some_and(|update| {
        update.id == context.id
            && update.phase == phase
            && update.generation == context.generation
            && update.generation == ctx.manager.current_generation()
            && update.kind == context.prepared.kind
            && update.prepared.as_ref() == Some(&context.prepared)
            && update.intent == context.intent
            && !update.cancelled.load(Ordering::SeqCst)
            && context.enable_tun == app.gui_config.enable_tun_mode.unwrap_or(false)
    })
}

fn tun_result(
    app: &mut App,
    ctx: &Ctx,
    context: GuidedTunContext,
    result: Result<bool, String>,
    phase: CoreUpdatePhase,
) {
    let result = result.and_then(|needs_setup| {
        ctx.manager
            .guided_preflight(context.generation, context.prepared.kind)
            .map(|()| needs_setup)
            .map_err(|error| format!("{error:#}"))
    });
    tun_result_with(app, ctx, context, result, phase, start_apply);
}

fn tun_result_with(
    app: &mut App,
    ctx: &Ctx,
    context: GuidedTunContext,
    result: Result<bool, String>,
    phase: CoreUpdatePhase,
    apply: impl FnOnce(&mut App, &Ctx, GuidedTunContext),
) {
    if !context_matches(app, ctx, &context, phase) {
        // A matching active operation whose generation/config changed is visibly failed.
        if app.core_update.as_ref().is_some_and(|update| {
            update.id == context.id && update.phase == phase && !update.cancelled.load(Ordering::SeqCst)
        }) {
            let update = app.core_update.as_mut().unwrap();
            update.phase = CoreUpdatePhase::Failed;
            update.message = "Core ownership/generation or TUN configuration changed during setup; retry".into();
            app.pending_sudo = None;
            app.overlay = Some(Overlay::CoreUpdate);
        }
        return;
    }
    match result {
        Ok(true) => {
            let update = app.core_update.as_mut().unwrap();
            update.phase = CoreUpdatePhase::TunConsent;
            update.message = crate::commands::privilege::missing_capability_error(&context.prepared.path);
            app.pending_sudo = Some(PendingSudoAction::GuidedTunSetup(context));
            app.overlay = Some(Overlay::TunSetupConfirmation);
        }
        Ok(false) => apply(app, ctx, context),
        Err(error) => {
            let update = app.core_update.as_mut().unwrap();
            update.phase = CoreUpdatePhase::Failed;
            update.message = error;
            app.overlay = Some(Overlay::CoreUpdate);
        }
    }
}

pub(super) fn submit_tun_setup(app: &mut App, ctx: &Ctx, context: GuidedTunContext, password: String) {
    let target = context.prepared.clone();
    submit_tun_setup_with(
        app,
        ctx,
        context,
        password,
        move |path, password| {
            binary::validate_permission_target(&target)?;
            crate::commands::privilege::apply_tun_capability_with_password(path, password)
        },
        crate::commands::privilege::require_tun_capability,
        |manager, context| manager.guided_preflight(context.generation, context.prepared.kind),
    );
}

fn submit_tun_setup_with<S, P, O>(
    app: &mut App,
    ctx: &Ctx,
    context: GuidedTunContext,
    password: String,
    setup: S,
    probe: P,
    owner: O,
) where
    S: FnOnce(&std::path::Path, &str) -> anyhow::Result<()> + Send + 'static,
    P: FnOnce(&std::path::Path) -> anyhow::Result<()> + Send + 'static,
    O: Fn(&crate::mihomo_manager::MihomoManager, &GuidedTunContext) -> anyhow::Result<()> + Send + 'static,
{
    if app.overlay != Some(Overlay::PasswordInput) {
        return;
    }
    if !context_matches(app, ctx, &context, CoreUpdatePhase::TunConsent) {
        tun_result_with(
            app,
            ctx,
            context,
            Err("Core setup context changed".into()),
            CoreUpdatePhase::TunConsent,
            |_, _, _| {},
        );
        return;
    }
    let update = app.core_update.as_mut().unwrap();
    update.phase = CoreUpdatePhase::TunSettingUp;
    app.overlay = Some(Overlay::CoreUpdate);
    let cancelled = update.cancelled.clone();
    let manager = ctx.manager.clone();
    let tx = ctx.tx.for_operation();
    app.core_operation_task = Some(tokio::spawn(async move {
        let completion = context.clone();
        let result = tokio::task::spawn_blocking(move || {
            let work = (|| {
                owner(&manager, &context)?;
                if cancelled.load(Ordering::SeqCst) {
                    anyhow::bail!("Core operation cancelled");
                }
                setup(&context.prepared.path, &password)?;
                owner(&manager, &context)?;
                if cancelled.load(Ordering::SeqCst) {
                    anyhow::bail!("Core operation cancelled");
                }
                probe(&context.prepared.path)?;
                Ok(())
            })()
            .map_err(|error: anyhow::Error| format!("{error:#}"));
            (context, work)
        })
        .await;
        let (context, result) =
            result.unwrap_or_else(|error| (completion, Err(format!("TUN setup task failed: {error}"))));
        let _ = tx.send(Action::CoreTunSetupFinished { context, result }).await;
    }));
}

pub(super) fn cancel(app: &mut App) {
    let Some(update) = app.core_update.as_mut() else {
        app.overlay = None;
        return;
    };
    if matches!(
        update.phase,
        CoreUpdatePhase::Failed | CoreUpdatePhase::Success | CoreUpdatePhase::Cancelled
    ) {
        app.overlay = None;
        return;
    }
    update.cancelled.store(true, Ordering::SeqCst);
    app.pending_sudo = None;
    app.password_buffer.clear();
    app.overlay = Some(Overlay::CoreUpdate);
    if matches!(update.phase, CoreUpdatePhase::Preparing | CoreUpdatePhase::Switching) {
        // Keep the visible operation until restoration has completed.
        update.message = "Cancellation requested; waiting for owned rollback".into();
    } else {
        update.phase = CoreUpdatePhase::Cancelled;
        update.message = "TUN/core operation cancelled; current core and configuration unchanged".into();
    }
}

pub(crate) async fn finish_owned_operation(app: &mut App) {
    if let Some(update) = &app.core_update {
        update.cancelled.store(true, Ordering::SeqCst);
    }
    if let Some(task) = app.core_operation_task.take() {
        let _ = task.await;
    }
}

pub(super) fn event(app: &mut App, ctx: &Ctx, action: Action) {
    match action {
        Action::CoreTunChecked { context, result } => {
            tun_result(app, ctx, context, result, CoreUpdatePhase::TunChecking);
            return;
        }
        Action::CoreTunSetupFinished { context, result } => {
            tun_result(app, ctx, context, result.map(|()| false), CoreUpdatePhase::TunSettingUp);
            return;
        }
        _ => {}
    }
    let id = match &action {
        Action::CoreInspected { id, .. }
        | Action::CorePrepared { id, .. }
        | Action::CoreUpdateFinished { id, .. }
        | Action::CoreUpdateSwitching { id } => *id,
        Action::CoreUpdateProgress(progress) => progress.operation_id,
        _ => return,
    };
    let Some(update) = app.core_update.as_mut().filter(|update| update.id == id) else {
        return;
    };
    if update.cancelled.load(Ordering::SeqCst) && !matches!(action, Action::CoreUpdateFinished { .. }) {
        return;
    }
    match action {
        Action::CoreInspected { result, .. } if update.phase == CoreUpdatePhase::Checking => match result {
            CoreInspection::Ready(prepared) if prepared.kind == update.kind => {
                update.prepared = Some(prepared);
                update.phase = CoreUpdatePhase::Ready;
            }
            CoreInspection::NeedsUpdate(request) => {
                update.message = request.diagnostic.clone();
                update.request = Some(request);
                update.phase = CoreUpdatePhase::Consent;
            }
            CoreInspection::Review(message) => {
                update.message = message;
                update.phase = CoreUpdatePhase::Failed;
            }
            _ => {
                update.phase = CoreUpdatePhase::Failed;
                update.message = "Candidate belongs to a different core".into();
            }
        },
        Action::CorePrepared { prepared, .. }
            if prepared.kind == update.kind
                && matches!(update.phase, CoreUpdatePhase::Downloading | CoreUpdatePhase::Verifying) =>
        {
            update.prepared = Some(prepared);
            update.phase = CoreUpdatePhase::Ready;
        }
        Action::CoreUpdateProgress(progress)
            if matches!(update.phase, CoreUpdatePhase::Downloading | CoreUpdatePhase::Verifying) =>
        {
            update.phase = match progress.phase {
                CoreProgressPhase::Verifying => CoreUpdatePhase::Verifying,
                // Ready is published only with the complete validated candidate.
                _ => update.phase,
            };
        }
        Action::CoreUpdateSwitching { .. } if update.phase == CoreUpdatePhase::Preparing => {
            update.phase = CoreUpdatePhase::Switching
        }
        Action::CoreUpdateFinished { result, .. }
            if !matches!(
                update.phase,
                CoreUpdatePhase::Success | CoreUpdatePhase::Failed | CoreUpdatePhase::Cancelled
            ) =>
        {
            update.phase = match &result {
                Ok(()) => CoreUpdatePhase::Success,
                Err(_) if update.cancelled.load(Ordering::SeqCst) => CoreUpdatePhase::Cancelled,
                Err(_) => CoreUpdatePhase::Failed,
            };
            update.message = result.as_ref().err().cloned().unwrap_or_default();
            // Lifecycle failures retain the old runtime when rollback succeeded.
            app.core_state = ctx.manager.state();
            app.core_pid = ctx.manager.pid();
            if result.is_ok() {
                app.gui_config.proxy_core = Some(
                    match ctx.manager.core_kind() {
                        CoreKind::Mihomo => "mihomo",
                        CoreKind::SingBox => "singbox",
                    }
                    .into(),
                );
                if app.core_state == crate::app::CoreState::Running {
                    app.core_version = update.prepared.as_ref().map(|core| core.version.clone());
                    let prepared = update.prepared.clone();
                    ctx.cancel_background();
                    super::lifecycle::note_started(
                        app,
                        ctx,
                        prepared.as_ref().map(|core| core.version.clone()),
                        prepared.as_ref().map(|core| core.path.display().to_string()),
                        prepared.map(|core| core.source),
                    );
                } else {
                    app.core_version = None;
                    app.clear_runtime_caches();
                }
            } else if app.core_state == crate::app::CoreState::Running
                && update.generation != ctx.manager.current_generation()
            {
                // Successful rollback spawned the old executable under a new
                // generation. Re-arm its live streams without losing the error.
                let version = app.core_version.clone();
                let path = ctx.manager.binary_path().map(|path| path.display().to_string());
                ctx.cancel_background();
                super::lifecycle::note_started(app, ctx, version, path, Some("rollback".into()));
            }
            app.overlay = Some(Overlay::CoreUpdate);
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> (Ctx, tokio::sync::mpsc::Receiver<Action>) {
        let (sender, receiver) = tokio::sync::mpsc::channel(16);
        (
            Ctx {
                manager: crate::mihomo_manager::MihomoManager::new(std::env::temp_dir()),
                tx: sender.into(),
                traffic_tx: tokio::sync::watch::channel(None).0,
                log_tx: tokio::sync::mpsc::channel(8).0,
                dropped_logs: Arc::new(std::sync::atomic::AtomicU64::new(0)),
                local_actions: Arc::new(parking_lot::Mutex::new(super::super::LocalActionQueue::new())),
                guard: Arc::new(tokio::sync::Mutex::new(crate::tui::TerminalGuard::detached())),
                keys: crate::tui::keymap::KeyMap::default(),
            },
            receiver,
        )
    }

    fn ready() -> binary::PreparedCore {
        binary::PreparedCore {
            kind: CoreKind::SingBox,
            path: "/fixture/sing-box-v1.14.2".into(),
            version: "1.14.2".into(),
            source: "cached".into(),
        }
    }

    #[tokio::test]
    async fn stopped_switch_confirmation_commits_selection_without_tun_or_runtime_version() {
        let home = tempfile::tempdir().unwrap();
        let _guard = crate::profile_store::store::tests::claim_test_app_home(home.path().to_path_buf()).await;
        std::fs::write(
            home.path().join("verge.yaml"),
            "proxy_core: mihomo\nenable_tun_mode: true\n",
        )
        .unwrap();
        std::fs::write(
            home.path().join("profiles.yaml"),
            "this is deliberately not profile metadata",
        )
        .unwrap();
        let (mut ctx, mut rx) = ctx();
        ctx.manager = crate::mihomo_manager::MihomoManager::new(home.path().to_path_buf())
            .with_socket(home.path().join("private.sock"))
            .with_singbox_controller("127.0.0.1:0".parse().unwrap());
        let mut app = App::new();
        app.gui_config.enable_tun_mode = Some(true);
        begin_with(&mut app, &ctx, CoreKind::SingBox, CoreIntent::Switch, async {
            Ok(CoreInspection::Ready(ready()))
        });
        app.core_operation_task.take().unwrap().await.unwrap();
        event(&mut app, &ctx, rx.recv().await.unwrap());
        confirm(&mut app, &ctx);
        assert_eq!(app.core_update.as_ref().unwrap().phase, CoreUpdatePhase::Preparing);
        app.core_operation_task.take().unwrap().await.unwrap();
        let result = rx.recv().await.unwrap();
        assert!(
            matches!(&result, Action::CoreUpdateFinished { result: Ok(()), .. }),
            "{result:?}"
        );
        event(&mut app, &ctx, result);
        assert_eq!(app.gui_config.proxy_core.as_deref(), Some("singbox"));
        assert_eq!(app.core_state, crate::app::CoreState::Stopped);
        assert!(app.core_pid.is_none());
        assert!(app.core_version.is_none());
        assert!(ctx.manager.binary_path().is_none());
        assert_eq!(ctx.manager.current_generation(), 1);
        assert_eq!(
            app.core_update.as_ref().unwrap().prepared.as_ref().unwrap().version,
            "1.14.2"
        );
        assert_eq!(
            std::fs::read_to_string(home.path().join("profiles.yaml")).unwrap(),
            "this is deliberately not profile metadata"
        );
    }

    #[tokio::test]
    async fn guided_tun_decline_preserves_old_runtime_and_exact_operation() {
        for kind in [CoreKind::Mihomo, CoreKind::SingBox] {
            for intent in [CoreIntent::Start, CoreIntent::Restart, CoreIntent::Switch] {
                let (ctx, mut rx) = ctx();
                let fixture = tempfile::tempdir().unwrap();
                let prepared = binary::PreparedCore {
                    kind,
                    path: fixture.path().join(kind.as_str()),
                    version: binary::target_version(kind).into(),
                    source: "cached".into(),
                };
                let target = prepared.clone();
                let mut app = App::new();
                app.core_state = crate::app::CoreState::Running;
                app.core_pid = Some(42);
                app.gui_config.enable_tun_mode = Some(true);
                app.core_version = Some("old-fixture".into());
                begin_with(&mut app, &ctx, kind, intent, async move {
                    Ok(CoreInspection::Ready(target))
                });
                app.core_operation_task.take().unwrap().await.unwrap();
                event(&mut app, &ctx, rx.recv().await.unwrap());
                let id = app.core_update.as_ref().unwrap().id;
                app.core_update.as_mut().unwrap().phase = CoreUpdatePhase::TunChecking;
                tun_result_with(
                    &mut app,
                    &ctx,
                    GuidedTunContext {
                        id,
                        generation: 0,
                        prepared: prepared.clone(),
                        intent,
                        enable_tun: true,
                    },
                    Ok(true),
                    CoreUpdatePhase::TunChecking,
                    |_, _, _| panic!("missing capability requires consent"),
                );
                super::super::tun::skip_tun_setup_start(&mut app, &ctx.tx).await;
                assert_eq!(
                    app.core_state,
                    crate::app::CoreState::Running,
                    "decline must preserve the old core"
                );
                assert_eq!(app.core_pid, Some(42));
                assert_eq!(app.core_version.as_deref(), Some("old-fixture"));
                assert_eq!(app.gui_config.enable_tun_mode, Some(true));
                let update = app.core_update.as_ref().unwrap();
                assert_eq!(update.id, id);
                assert_eq!(update.prepared.as_ref(), Some(&prepared));
                assert_eq!(update.phase, CoreUpdatePhase::Cancelled);
                assert_eq!(app.overlay, Some(Overlay::CoreUpdate));
                assert!(rx.try_recv().is_err(), "decline never starts a core");
            }
        }
    }

    fn tun_fixture(
        kind: CoreKind,
        intent: CoreIntent,
    ) -> (
        App,
        Ctx,
        tokio::sync::mpsc::Receiver<Action>,
        GuidedTunContext,
        tempfile::TempDir,
    ) {
        let (ctx, rx) = ctx();
        let dir = tempfile::tempdir().unwrap();
        let prepared = binary::PreparedCore {
            kind,
            path: dir.path().join(kind.as_str()),
            version: binary::target_version(kind).into(),
            source: "cached".into(),
        };
        let context = GuidedTunContext {
            id: 7,
            generation: 0,
            prepared: prepared.clone(),
            intent,
            enable_tun: true,
        };
        let mut app = App::new();
        app.core_state = crate::app::CoreState::Running;
        app.core_pid = Some(42);
        app.core_version = Some("old-fixture".into());
        *ctx.manager.inner().resolved_binary.lock() = Some(dir.path().join("old-core"));
        app.traffic_totals = Some((12, 34));
        app.gui_config.enable_tun_mode = Some(true);
        app.core_update = Some(CoreUpdate {
            id: 7,
            generation: 0,
            kind,
            intent,
            phase: CoreUpdatePhase::TunChecking,
            request: None,
            prepared: Some(prepared),
            message: String::new(),
            cancelled: Arc::new(AtomicBool::new(false)),
        });
        (app, ctx, rx, context, dir)
    }

    #[test]
    fn guided_tun_gate_probes_exact_candidate_and_rejects_foreign_owner_first() {
        for kind in [CoreKind::Mihomo, CoreKind::SingBox] {
            let (_app, _ctx, _rx, context, _dir) = tun_fixture(kind, CoreIntent::Restart);
            let cancelled = AtomicBool::new(false);
            assert!(
                check_tun_with(
                    &context,
                    &cancelled,
                    || Ok(()),
                    || false,
                    |path| {
                        assert_eq!(path, context.prepared.path);
                        false
                    }
                )
                .unwrap()
            );
            assert!(
                !check_tun_with(
                    &context,
                    &cancelled,
                    || Ok(()),
                    || true,
                    |_| panic!("root does not probe")
                )
                .unwrap()
            );
            assert!(!check_tun_with(&context, &cancelled, || Ok(()), || false, |_| true).unwrap());
            let mut off = context.clone();
            off.enable_tun = false;
            assert!(!check_tun_with(&off, &cancelled, || Ok(()), || panic!("TUN off"), |_| panic!("TUN off")).unwrap());
            assert!(
                check_tun_with(
                    &context,
                    &cancelled,
                    || anyhow::bail!("GUI owner"),
                    || panic!("owner first"),
                    |_| panic!("owner first")
                )
                .unwrap_err()
                .contains("GUI")
            );
        }
    }

    #[tokio::test]
    async fn guided_tun_setup_success_rechecks_exact_target_and_resumes_once_across_stream_cancel() {
        for kind in [CoreKind::Mihomo, CoreKind::SingBox] {
            for intent in [CoreIntent::Start, CoreIntent::Restart, CoreIntent::Switch] {
                let (mut app, ctx, mut rx, context, _dir) = tun_fixture(kind, intent);
                tun_result_with(
                    &mut app,
                    &ctx,
                    context.clone(),
                    Ok(true),
                    CoreUpdatePhase::TunChecking,
                    |_, _, _| panic!("consent before apply"),
                );
                assert_eq!(app.overlay, Some(Overlay::TunSetupConfirmation));
                let path = context.prepared.path.clone();
                let probed_path = path.clone();
                // Submission before explicit y is ignored, even with a pending exact target.
                submit_tun_setup_with(
                    &mut app,
                    &ctx,
                    context.clone(),
                    "fixture-password".into(),
                    |_, _| panic!("no y"),
                    |_| panic!("no y"),
                    |_, _| Ok(()),
                );
                assert!(app.core_operation_task.is_none());
                super::super::tun::confirm_tun_setup(&mut app);
                app.pending_sudo = None;
                let checks = Arc::new(std::sync::atomic::AtomicU64::new(0));
                let counter = checks.clone();
                submit_tun_setup_with(
                    &mut app,
                    &ctx,
                    context.clone(),
                    "fixture-password".into(),
                    move |target, password| {
                        assert_eq!(target, path);
                        assert_eq!(password, "fixture-password");
                        Ok(())
                    },
                    move |target| {
                        assert_eq!(target, probed_path);
                        Ok(())
                    },
                    move |_, _| {
                        counter.fetch_add(1, Ordering::SeqCst);
                        Ok(())
                    },
                );
                ctx.cancel_background();
                app.core_operation_task.take().unwrap().await.unwrap();
                assert_eq!(checks.load(Ordering::SeqCst), 2);
                let action = ctx.tx.accept(rx.recv().await.unwrap()).unwrap();
                let Action::CoreTunSetupFinished {
                    context: resumed,
                    result,
                } = action
                else {
                    panic!("guided completion required");
                };
                assert_eq!(resumed, context);
                let mut applied = 0;
                tun_result_with(
                    &mut app,
                    &ctx,
                    resumed.clone(),
                    result.map(|()| false),
                    CoreUpdatePhase::TunSettingUp,
                    |app, _, exact| {
                        assert_eq!(exact, context);
                        applied += 1;
                        app.core_update.as_mut().unwrap().phase = CoreUpdatePhase::Preparing;
                    },
                );
                tun_result_with(
                    &mut app,
                    &ctx,
                    resumed,
                    Ok(false),
                    CoreUpdatePhase::TunSettingUp,
                    |_, _, _| applied += 1,
                );
                assert_eq!(applied, 1);
                assert_eq!(app.core_pid, Some(42));
                assert_eq!(app.gui_config.get_valid_proxy_core(), "mihomo");
            }
        }
    }

    #[test]
    fn guided_tun_stale_wrong_target_and_password_cancel_cannot_resume() {
        for changed in 0..7 {
            let (mut app, ctx, _rx, context, _dir) = tun_fixture(CoreKind::SingBox, CoreIntent::Switch);
            app.core_update.as_mut().unwrap().phase = CoreUpdatePhase::TunSettingUp;
            let mut wrong = context.clone();
            match changed {
                0 => wrong.id += 1,
                1 => wrong.generation += 1,
                2 => wrong.prepared.kind = CoreKind::Mihomo,
                3 => wrong.prepared.path = wrong.prepared.path.with_file_name("other"),
                4 => wrong.intent = CoreIntent::Start,
                5 => {
                    ctx.manager.inner().generation.store(1, Ordering::SeqCst);
                }
                _ => wrong.enable_tun = false,
            }
            tun_result_with(
                &mut app,
                &ctx,
                wrong,
                Ok(false),
                CoreUpdatePhase::TunSettingUp,
                |_, _, _| panic!("stale completion must never apply"),
            );
            assert_eq!(app.core_state, crate::app::CoreState::Running);
            assert_eq!(app.core_pid, Some(42));
            assert_eq!(app.gui_config.get_valid_proxy_core(), "mihomo");
            assert_eq!(app.gui_config.enable_tun_mode, Some(true));
        }
        let (mut app, ctx, _rx, context, _dir) = tun_fixture(CoreKind::SingBox, CoreIntent::Switch);
        tun_result_with(
            &mut app,
            &ctx,
            context.clone(),
            Ok(true),
            CoreUpdatePhase::TunChecking,
            |_, _, _| panic!("no consent"),
        );
        super::super::tun::confirm_tun_setup(&mut app);
        app.password_buffer = vec!['x'];
        super::super::tun::handle_password_cancel(&mut app);
        assert_eq!(app.core_state, crate::app::CoreState::Running);
        assert!(app.password_buffer.is_empty());
        assert_eq!(app.overlay, Some(Overlay::CoreUpdate));
        assert_eq!(app.core_update.as_ref().unwrap().phase, CoreUpdatePhase::Cancelled);
        tun_result_with(
            &mut app,
            &ctx,
            context,
            Ok(false),
            CoreUpdatePhase::TunSettingUp,
            |_, _, _| panic!("cancelled"),
        );
    }

    #[tokio::test]
    async fn guided_tun_setup_failure_and_failed_recheck_leave_old_runtime_visible() {
        for fail_probe in [false, true] {
            let (mut app, ctx, mut rx, context, _dir) = tun_fixture(CoreKind::SingBox, CoreIntent::Restart);
            app.core_update.as_mut().unwrap().phase = CoreUpdatePhase::TunConsent;
            app.overlay = Some(Overlay::PasswordInput);
            submit_tun_setup_with(
                &mut app,
                &ctx,
                context,
                "fixture".into(),
                move |_, _| {
                    if fail_probe {
                        Ok(())
                    } else {
                        anyhow::bail!("fixture setup failed")
                    }
                },
                |_| anyhow::bail!("fixture capability still missing"),
                |_, _| Ok(()),
            );
            app.core_operation_task.take().unwrap().await.unwrap();
            let Action::CoreTunSetupFinished { context, result } = rx.recv().await.unwrap() else {
                panic!("guided result");
            };
            tun_result_with(
                &mut app,
                &ctx,
                context,
                result.map(|()| false),
                CoreUpdatePhase::TunSettingUp,
                |_, _, _| panic!("failure never applies"),
            );
            assert_eq!(app.core_update.as_ref().unwrap().phase, CoreUpdatePhase::Failed);
            assert!(app.core_update.as_ref().unwrap().message.contains("fixture"));
            assert_eq!(app.core_state, crate::app::CoreState::Running);
            assert_eq!(app.core_pid, Some(42));
            assert_eq!(app.traffic_totals, Some((12, 34)));
            assert_eq!(app.overlay, Some(Overlay::CoreUpdate));
        }
    }

    #[tokio::test]
    async fn guided_tun_generation_change_or_cancel_during_setup_cannot_resume() {
        for cancellation in [false, true] {
            let (mut app, ctx, mut rx, context, _dir) = tun_fixture(CoreKind::SingBox, CoreIntent::Restart);
            app.core_update.as_mut().unwrap().phase = CoreUpdatePhase::TunConsent;
            app.overlay = Some(Overlay::PasswordInput);
            let token = app.core_update.as_ref().unwrap().cancelled.clone();
            let manager = ctx.manager.clone();
            submit_tun_setup_with(
                &mut app,
                &ctx,
                context,
                "fixture".into(),
                move |_, _| {
                    if cancellation {
                        token.store(true, Ordering::SeqCst);
                    } else {
                        manager.inner().generation.store(1, Ordering::SeqCst);
                    }
                    Ok(())
                },
                |_| panic!("changed operation must not pass to capability recheck"),
                |manager, context| {
                    if manager.current_generation() != context.generation {
                        anyhow::bail!("fixture generation changed");
                    }
                    Ok(())
                },
            );
            app.core_operation_task.take().unwrap().await.unwrap();
            let Action::CoreTunSetupFinished { context, result } = rx.recv().await.unwrap() else {
                panic!("guided result");
            };
            assert!(result.is_err());
            if cancellation {
                cancel(&mut app);
            }
            tun_result_with(
                &mut app,
                &ctx,
                context,
                result.map(|()| false),
                CoreUpdatePhase::TunSettingUp,
                |_, _, _| panic!("changed operation never resumes"),
            );
            assert_eq!(
                app.core_update.as_ref().unwrap().phase,
                if cancellation {
                    CoreUpdatePhase::Cancelled
                } else {
                    CoreUpdatePhase::Failed
                }
            );
            assert_eq!(app.core_state, crate::app::CoreState::Running);
            assert_eq!(app.core_pid, Some(42));
            assert_eq!(app.gui_config.enable_tun_mode, Some(true));
        }
    }

    #[tokio::test]
    async fn guided_tun_setup_worker_panic_reports_persistent_failure_without_password() {
        let (mut app, ctx, mut rx, context, _dir) = tun_fixture(CoreKind::SingBox, CoreIntent::Restart);
        app.core_update.as_mut().unwrap().phase = CoreUpdatePhase::TunConsent;
        app.overlay = Some(Overlay::PasswordInput);
        submit_tun_setup_with(
            &mut app,
            &ctx,
            context,
            "fixture-private-password".into(),
            |_, _| panic!("fixture worker panic"),
            |_| panic!("setup failed"),
            |_, _| Ok(()),
        );
        app.core_operation_task.take().unwrap().await.unwrap();
        let Action::CoreTunSetupFinished { context, result } = rx.recv().await.unwrap() else {
            panic!("failure result required");
        };
        assert!(result.as_ref().unwrap_err().contains("TUN setup task failed"));
        assert!(!result.as_ref().unwrap_err().contains("fixture-private-password"));
        tun_result_with(
            &mut app,
            &ctx,
            context,
            result.map(|()| false),
            CoreUpdatePhase::TunSettingUp,
            |_, _, _| panic!("failure never starts"),
        );
        assert_eq!(app.core_update.as_ref().unwrap().phase, CoreUpdatePhase::Failed);
        assert_eq!(app.overlay, Some(Overlay::CoreUpdate));
        assert_eq!(app.core_pid, Some(42));
    }

    #[test]
    fn guided_tun_settings_setup_only_does_not_modify_runtime_or_selection() {
        let (mut app, ctx, _rx, context, _dir) = tun_fixture(CoreKind::SingBox, CoreIntent::TunSetup);
        app.core_update.as_mut().unwrap().phase = CoreUpdatePhase::TunSettingUp;
        let old_path = ctx.manager.binary_path();
        tun_result_with(
            &mut app,
            &ctx,
            context,
            Ok(false),
            CoreUpdatePhase::TunSettingUp,
            start_apply,
        );
        assert_eq!(app.core_state, crate::app::CoreState::Running);
        assert_eq!(app.core_pid, Some(42));
        assert_eq!(app.core_version.as_deref(), Some("old-fixture"));
        assert_eq!(ctx.manager.binary_path(), old_path);
        assert_eq!(app.traffic_totals, Some((12, 34)));
        assert_eq!(ctx.manager.core_kind(), CoreKind::Mihomo);
        assert_eq!(app.gui_config.get_valid_proxy_core(), "mihomo");
        assert!(app.core_operation_task.is_none());
        assert_eq!(app.core_update.as_ref().unwrap().phase, CoreUpdatePhase::Success);
    }

    #[test]
    fn guided_tun_setup_only_existing_version_cannot_be_used_for_lifecycle() {
        let (mut app, ctx, _rx, mut context, _dir) = tun_fixture(CoreKind::SingBox, CoreIntent::TunSetup);
        context.prepared.version = "1.0.0".into();
        start_apply(&mut app, &ctx, context.clone());
        assert_eq!(app.core_update.as_ref().unwrap().phase, CoreUpdatePhase::Success);
        context.intent = CoreIntent::Start;
        start_apply(&mut app, &ctx, context);
        assert_eq!(app.core_update.as_ref().unwrap().phase, CoreUpdatePhase::Failed);
        assert!(app.core_update.as_ref().unwrap().message.contains("Unreviewed"));
        assert!(app.core_operation_task.is_none());
        assert_eq!(app.core_pid, Some(42));
    }

    #[tokio::test]
    async fn guided_core_cancel_stalled_check_is_responsive_and_preserves_running_state() {
        let (ctx, _rx) = ctx();
        let mut app = App::new();
        app.core_state = crate::app::CoreState::Running;
        app.core_pid = Some(42);
        app.core_version = Some("old-fixture".into());
        begin_with(
            &mut app,
            &ctx,
            CoreKind::SingBox,
            CoreIntent::Switch,
            std::future::pending(),
        );
        tokio::task::yield_now().await;
        assert_eq!(app.core_update.as_ref().unwrap().phase, CoreUpdatePhase::Checking);
        cancel(&mut app);
        finish_owned_operation(&mut app).await;
        assert_eq!(app.core_update.as_ref().unwrap().phase, CoreUpdatePhase::Cancelled);
        assert_eq!(app.core_state, crate::app::CoreState::Running);
        assert_eq!(app.core_pid, Some(42));
        assert_eq!(app.core_version.as_deref(), Some("old-fixture"));
    }

    #[tokio::test]
    async fn guided_core_ready_survives_stream_cancel_and_waits_user_commit_confirmation() {
        let (ctx, mut rx) = ctx();
        let mut app = App::new();
        begin_with(&mut app, &ctx, CoreKind::SingBox, CoreIntent::Switch, async {
            Ok(CoreInspection::Ready(ready()))
        });
        app.core_operation_task.take().unwrap().await.unwrap();
        ctx.cancel_background();
        let result = rx.recv().await.unwrap();
        let accepted = ctx
            .tx
            .accept(result)
            .expect("operation independent of stream generation");
        event(&mut app, &ctx, accepted);
        assert_eq!(app.core_update.as_ref().unwrap().phase, CoreUpdatePhase::Ready);
        assert_eq!(ctx.manager.core_kind(), CoreKind::Mihomo);
        assert_eq!(app.gui_config.get_valid_proxy_core(), "mihomo");
        cancel(&mut app);
        assert_eq!(app.core_update.as_ref().unwrap().phase, CoreUpdatePhase::Cancelled);
    }

    #[tokio::test]
    async fn guided_core_changed_generation_rejects_verified_ready_before_apply() {
        let (ctx, mut rx) = ctx();
        let mut app = App::new();
        begin_with(&mut app, &ctx, CoreKind::SingBox, CoreIntent::Switch, async {
            Ok(CoreInspection::Ready(ready()))
        });
        app.core_operation_task.take().unwrap().await.unwrap();
        event(&mut app, &ctx, rx.recv().await.unwrap());
        ctx.manager.inner().generation.store(1, Ordering::SeqCst);
        confirm(&mut app, &ctx);
        assert_eq!(app.core_update.as_ref().unwrap().phase, CoreUpdatePhase::Failed);
        assert!(app.core_update.as_ref().unwrap().message.contains("generation changed"));
        assert!(app.core_operation_task.is_none());
        assert_eq!(ctx.manager.core_kind(), CoreKind::Mihomo);
    }

    #[tokio::test]
    async fn guided_core_duplicate_and_stale_ready_never_resurrect_cancelled_operation() {
        let (ctx, _rx) = ctx();
        let mut app = App::new();
        begin_with(
            &mut app,
            &ctx,
            CoreKind::SingBox,
            CoreIntent::Switch,
            std::future::pending(),
        );
        let id = app.core_update.as_ref().unwrap().id;
        begin_with(&mut app, &ctx, CoreKind::SingBox, CoreIntent::Switch, async {
            Ok(CoreInspection::Ready(ready()))
        });
        assert_eq!(app.core_update.as_ref().unwrap().id, id);
        cancel(&mut app);
        event(&mut app, &ctx, Action::CorePrepared { id, prepared: ready() });
        event(
            &mut app,
            &ctx,
            Action::CoreInspected {
                id: id + 1,
                result: CoreInspection::Ready(ready()),
            },
        );
        assert!(app.core_update.as_ref().unwrap().prepared.is_none());
        assert_eq!(app.core_update.as_ref().unwrap().phase, CoreUpdatePhase::Cancelled);
        finish_owned_operation(&mut app).await;
    }

    #[tokio::test]
    async fn guided_core_gui_allows_preparation_but_refuses_lifecycle_visibly() {
        let (ctx, mut rx) = ctx();
        let mut app = App::new();
        begin_with(&mut app, &ctx, CoreKind::SingBox, CoreIntent::Switch, async {
            Ok(CoreInspection::Ready(ready()))
        });
        app.core_operation_task.take().unwrap().await.unwrap();
        event(&mut app, &ctx, rx.recv().await.unwrap());
        assert_eq!(app.core_update.as_ref().unwrap().phase, CoreUpdatePhase::Ready);
        assert_eq!(ctx.manager.core_kind(), CoreKind::Mihomo);
        let error = ctx.manager.guided_owner_check(true).unwrap_err();
        let id = app.core_update.as_ref().unwrap().id;
        event(
            &mut app,
            &ctx,
            Action::CoreUpdateFinished {
                id,
                result: Err(error.to_string()),
            },
        );
        assert_eq!(app.overlay, Some(Overlay::CoreUpdate));
        assert_eq!(app.core_update.as_ref().unwrap().phase, CoreUpdatePhase::Failed);
        app.status_msg = Some("unrelated refresh".into());
        assert!(app.core_update.as_ref().unwrap().message.contains("GUI"));
        cancel(&mut app);
        assert!(app.overlay.is_none());
    }

    #[tokio::test]
    async fn guided_core_prepare_failure_keeps_old_running_pid_version_and_data() {
        let (ctx, _rx) = ctx();
        *ctx.manager.inner().state.lock() = crate::app::CoreState::Running;
        *ctx.manager.inner().pid.lock() = Some(42);
        let mut app = App::new();
        app.core_state = crate::app::CoreState::Running;
        app.core_pid = Some(42);
        app.core_version = Some("old-fixture".into());
        app.traffic_totals = Some((12, 34));
        begin_with(
            &mut app,
            &ctx,
            CoreKind::SingBox,
            CoreIntent::Switch,
            std::future::pending(),
        );
        let id = app.core_update.as_ref().unwrap().id;
        event(
            &mut app,
            &ctx,
            Action::CoreUpdateFinished {
                id,
                result: Err("Unsupported dns.fake-ip-range; old core unchanged".into()),
            },
        );
        assert_eq!(app.core_state, crate::app::CoreState::Running);
        assert_eq!(app.core_pid, Some(42));
        assert_eq!(app.core_version.as_deref(), Some("old-fixture"));
        assert_eq!(app.traffic_totals, Some((12, 34)));
        assert_eq!(app.overlay, Some(Overlay::CoreUpdate));
        finish_owned_operation(&mut app).await;
    }
}
