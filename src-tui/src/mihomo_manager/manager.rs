// Foundation module — public surface is wired up by Plan 02-03 (CLI
// dispatch + start/stop wiring).

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use anyhow::Context;
use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use serde::Serialize;
use tokio::process::Command;
use tokio::sync::mpsc::UnboundedSender;

use crate::app::{Action, CoreState};
use crate::mihomo_api::MihomoApi;
use crate::mihomo_manager::{binary, signal, watcher::spawn_watcher};

use std::process::Stdio;

const RESTART_WINDOW: Duration = Duration::from_secs(60);
const MAX_RESTARTS_IN_WINDOW: usize = 3;

/// Shared inner state of the MihomoManager. Cloning the `Arc<ManagerInner>`
/// is cheap and lets background tasks (watcher, auto-restart) safely
/// observe state without holding the manager itself.
///
/// `config_dir`, `socket_path`, and `secret` are owned by `MihomoManager`
/// and passed in as `&Path` references to spawn operations, so they are
/// not duplicated here.
pub struct ManagerInner {
    pub state: Mutex<CoreState>,
    pub action_tx: Mutex<Option<UnboundedSender<Action>>>,
    pub started_at: Mutex<Option<DateTime<Utc>>>,
    pub restart_history: Mutex<VecDeque<DateTime<Utc>>>,
    pub pid: Mutex<Option<u32>>,
    /// Set by `stop()` so the watcher knows this exit was intentional and
    /// should NOT trigger an auto-restart.
    pub expected_exit: AtomicBool,
}

impl ManagerInner {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(CoreState::Stopped),
            action_tx: Mutex::new(None),
            started_at: Mutex::new(None),
            restart_history: Mutex::new(VecDeque::new()),
            pid: Mutex::new(None),
            expected_exit: AtomicBool::new(false),
        }
    }

    /// D-09: 3-in-60s policy. Returns `true` if another auto-restart is
    /// permitted right now, `false` if the cap has been hit.
    ///
    /// This is a pure predicate: expired entries are pruned but the check
    /// is read-only for external callers.
    pub fn should_auto_restart(&self) -> bool {
        let now = Utc::now();
        let window_start = now - chrono::Duration::seconds(60);

        let mut history = self.restart_history.lock();
        // Prune expired entries, then check the cap.
        while let Some(front) = history.front() {
            if *front < window_start {
                history.pop_front();
            } else {
                break;
            }
        }
        history.len() < MAX_RESTARTS_IN_WINDOW
    }

    pub fn record_restart(&self) {
        self.restart_history.lock().push_back(Utc::now());
    }

    pub fn reset_restart_history(&self) {
        self.restart_history.lock().clear();
    }

    /// Spawn a mihomo child from a resolved binary, wire up the watcher,
    /// and update the inner state.  Used by both `start` (initial launch)
    /// and `try_auto_restart` (crash recovery).
    fn spawn_and_watch(
        resolved: &binary::ResolvedMihomo,
        config_dir: &Path,
        socket_path: &Path,
        inner: Arc<ManagerInner>,
    ) -> anyhow::Result<()> {
        let mut command = Command::new(&resolved.path);
        command.arg("-d").arg(config_dir);
        if let Ok(config_path) = clash_verge_core::utils::dirs::clash_path()
            && config_path.exists()
        {
            command.arg("-f").arg(config_path);
        }
        let child = command
            .arg("-ext-ctl-unix")
            .arg(socket_path)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(false)
            .spawn()
            .with_context(|| format!(
                "failed to spawn mihomo from '{}' — check that the file exists, is executable (chmod +x), and is a valid binary. Try: ls -la '{}'",
                resolved.path.display(), resolved.path.display()
            ))?;

        let pid = child.id().expect("child must have PID after spawn");

        *inner.state.lock() = CoreState::Running;
        *inner.pid.lock() = Some(pid);
        *inner.started_at.lock() = Some(Utc::now());
        inner.expected_exit.store(false, std::sync::atomic::Ordering::SeqCst);

        if let Some(tx) = inner.action_tx.lock().as_ref() {
            let _ = tx.send(Action::CoreStarted {
                version: Some(resolved.version.clone()),
                binary_path: Some(resolved.path.display().to_string()),
                binary_source: Some(resolved.source.as_str().into()),
            });
        }

        spawn_watcher(child, inner, config_dir, socket_path);
        Ok(())
    }

    /// Attempt to restart mihomo from the watcher after a crash.
    ///
    /// Resolves the binary (reusing cached managed or system path) and
    /// delegates to [`spawn_and_watch`] so the spawn pipeline is shared
    /// with [`MihomoManager::start`].
    ///
    /// `config_dir` and `socket_path` are passed in by the caller (the
    /// outer `MihomoManager`) — the inner state is shared via the
    /// existing `Arc<ManagerInner>` so restart history, action channel,
    /// and the `expected_exit` flag carry over.
    pub async fn try_auto_restart(
        inner: Arc<ManagerInner>,
        config_dir: &Path,
        socket_path: &Path,
    ) -> anyhow::Result<()> {
        let resolved = binary::resolve_or_install()
            .await
            .context("auto-restart: failed to resolve mihomo binary")?;

        tracing::info!(
            target: "mihomo",
            "auto-restarting mihomo {}",
            resolved.version
        );

        Self::spawn_and_watch(&resolved, config_dir, socket_path, inner).context("auto-restart: failed to spawn mihomo")
    }
}

