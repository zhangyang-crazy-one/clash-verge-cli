// Foundation module — `spawn_watcher` is wired up by Plan 02-03.

use std::sync::Arc;

use tokio::io::{AsyncBufReadExt as _, BufReader};
use tokio::process::Child;
use tokio::task::JoinHandle;

use crate::app::{Action, CoreState};
use crate::mihomo_manager::manager::ManagerInner;

/// D-08: spawn the watcher task that waits for the mihomo child to
/// exit, drains its stdout/stderr into tracing, and emits
/// `Action::CoreExited(code)` via the manager's action channel.
///
/// If the auto-restart policy permits, the watcher also calls
/// `ManagerInner::try_auto_restart` after a small backoff. Otherwise
/// the manager state is transitioned to `Error` and a
/// `Action::CoreError` is sent.
pub fn spawn_watcher(child: Child, inner: Arc<ManagerInner>) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut child = child;
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();

        if let Some(out) = stdout {
            pipe_to_tracing(out, tracing::Level::INFO, "stdout");
        }
        if let Some(err) = stderr {
            pipe_to_tracing(err, tracing::Level::WARN, "stderr");
        }

        let exit_code = match child.wait().await {
            Ok(status) => status.code().unwrap_or(-1),
            Err(e) => {
                tracing::error!("mihomo wait failed: {e}");
                -1
            }
        };

        tracing::info!("mihomo exited with code {exit_code}");

        // Clear the child handle so the manager knows there is no live
        // process to signal on stop.
        *inner.child.lock() = None;
        *inner.pid.lock() = None;

        if let Some(tx) = inner.action_tx.lock().as_ref() {
            let _ = tx.send(Action::CoreExited(exit_code));
        }

        if inner.should_auto_restart() {
            inner.record_restart();
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            if let Err(e) = inner.try_auto_restart().await {
                tracing::error!("auto-restart failed: {e}");
            }
        } else {
            let msg = format!("exited {exit_code}");
            *inner.state.lock() = CoreState::Error(msg.clone());
            if let Some(tx) = inner.action_tx.lock().as_ref() {
                let _ = tx.send(Action::CoreError(msg));
            }
        }
    })
}

fn pipe_to_tracing<R>(reader: R, level: tracing::Level, label: &'static str)
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut lines = BufReader::new(reader).lines();
        loop {
            match lines.next_line().await {
                Ok(Some(line)) => match level {
                    tracing::Level::INFO => tracing::info!(target: "mihomo", "[{label}] {line}"),
                    tracing::Level::WARN => tracing::warn!(target: "mihomo", "[{label}] {line}"),
                    _ => tracing::debug!(target: "mihomo", "[{label}] {line}"),
                },
                Ok(None) => break,
                Err(_) => break,
            }
        }
    });
}
