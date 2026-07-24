// Foundation module — public surface is wired up by Plan 02-03 (CLI
// dispatch + start/stop wiring). The `dead_code` allow covers fields
// and methods that are intentionally unused at the end of Plan 02-01.
#![allow(dead_code)]

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use serde::Serialize;
use tokio::process::{Child, Command};
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
pub struct ManagerInner {
    pub state: Mutex<CoreState>,
    pub child: Mutex<Option<Child>>,
    pub action_tx: Mutex<Option<UnboundedSender<Action>>>,
    pub started_at: Mutex<Option<DateTime<Utc>>>,
    pub restart_history: Mutex<VecDeque<DateTime<Utc>>>,
    pub pid: Mutex<Option<u32>>,
}

impl ManagerInner {
    pub const fn new() -> Self {
        Self {
            state: Mutex::new(CoreState::Stopped),
            child: Mutex::new(None),
            action_tx: Mutex::new(None),
            started_at: Mutex::new(None),
            restart_history: Mutex::new(VecDeque::new()),
            pid: Mutex::new(None),
        }
    }

    /// D-09: 3-in-60s policy. Returns `true` if another auto-restart is
    /// permitted right now, `false` if the cap has been hit.
    pub fn should_auto_restart(&self) -> bool {
        let now = Utc::now();
        let window_start = now - chrono::Duration::seconds(60);

        let mut history = self.restart_history.lock();
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

    /// Stub for Plan 03. Returns `Ok(())` so the watcher can call it
    /// before the actual spawn logic lands.
    #[allow(clippy::unused_async, dead_code)]
    pub async fn try_auto_restart(&self) -> anyhow::Result<()> {
        Ok(())
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
        Self {
            inner: Arc::new(ManagerInner::new()),
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
        self.inner.try_auto_restart().await
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

        let mut command = Command::new(&resolved.path);
        command.arg("-d").arg(&self.config_dir);
        if let Ok(config_path) = clash_verge_core::utils::dirs::clash_path()
            && config_path.exists()
        {
            command.arg("-f").arg(config_path);
        }
        // Keep mihomo I/O off the TTY. Without pipes, core logs overwrite the
        // ratatui alternate screen (visible as garbled home-view output on start).
        let child = command
            .arg("-ext-ctl-unix")
            .arg(&self.socket_path)
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

        {
            let mut state = self.inner.state.lock();
            *state = CoreState::Starting;
        }
        *self.inner.pid.lock() = Some(pid);
        *self.inner.started_at.lock() = Some(Utc::now());

        // Reset state to Running; the watcher will detect crashes
        {
            let mut state = self.inner.state.lock();
            *state = CoreState::Running;
        }

        if let Some(tx) = self.inner.action_tx.lock().as_ref() {
            let _ = tx.send(Action::CoreStarted {
                version: Some(resolved.version.clone()),
                binary_path: Some(resolved.path.display().to_string()),
                binary_source: Some(resolved.source.as_str().into()),
            });
        }

        // Spawn watcher — takes ownership of the Child handle
        let inner = Arc::clone(&self.inner);
        spawn_watcher(child, inner);

        Ok(resolved)
    }

    /// D-10: gracefully stop mihomo.
    ///
    /// Returns Ok(()) even if no child was running (idempotent).
    pub async fn stop(&self) -> anyhow::Result<()> {
        let pid = { *self.inner.pid.lock() };
        let mut child_opt = self.inner.child.lock().take();

        match (pid, child_opt.as_mut()) {
            (Some(pid), Some(child)) => {
                signal::graceful_stop(pid, child).await?;
            }
            _ => { /* already stopped — idempotent */ }
        }

        *self.inner.pid.lock() = None;
        *self.inner.child.lock() = None;
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