/// Mihomo process lifecycle manager.
///
/// The struct itself is a thin wrapper around `Arc<ManagerInner>` so the
/// manager can be cloned freely and passed to background tasks.
#[derive(Clone)]
pub struct MihomoManager {
    inner: Arc<ManagerInner>,
    config_dir: PathBuf,
    socket_path: PathBuf,
    secret: String,
}

impl MihomoManager {
    /// Construct a new manager. `socket_path` and `secret` default to the
    /// standard values from D-01/D-02 so a manager built without arguments
    /// is usable for the common case.
    pub fn new(config_dir: PathBuf) -> Self {
        let socket_path = default_socket_path();
        let inner = ManagerInner::new();
        Self {
            inner: Arc::new(inner),
            config_dir,
            socket_path,
            secret: String::new(),
        }
    }

    pub fn with_socket(mut self, socket_path: PathBuf) -> Self {
        self.socket_path = socket_path;
        self
    }

    pub fn with_secret(mut self, secret: String) -> Self {
        self.secret = secret;
        self
    }

    /// Install the action channel sender. Called by the TUI after it has
    /// spawned its action loop. The CLI mode leaves this unset and just
    /// ignores watcher events.
    pub fn set_action_tx(&self, tx: UnboundedSender<Action>) {
        *self.inner.action_tx.lock() = Some(tx);
    }

    pub fn state(&self) -> CoreState {
        self.inner.state.lock().clone()
    }

    pub fn pid(&self) -> Option<u32> {
        *self.inner.pid.lock()
    }

    pub fn uptime(&self) -> Option<chrono::Duration> {
        let started = *self.inner.started_at.lock();
        started.map(|t| Utc::now() - t)
    }

    pub const fn config_dir(&self) -> &PathBuf {
        &self.config_dir
    }

    pub const fn socket_path(&self) -> &PathBuf {
        &self.socket_path
    }

    pub fn inner(&self) -> Arc<ManagerInner> {
        Arc::clone(&self.inner)
    }

    pub fn should_auto_restart(&self) -> bool {
        self.inner.should_auto_restart()
    }

    pub fn record_restart(&self) {
        self.inner.record_restart();
    }

    pub fn reset_restart_history(&self) {
        self.inner.reset_restart_history();
    }

    pub async fn try_auto_restart(&self) -> anyhow::Result<()> {
        ManagerInner::try_auto_restart(Arc::clone(&self.inner), &self.config_dir, &self.socket_path).await
    }

    pub fn set_secret(&mut self, secret: String) {
        self.secret = secret;
    }

    pub fn set_socket_path(&mut self, path: PathBuf) {
        self.socket_path = path;
    }

    /// Build a MihomoApi client targeting this manager's socket with
    /// bearer auth from the configured secret.
    pub fn api(&self) -> MihomoApi {
        MihomoApi::new(self.socket_path.clone(), self.secret.clone())
            .expect("MihomoApi construction failed — secret may contain invalid header characters")
    }

