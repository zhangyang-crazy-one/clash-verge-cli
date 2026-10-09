//! Confirmation-gated preparation and owned-core switching. This operation
//! has its own cancellation token and join handle, independent of live traffic.
use super::Ctx;
use crate::app::{Action, App, CoreIntent, CoreUpdate, CoreUpdatePhase, Overlay};
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

fn begin_with<F>(app: &mut App, ctx: &Ctx, kind: CoreKind, intent: CoreIntent, work: F)
where
    F: std::future::Future<Output = anyhow::Result<CoreInspection>> + Send + 'static,
{
    if app.core_operation_task.as_ref().is_some_and(|task| !task.is_finished()) {
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
            let intent = update.intent;
            let enable_tun = app.gui_config.enable_tun_mode.unwrap_or(false);
            update.phase = CoreUpdatePhase::Preparing;
            let manager = ctx.manager.clone();
            app.core_operation_task = Some(tokio::spawn(async move {
                let (stages, mut stage_rx) = tokio::sync::mpsc::channel(1);
                // Do not abort this future on cancel/exit: it owns rollback.
                let work = manager.apply_prepared_core(
                    &prepared,
                    intent != CoreIntent::Switch,
                    enable_tun,
                    &cancelled,
                    Some(stages),
                );
                tokio::pin!(work);
                let result = loop {
                    tokio::select! {
                        result = &mut work => break result.map_err(|error| format!("{error:#}")),
                        Some(()) = stage_rx.recv() => {
                            let _ = tx.send(Action::CoreUpdateSwitching { id }).await;
                        }
                    }
                };
                let _ = tx.send(Action::CoreUpdateFinished { id, result }).await;
            }));
        }
        CoreUpdatePhase::Failed | CoreUpdatePhase::Success | CoreUpdatePhase::Cancelled => app.overlay = None,
        _ => {}
    }
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
    if matches!(update.phase, CoreUpdatePhase::Preparing | CoreUpdatePhase::Switching) {
        // Keep the visible operation until restoration has completed.
        update.message = "Cancellation requested; waiting for owned rollback".into();
    } else {
        update.phase = CoreUpdatePhase::Cancelled;
        update.message.clear();
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
            CoreInspection::Ready(prepared) => {
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
        },
        Action::CorePrepared { prepared, .. }
            if matches!(update.phase, CoreUpdatePhase::Downloading | CoreUpdatePhase::Verifying) =>
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
        Action::CoreUpdateFinished { result, .. } => {
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
                app.core_version = update.prepared.as_ref().map(|core| core.version.clone());
                if app.core_state == crate::app::CoreState::Running {
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
