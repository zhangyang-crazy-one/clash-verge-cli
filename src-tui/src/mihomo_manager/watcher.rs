// Foundation module — `spawn_watcher` is wired up by Plan 02-03.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use std::sync::atomic::Ordering;

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
pub fn spawn_watcher(child: Child, inner: Arc<ManagerInner>, config_dir: &Path, socket_path: &Path) -> JoinHandle<()> {
    // The auto-restart path outlives this function, so own the paths.
    let config_dir = config_dir.to_path_buf();
    let socket_path = socket_path.to_path_buf();
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

        *inner.pid.lock() = None;

        if let Some(tx) = inner.action_tx.lock().as_ref() {
            let _ = tx.send(Action::CoreExited(exit_code));
        }

        // Skip auto-restart when stop() intentionally shut down the core.
        if inner.expected_exit.swap(false, Ordering::SeqCst) {
            *inner.state.lock() = CoreState::Stopped;
            return;
        }

        if inner.should_auto_restart() {
            inner.record_restart();
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            if let Err(e) = ManagerInner::try_auto_restart(Arc::clone(&inner), &config_dir, &socket_path).await {
                tracing::error!("auto-restart failed: {e}");
                let msg = format!("exited {exit_code} (restart failed)");
                *inner.state.lock() = CoreState::Error(msg.clone());
                if let Some(tx) = inner.action_tx.lock().as_ref() {
                    let _ = tx.send(Action::CoreError(msg));
                }
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
