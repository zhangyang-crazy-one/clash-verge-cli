// Foundation module — public surface is wired up by Plan 02-03 (CLI
// dispatch + start/stop wiring).

use std::collections::VecDeque;
use std::future::Future;
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
    /// Path of the resolved mihomo binary (set on start; None before first
    /// start). Used by TUN capability setup.
    pub resolved_binary: Mutex<Option<PathBuf>>,
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
            resolved_binary: Mutex::new(None),
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
    ///
    /// Single-point TUN preflight (D2): after the binary has been resolved
    /// and before every TUN-enabled spawn we check the file capability.
    /// The check is read-only — no sudo/setcap/askpass here — and root
    /// processes bypass it. This covers CLI start/restart, TUI Start/Restart,
    /// the TUN toggle, watcher auto-restart, and post-upgrade replacement.
    async fn spawn_and_watch(
        resolved: &binary::ResolvedMihomo,
        config_dir: &Path,
        socket_path: &Path,
        inner: Arc<ManagerInner>,
    ) -> anyhow::Result<()> {
        // TUN disabled → no capability needed. If the config cannot be read
        // we assume TUN is off and let mihomo fail on its own if it is not.
        let tun_enabled = runtime_tun_enabled().await.unwrap_or(false);
        preflight_tun_capability(&resolved.path, tun_enabled)?;

        *inner.resolved_binary.lock() = Some(resolved.path.clone());
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

        Self::spawn_and_watch(&resolved, config_dir, socket_path, inner)
            .await
            .context("auto-restart: failed to spawn mihomo")
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
        let socket_path = clash_verge_core::utils::dirs::standalone_socket_path();
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

    /// Path of the resolved mihomo binary, when the core has been started.
    pub fn binary_path(&self) -> Option<std::path::PathBuf> {
        self.inner.resolved_binary.lock().clone()
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

    /// D1: resolve the next mihomo binary and run the read-only TUN
    /// capability preflight BEFORE any lifecycle change. Shared by `start`
    /// and the pre-stop phase of `restart`; failure here must leave the
    /// currently running core untouched. Never invokes sudo/setcap.
    async fn resolve_and_preflight() -> anyhow::Result<binary::ResolvedMihomo> {
        let resolved = binary::resolve_or_install()
            .await
            .context("failed to resolve or auto-install mihomo core")?;
        let tun_enabled = runtime_tun_enabled().await.unwrap_or(false);
        preflight_tun_capability(&resolved.path, tun_enabled)?;
        Ok(resolved)
    }

    /// D-13: spawn mihomo as a child process.
    ///
    /// Prefers a system `verge-mihomo`. Otherwise auto-downloads the managed
    /// mihomo build into the clash-verge-cli data directory.
    ///
    /// Returns details about which binary was used so the UI/CLI can report
    /// install vs reuse clearly.
    pub async fn start(&self) -> anyhow::Result<binary::ResolvedMihomo> {
        let resolved = Self::resolve_and_preflight().await.context("failed to start mihomo")?;

        ManagerInner::spawn_and_watch(&resolved, &self.config_dir, &self.socket_path, Arc::clone(&self.inner))
            .await
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

    /// D-09 restart: validate the replacement core BEFORE stopping the
    /// running one (see [`orchestrate_restart`]).
    ///
    /// Ordering: resolve the binary, run the read-only TUN capability
    /// preflight, and only then stop the old core and spawn the SAME
    /// resolved binary — resolve happens exactly once. A resolve or
    /// capability failure (e.g. a replaced binary that lost its file
    /// capability) returns explicit `tun setup` guidance and leaves the
    /// currently running core untouched. No sudo/setcap here.
    pub async fn restart(&self) -> anyhow::Result<binary::ResolvedMihomo> {
        self.reset_restart_history();
        orchestrate_restart(
            || async {
                binary::resolve_or_install()
                    .await
                    .context("failed to resolve or auto-install mihomo core")
            },
            |resolved| {
                // Own the path so the future does not borrow the closure
                // argument across an await point.
                let path = resolved.path.clone();
                async move {
                    let tun_enabled = runtime_tun_enabled().await.unwrap_or(false);
                    preflight_tun_capability(&path, tun_enabled).context(
                        "TUN capability preflight failed — run `clash-verge-cli tun setup` for the \
resolved binary; the running core was left untouched",
                    )
                }
            },
            || async move { self.stop().await },
            |resolved| {
                let (resolved, config_dir, socket_path, inner) = (
                    resolved.clone(),
                    self.config_dir.clone(),
                    self.socket_path.clone(),
                    Arc::clone(&self.inner),
                );
                async move {
                    ManagerInner::spawn_and_watch(&resolved, &config_dir, &socket_path, inner)
                        .await
                        .context("failed to spawn mihomo")
                }
            },
        )
        .await
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

/// Whether the runtime clash config enables TUN mode.
pub async fn runtime_tun_enabled() -> anyhow::Result<bool> {
    let config = clash_verge_core::config::IClashTemp::new().await.0;
    Ok(config
        .get("tun")
        .and_then(|value| value.as_mapping())
        .and_then(|map| map.get("enable"))
        .and_then(|value| value.as_bool())
        .unwrap_or(false))
}

/// D2 preflight gate: when TUN is enabled, require the resolved binary to
/// carry the TUN capability (unless the process is root). Pure check —
/// never performs sudo/setcap. Runs after binary resolution and before
/// every spawn, so a replaced/upgraded binary loses its capability into
/// this same failure path instead of silently degrading.
fn preflight_tun_capability(path: &Path, tun_enabled: bool) -> anyhow::Result<()> {
    if tun_enabled {
        crate::commands::privilege::require_tun_capability(path)?;
    }
    Ok(())
}

/// Restart orchestration with injectable steps, so the exact ordering and
/// short-circuit behavior can be tested with zero processes, network, or
/// lifecycle side effects. Contract:
///
/// 1. `resolve` — produce the replacement binary; failure aborts here and
///    the currently running core is never touched.
/// 2. `preflight` — read-only TUN capability check on that binary; failure
///    aborts here, again leaving the running core untouched.
/// 3. `stop` — only runs after 1+2 pass. Its error is tolerated: a
///    "not running" stop must not block the replacement spawn (historical
///    restart behavior).
/// 4. `spawn` — receives the SAME binary produced by `resolve`; the binary
///    is never resolved a second time.
///
/// Returns the resolved binary on success.
async fn orchestrate_restart<ResolveFut, PreflightFut, StopFut, SpawnFut>(
    resolve: impl FnOnce() -> ResolveFut,
    preflight: impl FnOnce(&binary::ResolvedMihomo) -> PreflightFut,
    stop: impl FnOnce() -> StopFut,
    spawn: impl FnOnce(&binary::ResolvedMihomo) -> SpawnFut,
) -> anyhow::Result<binary::ResolvedMihomo>
where
    ResolveFut: Future<Output = anyhow::Result<binary::ResolvedMihomo>>,
    PreflightFut: Future<Output = anyhow::Result<()>>,
    StopFut: Future<Output = anyhow::Result<()>>,
    SpawnFut: Future<Output = anyhow::Result<()>>,
{
    let resolved = resolve().await?;
    preflight(&resolved).await?;
    let _ = stop().await;
    spawn(&resolved).await?;
    Ok(resolved)
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

    #[test]
    fn preflight_skips_check_when_tun_disabled() {
        let tmp = std::env::temp_dir().join(format!("cv-preflight-{}.bin", uuid::Uuid::new_v4()));
        let _ = std::fs::write(&tmp, b"x");
        let resolved = binary::ResolvedMihomo {
            path: tmp.clone(),
            source: binary::MihomoBinarySource::ManagedCached,
            version: "v0.0.0".into(),
        };
        // TUN off → spawn allowed regardless of file capabilities.
        assert!(preflight_tun_capability(&resolved.path, false).is_ok());
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn preflight_rejects_uncapped_binary_when_tun_enabled() {
        let tmp = std::env::temp_dir().join(format!("cv-preflight-{}.bin", uuid::Uuid::new_v4()));
        let _ = std::fs::write(&tmp, b"x");
        let resolved = binary::ResolvedMihomo {
            path: tmp.clone(),
            source: binary::MihomoBinarySource::ManagedCached,
            version: "v0.0.0".into(),
        };
        // Root bypasses the check; non-root (the normal CI/user case) must
        // get an actionable error naming the binary and the setup command.
        if crate::commands::privilege::running_as_root() {
            assert!(preflight_tun_capability(&resolved.path, true).is_ok());
        } else {
            let error = preflight_tun_capability(&resolved.path, true)
                .expect_err("uncapped TUN-enabled spawn must fail before spawn")
                .to_string();
            assert!(error.contains("tun setup"), "{error}");
            assert!(error.contains(&tmp.display().to_string()), "{error}");
        }
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn preflight_rejects_replaced_binary_without_capability() {
        // A post-upgrade binary at a new path must hit the same failure.
        let tmp = std::env::temp_dir().join(format!("cv-replaced-{}.bin", uuid::Uuid::new_v4()));
        let _ = std::fs::write(&tmp, b"x");
        let resolved = binary::ResolvedMihomo {
            path: tmp.clone(),
            source: binary::MihomoBinarySource::Downloaded,
            version: "v9.9.9".into(),
        };
        if !crate::commands::privilege::running_as_root() {
            let error = preflight_tun_capability(&resolved.path, true)
                .expect_err("replaced uncapped binary must fail")
                .to_string();
            assert!(error.contains(&tmp.display().to_string()), "{error}");
        }
        let _ = std::fs::remove_file(&tmp);
    }

    // ── restart orchestration seam: pure ordering tests, no process / ──────
    // ── network / lifecycle side effects (never call start/stop/restart) ───

    fn fake_resolved(path: &str) -> binary::ResolvedMihomo {
        binary::ResolvedMihomo {
            path: PathBuf::from(path),
            source: binary::MihomoBinarySource::System,
            version: "v1.2.3".into(),
        }
    }

    /// Stand-in for the `stop` step: records the event and succeeds.
    fn record_ok(
        events: &Arc<Mutex<Vec<&'static str>>>,
        event: &'static str,
    ) -> impl FnOnce() -> std::future::Ready<anyhow::Result<()>> {
        let events = Arc::clone(events);
        move || {
            events.lock().push(event);
            std::future::ready(Ok(()))
        }
    }

    /// Stand-in for the `preflight` step: records the event and succeeds.
    fn record_ok_preflight(
        events: &Arc<Mutex<Vec<&'static str>>>,
        event: &'static str,
    ) -> impl FnOnce(&binary::ResolvedMihomo) -> std::future::Ready<anyhow::Result<()>> {
        let events = Arc::clone(events);
        move |_resolved| {
            events.lock().push(event);
            std::future::ready(Ok(()))
        }
    }

    /// Stand-in for the `spawn` step: records the event and the binary path
    /// it received, then succeeds.
    fn record_ok_spawn(
        events: &Arc<Mutex<Vec<&'static str>>>,
        spawned: &Arc<Mutex<Option<PathBuf>>>,
    ) -> impl FnOnce(&binary::ResolvedMihomo) -> std::future::Ready<anyhow::Result<()>> {
        let events = Arc::clone(events);
        let spawned = Arc::clone(spawned);
        move |resolved| {
            events.lock().push("spawn");
            *spawned.lock() = Some(resolved.path.clone());
            std::future::ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn restart_sequence_is_resolve_preflight_stop_then_spawn() {
        let events: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::new(Vec::new()));
        let spawned: Arc<Mutex<Option<PathBuf>>> = Arc::new(Mutex::new(None));
        let resolved = fake_resolved("/fake/verge-mihomo");

        let result = orchestrate_restart(
            {
                let events = Arc::clone(&events);
                let resolved = resolved.clone();
                move || {
                    events.lock().push("resolve");
                    async move { Ok(resolved) }
                }
            },
            record_ok_preflight(&events, "preflight"),
            record_ok(&events, "stop"),
            record_ok_spawn(&events, &spawned),
        )
        .await;

        result.expect("success path must resolve");
        assert_eq!(*events.lock(), ["resolve", "preflight", "stop", "spawn"]);
        // resolve ran exactly once and spawn reused that same binary.
        assert_eq!(*spawned.lock(), Some(PathBuf::from("/fake/verge-mihomo")));
    }

    #[tokio::test]
    async fn restart_resolve_failure_short_circuits_before_stop() {
        let events: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::new(Vec::new()));
        let spawned: Arc<Mutex<Option<PathBuf>>> = Arc::new(Mutex::new(None));

        let result = orchestrate_restart(
            {
                let events = Arc::clone(&events);
                move || {
                    events.lock().push("resolve");
                    std::future::ready(Err(anyhow::anyhow!("no binary available")))
                }
            },
            record_ok_preflight(&events, "preflight"),
            record_ok(&events, "stop"),
            record_ok_spawn(&events, &spawned),
        )
        .await;

        assert!(result.is_err());
        assert_eq!(
            *events.lock(),
            ["resolve"],
            "resolve failure must short-circuit before preflight/stop/spawn"
        );
        assert!(spawned.lock().is_none());
    }

    #[tokio::test]
    async fn restart_preflight_failure_keeps_old_core_running() {
        let events: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::new(Vec::new()));
        let spawned: Arc<Mutex<Option<PathBuf>>> = Arc::new(Mutex::new(None));
        let resolved = fake_resolved("/fake/verge-mihomo");

        let result = orchestrate_restart(
            {
                let events = Arc::clone(&events);
                let resolved = resolved.clone();
                move || {
                    events.lock().push("resolve");
                    async move { Ok(resolved) }
                }
            },
            {
                let events = Arc::clone(&events);
                move |_resolved| {
                    events.lock().push("preflight");
                    std::future::ready(Err(anyhow::anyhow!("TUN is enabled but the binary lacks capabilities")))
                }
            },
            record_ok(&events, "stop"),
            record_ok_spawn(&events, &spawned),
        )
        .await;

        assert!(result.is_err());
        assert_eq!(
            *events.lock(),
            ["resolve", "preflight"],
            "capability failure must never stop the running core"
        );
        assert!(spawned.lock().is_none());
    }

    #[tokio::test]
    async fn restart_tolerates_stop_failure_and_still_spawns() {
        let events: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::new(Vec::new()));
        let spawned: Arc<Mutex<Option<PathBuf>>> = Arc::new(Mutex::new(None));
        let resolved = fake_resolved("/fake/verge-mihomo");

        let result = orchestrate_restart(
            {
                let events = Arc::clone(&events);
                let resolved = resolved.clone();
                move || {
                    events.lock().push("resolve");
                    async move { Ok(resolved) }
                }
            },
            record_ok_preflight(&events, "preflight"),
            {
                let events = Arc::clone(&events);
                move || {
                    events.lock().push("stop");
                    std::future::ready(Err(anyhow::anyhow!("stop failed")))
                }
            },
            record_ok_spawn(&events, &spawned),
        )
        .await;

        result.expect("stop failure is tolerated and must not block the spawn");
        assert_eq!(*events.lock(), ["resolve", "preflight", "stop", "spawn"]);
    }

    #[tokio::test]
    async fn restart_spawn_failure_propagates_after_stop() {
        let events: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::new(Vec::new()));
        let resolved = fake_resolved("/fake/verge-mihomo");

        let result = orchestrate_restart(
            {
                let events = Arc::clone(&events);
                let resolved = resolved.clone();
                move || {
                    events.lock().push("resolve");
                    async move { Ok(resolved) }
                }
            },
            record_ok_preflight(&events, "preflight"),
            record_ok(&events, "stop"),
            {
                let events = Arc::clone(&events);
                move |_resolved| {
                    events.lock().push("spawn");
                    std::future::ready(Err(anyhow::anyhow!("spawn failed")))
                }
            },
        )
        .await;

        assert!(result.is_err());
        assert_eq!(
            *events.lock(),
            ["resolve", "preflight", "stop", "spawn"],
            "spawn failure comes only after the old core was stopped"
        );
    }
}