    /// D-13: spawn mihomo as a child process.
    ///
    /// Prefers a system `verge-mihomo`. Otherwise auto-downloads the managed
    /// mihomo build into the clash-verge-cli data directory.
    ///
    /// Returns details about which binary was used so the UI/CLI can report
    /// install vs reuse clearly.
    pub async fn start(&self) -> anyhow::Result<binary::ResolvedMihomo> {
        let resolved = binary::resolve_or_install()
            .await
            .context("failed to resolve or auto-install mihomo core")?;

        ManagerInner::spawn_and_watch(&resolved, &self.config_dir, &self.socket_path, Arc::clone(&self.inner))
            .context("failed to spawn mihomo")?;

        Ok(resolved)
    }

    /// D-10: gracefully stop mihomo.
    ///
    /// Returns Ok(()) even if no child was running (idempotent).
    ///
    /// `start()` moves the `Child` into the exit watcher, so we only have
    /// the PID to signal. `stop()` always uses the by-PID path.
    pub async fn stop(&self) -> anyhow::Result<()> {
        // Set a flag so the watcher knows this was intentional and skips
        // auto-restart.  The flag is cleared by the next successful start.
        self.inner
            .expected_exit
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let pid = { *self.inner.pid.lock() };

        if let Some(pid) = pid {
            signal::graceful_stop_by_pid(pid).await?;
        }
        // No PID — already stopped or never started (idempotent).

        *self.inner.pid.lock() = None;
        {
            let mut state = self.inner.state.lock();
            *state = CoreState::Stopped;
        }

        if let Some(tx) = self.inner.action_tx.lock().as_ref() {
            let _ = tx.send(Action::CoreExited(0));
        }

        Ok(())
    }

    /// D-09 restart: stop + start, resetting the auto-restart counter.
    pub async fn restart(&self) -> anyhow::Result<binary::ResolvedMihomo> {
        self.reset_restart_history();
        // Ignore "not running" from stop
        let _ = self.stop().await;
        self.start().await
    }

    /// Return CoreStatus with live version info if mihomo is running.
    pub async fn status(&self) -> CoreStatus {
        let state = self.state();
        let pid = self.pid();
        let uptime_secs = self.uptime().map(|d| d.num_seconds());
        let socket_path = self.socket_path.clone();
        let config_dir = self.config_dir.clone();

        let version = self.api().version().await.ok().map(|v| v.version);
        // A GUI-owned Mihomo process is not a child of this manager, but its
        // configured controller is still authoritative for CLI status.
        let state = observed_state(state, version.as_deref());

        CoreStatus {
            state,
            pid,
            uptime_secs,
            version,
            socket_path,
            config_dir,
        }
    }
}

fn observed_state(managed_state: CoreState, version: Option<&str>) -> CoreState {
    if version.is_some() {
        CoreState::Running
    } else {
        managed_state
    }
}

/// Public status snapshot returned by `MihomoManager::status()`.
#[derive(Debug, Clone, Serialize)]
pub struct CoreStatus {
    pub state: CoreState,
    pub pid: Option<u32>,
    pub uptime_secs: Option<i64>,
    pub version: Option<String>,
    pub socket_path: PathBuf,
    pub config_dir: PathBuf,
}

fn default_socket_path() -> PathBuf {
    if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR") {
        PathBuf::from(runtime)
            .join("clash-verge")
            .join("external-controller.sock")
    } else {
        PathBuf::from("/tmp/clash-verge/external-controller.sock")
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn test_new_manager_is_stopped_no_pid() {
        let mgr = MihomoManager::new(PathBuf::from("/tmp/cfg"));
        assert_eq!(mgr.state(), CoreState::Stopped);
        assert_eq!(mgr.pid(), None);
        assert_eq!(mgr.uptime(), None);
    }

    #[test]
    fn test_should_auto_restart_policy() {
        let mgr = MihomoManager::new(PathBuf::from("/tmp/cfg"));
        assert!(mgr.should_auto_restart());

        let now = Utc::now();
        {
            let mut history = mgr.inner.restart_history.lock();
            for _ in 0..MAX_RESTARTS_IN_WINDOW {
                history.push_back(now);
            }
        }
        assert!(!mgr.should_auto_restart());

        mgr.reset_restart_history();
        assert!(mgr.should_auto_restart());
    }

    #[test]
    fn live_controller_overrides_an_unmanaged_stopped_state() {
        assert_eq!(
            observed_state(CoreState::Stopped, Some("Mihomo v1")),
            CoreState::Running
        );
        assert_eq!(observed_state(CoreState::Stopped, None), CoreState::Stopped);
    }
}
