//! Unlock view: service availability checks through the core.

use crate::app::{Action, App, CoreState};

use super::Ctx;

/// `r`: check every service; ignored while a run is in progress.
pub(super) fn run(app: &mut App, ctx: &Ctx) {
    if app.core_state != CoreState::Running {
        app.status_msg = Some(app.tr("unlock.needs_core").into());
        return;
    }
    if app.unlock.running {
        return;
    }
    app.unlock.running = true;
    app.unlock.error = None;
    let api = ctx.manager.api();
    ctx.spawn_result(
        async move { crate::services::unlock::run(&api, &[]).await },
        Action::UnlockChecked,
        |error: anyhow::Error| Action::UnlockFailed(format!("{error:#}")),
    );
}

pub(super) fn note_report(app: &mut App, report: crate::services::unlock::Report) {
    app.unlock.running = false;
    app.unlock.report = Some(report);
    app.unlock.checked_at = Some(chrono::Local::now());
}
