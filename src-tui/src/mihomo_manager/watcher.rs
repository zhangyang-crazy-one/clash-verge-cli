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
                // Keep the underlying error (which carries the `tun setup`
                // guidance when the preflight rejects the binary) so the
                // user sees how to recover, not just the exit code.
                let msg = auto_restart_failure_message(exit_code, &e);
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

/// User-visible message when an automatic restart fails: keeps the exit
/// code AND the underlying error, so preflight rejections (which carry the
/// `tun setup` guidance) reach the user instead of being logged only.
fn auto_restart_failure_message(exit_code: i32, error: &anyhow::Error) -> String {
    format!("exited {exit_code} (auto-restart failed: {error})")
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_restart_failure_message_keeps_the_underlying_error() {
        // The preflight rejection carries the `tun setup` guidance; the
        // user-visible auto-restart failure must preserve it.
        let error = anyhow::anyhow!(
            "TUN is enabled but '/x/verge-mihomo' lacks cap_net_admin,cap_net_raw+eip.\nRun: clash-verge-cli tun setup (or use the TUI Settings → TUN setup action), then start again."
        );
        let msg = auto_restart_failure_message(137, &error);
        assert!(msg.contains("exited 137"), "{msg}");
        assert!(msg.contains("tun setup"), "{msg}");
        assert!(msg.contains("/x/verge-mihomo"), "{msg}");
    }

    #[test]
    fn auto_restart_failure_message_uses_the_descriptive_format() {
        let error = anyhow::anyhow!("cannot run getcap: No such file or directory");
        let msg = auto_restart_failure_message(-1, &error);
        assert!(msg.contains("auto-restart failed"), "{msg}");
        assert!(msg.contains("cannot run getcap"), "{msg}");
    }
}
