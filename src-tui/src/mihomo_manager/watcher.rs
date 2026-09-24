// Foundation module — `spawn_watcher` is wired up by Plan 02-03.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use std::sync::atomic::Ordering;

use tokio::io::{AsyncBufReadExt as _, BufReader};
use tokio::process::Child;
use tokio::task::JoinHandle;

use super::manager::{ExitDisposition, classify_exit};
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
pub fn spawn_watcher(
    child: Child,
    inner: Arc<ManagerInner>,
    config_dir: &Path,
    socket_path: &Path,
    spawned_gen: u64,
) -> JoinHandle<()> {
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

        let exited_pid = inner.pid.lock().take();
        inner.owns_child.store(false, Ordering::SeqCst);
        // Intended when this process's stop() asked for it, or when another
        // process (`clash-verge-cli stop`, a TUI) recorded a stop intent for
        // this pid before signalling it.
        let mut expected = inner.expected_exit.swap(false, Ordering::SeqCst);
        if let Some(pid) = exited_pid {
            use crate::mihomo_manager::pidfile;
            expected |= pidfile::take_stop_intent(&pidfile::stop_intent_path_for(&socket_path), pid);
            pidfile::remove_if(&pidfile::path_for(&socket_path), pid);
        }
        if expected {
            *inner.state.lock() = CoreState::Stopped;
        }

        if let Some(tx) = inner.action_tx.lock().as_ref() {
            let _ = tx.send(Action::CoreExited(exit_code));
        }

        // Combined exit classification: owner main's legacy `expected_exit`
        // bool + cross-process pidfile intent (already set state to
        // Stopped above when `expected` is true) is merged with the
        // sing-box branch's generation-race classifier (task 3.1). Either
        // path that says "intentional" must skip the auto-restart; the
        // classifier additionally guards against a stale watcher from a
        // superseded spawn resurrecting itself.
        if expected {
            // Legacy path: this process's stop() set the bool, or another
            // process (a TUI, `clash-verge-cli stop`) wrote a stop intent
            // matching this pid. State was already set to Stopped above.
            return;
        }
        match super::manager::classify_exit(
            spawned_gen,
            inner.generation.load(Ordering::SeqCst),
            inner.expected_exit_gen.load(Ordering::SeqCst),
        ) {
            ExitDisposition::StaleWatchedOver => return,
            ExitDisposition::IntentionalStop => {
                // Generation-only intentional (our own stop() armed
                // expected_exit_gen but no bool/pidfile path matched this
                // exit). The state was NOT set above.
                *inner.state.lock() = CoreState::Stopped;
                return;
            }
            ExitDisposition::Crash => {}
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
                crate::sys_proxy::release_on_core_stop().await;
                *inner.state.lock() = CoreState::Error(msg.clone());
                if let Some(tx) = inner.action_tx.lock().as_ref() {
                    let _ = tx.send(Action::CoreError(msg));
                }
            }
        } else {
            let msg = format!("exited {exit_code}");
            crate::sys_proxy::release_on_core_stop().await;
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
                    tracing::Level::INFO => {
                        tracing::info!(target: crate::logging::MIHOMO_OUTPUT_TARGET, "[{label}] {line}")
                    }
                    tracing::Level::WARN => {
                        tracing::warn!(target: crate::logging::MIHOMO_OUTPUT_TARGET, "[{label}] {line}")
                    }
                    _ => tracing::debug!(target: crate::logging::MIHOMO_OUTPUT_TARGET, "[{label}] {line}"),
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
