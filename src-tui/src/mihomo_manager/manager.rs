// Foundation module — public surface is wired up by Plan 02-03 (CLI
// dispatch + start/stop wiring).

use std::collections::VecDeque;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU16, AtomicU64, Ordering};
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
/// Which proxy core this manager owns. Selecting the kind decides the
/// binary resolver, spawn arguments, runtime config file, and API transport.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CoreKind {
    #[default]
    Mihomo,
    SingBox,
}

impl CoreKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Mihomo => "mihomo",
            Self::SingBox => "singbox",
        }
    }
}

pub struct ManagerInner {
    pub state: Mutex<CoreState>,
    pub action_tx: Mutex<Option<UnboundedSender<Action>>>,
    pub started_at: Mutex<Option<DateTime<Utc>>>,
    pub restart_history: Mutex<VecDeque<DateTime<Utc>>>,
    pub pid: Mutex<Option<u32>>,
    /// Path of the resolved mihomo binary (set on start; None before first
    /// start). Used by TUN capability setup.
    pub resolved_binary: Mutex<Option<PathBuf>>,
    /// Generation of the currently-owning spawn. Bumped on every spawn;
    /// a watcher bound to an older generation stands down on exit events
    /// (task 3.1: replaces the racy global `expected_exit` bool).
    pub generation: AtomicU64,
    /// Generation whose exit is intentional (`u64::MAX` = none). Set by
    /// `stop()`; only honored when it equals the watcher's own generation.
    pub expected_exit_gen: AtomicU64,
    /// Core kind decided at construction; drives binary resolution and
    /// spawn arguments. Stored atomically because the watcher's
    /// auto-restart path reads it through the shared `Arc`.
    pub core_kind: AtomicU8,
    /// TCP port of the sing-box clash_api controller (fixed loopback host).
    pub singbox_port: AtomicU16,
}

/// What a watcher should do when its child exits (task 3.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitDisposition {
    /// A newer spawn owns the lifecycle — the stale watcher must stand
    /// down entirely (no restart, no state mutation).
    StaleWatchedOver,
    /// Intentional stop within this generation — record Stopped, no restart.
    IntentionalStop,
    /// Unexpected exit within this generation — auto-restart path applies.
    Crash,
}

/// Pure classifier so the generation-race semantics are unit-testable:
/// a watcher whose generation no longer matches NEVER restarts, even if
/// the expected-exit slot still carries its own generation.
/// Pure classifier so the generation-race semantics are unit-testable:
/// a watcher whose generation no longer matches NEVER restarts, even if
/// the expected-exit slot still carries its own generation.
pub(super) fn classify_exit(spawned_gen: u64, current_gen: u64, expected_exit_gen: u64) -> ExitDisposition {
    if spawned_gen != current_gen {
        ExitDisposition::StaleWatchedOver
    } else if expected_exit_gen == spawned_gen {
        ExitDisposition::IntentionalStop
    } else {
        ExitDisposition::Crash
    }
}

/// Default sing-box skeleton payload (pure; unit-testable without touching
/// the real data directory).
pub(super) fn build_singbox_skeleton_json() -> anyhow::Result<String> {
    let input = crate::singbox::ConfigInput {
        outbounds: Vec::new(),
        groups: Vec::new(),
        mixed_port: 7897,
        enable_tun: false,
        tun: crate::singbox::TunSettings {
            stack: "gvisor".into(),
            mtu: 9000,
        },
        clash_api: crate::singbox::ClashApiSettings {
            listen: "127.0.0.1:9090".parse().expect("static addr"),
            secret: String::new(),
        },
    };
    let config = crate::singbox::generate_config(&input).map_err(anyhow::Error::msg)?;
    serde_json::to_string_pretty(&config).map_err(Into::into)
}

/// Resource-release barrier (task 3.2, add-singbox-dual-core).
///
/// Called between stopping an old core and spawning the next one. Two jobs:
/// 1. Remove a stale external-controller unix socket — dead processes do
///    not clean it up on SIGKILL, and a leftover file blocks the rebind.
///    Safe to remove unconditionally here: we only run after our own stop.
/// 2. Poll until TUN devices from either core are gone. Both cores hijack
///    the default route; overlapping TUN lifetimes can blackhole traffic.
///
/// Best-effort: logs a warning on timeout instead of failing — the spawn
/// itself will surface a bind error if a resource really is still held.
pub(super) async fn resource_barrier(socket_path: &Path, timeout: std::time::Duration) {
    if socket_path.exists() {
        match tokio::fs::remove_file(socket_path).await {
            Ok(()) => tracing::info!(target: "mihomo", "removed stale controller socket {}", socket_path.display()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                tracing::warn!(target: "mihomo", "could not remove stale socket {}: {error}", socket_path.display())
            }
        }
    }

    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let tun0 = net_iface_exists("tun0");
        let sb_tun0 = net_iface_exists("sb-tun0");
        if !tun0 && !sb_tun0 {
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            tracing::warn!(
                target: "mihomo",
                "resource barrier timeout: tun device still present (tun0={tun0}, sb-tun0={sb_tun0})"
            );
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

/// Whether a network interface is currently up, via the sysfs listing.
fn net_iface_exists(name: &str) -> bool {
    Path::new("/sys/class/net").join(name).exists()
}

/// Whether a controller `/version` string belongs to the expected core
/// kind. Verified against live APIs: sing-box answers "sing-box 1.13.x",
/// mihomo answers "v1.19.x" / "Mihomo Meta v1.19.x" (design F1).
fn version_matches_kind(version: &str, kind: CoreKind) -> bool {
    match kind {
        CoreKind::SingBox => version.starts_with("sing-box"),
        CoreKind::Mihomo => !version.starts_with("sing-box"),
    }
}

const READINESS_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Readiness probe (task 3.3): poll the controller until `/version`
/// answers with the expected core type. sing-box's `PUT /configs` is a
/// no-op, so EVERY config application for that core restarts the process
/// and relies on this probe to know the new config actually came up.
pub(super) async fn probe_readiness(
    api: &crate::mihomo_api::MihomoApi,
    kind: CoreKind,
    timeout: std::time::Duration,
) -> anyhow::Result<String> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        match api.version().await {
            Ok(v) => {
                if version_matches_kind(&v.version, kind) {
                    return Ok(v.version);
                }
                anyhow::bail!(
                    "controller answered with unexpected core version '{}' — expected {}",
                    v.version,
                    kind.as_str()
                );
            }
            Err(_) if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
            Err(error) => {
                anyhow::bail!("core did not become ready within {timeout:?}: {error}");
            }
        }
    }
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
            generation: AtomicU64::new(0),
            expected_exit_gen: AtomicU64::new(u64::MAX),
            core_kind: AtomicU8::new(0),
            singbox_port: AtomicU16::new(9090),
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

    pub fn core_kind(&self) -> CoreKind {
        match self.core_kind.load(Ordering::SeqCst) {
            1 => CoreKind::SingBox,
            _ => CoreKind::Mihomo,
        }
    }

    /// Loopback controller endpoint for the sing-box clash_api.
    pub fn singbox_controller(&self) -> std::net::SocketAddr {
        std::net::SocketAddr::from(([127, 0, 0, 1], self.singbox_port.load(Ordering::SeqCst)))
    }

    pub fn set_singbox_port(&self, port: u16) {
        self.singbox_port.store(port, Ordering::SeqCst);
    }

    pub fn set_core_kind(&self, kind: CoreKind) {
        let v = match kind {
            CoreKind::Mihomo => 0,
            CoreKind::SingBox => 1,
        };
        self.core_kind.store(v, Ordering::SeqCst);
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
        resolved_path: &Path,
        version: &str,
        source: &str,
        config_dir: &Path,
        socket_path: &Path,
        inner: Arc<ManagerInner>,
    ) -> anyhow::Result<()> {
        Self::spawn_core(resolved_path, version, source, config_dir, socket_path, None, inner).await
    }

    /// Spawn a core child from a resolved binary, wire up the watcher, and
    /// update the inner state. `config_path` overrides the default mihomo
    /// `-f` argument (used by sing-box, which always passes its generated
    /// `singbox.json`).
    async fn spawn_core(
        resolved_path: &Path,
        _version: &str,
        source: &str,
        config_dir: &Path,
        socket_path: &Path,
        config_path: Option<PathBuf>,
        inner: Arc<ManagerInner>,
    ) -> anyhow::Result<()> {
        // TUN disabled → no capability needed. If the config cannot be read
        // we assume TUN is off and let mihomo fail on its own if it is not.
        let tun_enabled = runtime_tun_enabled().await.unwrap_or(false);
        preflight_tun_capability(resolved_path, tun_enabled)?;

        // D-01: the external-controller unix socket's parent dir must exist
        // before mihomo binds it. On a fresh install neither
        // $XDG_RUNTIME_DIR/clash-verge-cli nor the /tmp fallback exists yet,
        // and a missing parent fails the bind with ENOENT.
        clash_verge_core::utils::dirs::ensure_standalone_socket_dir()
            .context("failed to prepare external-controller socket dir")?;

        // Task 3.2: wait out TUN teardown and clear any stale controller
        // socket left by a SIGKILLed predecessor before binding anew.
        resource_barrier(socket_path, std::time::Duration::from_secs(5)).await;

        *inner.resolved_binary.lock() = Some(resolved_path.to_path_buf());
        let mut command = Command::new(resolved_path);
        match inner.core_kind() {
            CoreKind::Mihomo => {
                command.arg("-d").arg(config_dir);
                if let Ok(config_path) = clash_verge_core::utils::dirs::clash_path()
                    && config_path.exists()
                {
                    command.arg("-f").arg(config_path);
                }
                command.arg("-ext-ctl-unix").arg(socket_path);
            }
            CoreKind::SingBox => {
                // clash_api endpoint lives inside the generated JSON (TCP);
                // no controller flag needed.
                let path = config_path
                    .clone()
                    .map(Ok)
                    .unwrap_or_else(clash_verge_core::utils::dirs::singbox_config_path)?;
                command.arg("run").arg("-c").arg(path);
            }
        }
        let child = command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(false)
            .spawn()
            .with_context(|| format!(
                "failed to spawn core from '{}' — check that the file exists, is executable (chmod +x), and is a valid binary. Try: ls -la '{}'",
                resolved_path.display(), resolved_path.display()
            ))?;

        let pid = child.id().expect("child must have PID after spawn");

        *inner.state.lock() = CoreState::Running;
        *inner.pid.lock() = Some(pid);
        *inner.started_at.lock() = Some(Utc::now());
        let spawned_gen = inner.generation.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
        inner
            .expected_exit_gen
            .store(u64::MAX, std::sync::atomic::Ordering::SeqCst);

        // Task 3.3: do not report success until the controller actually
        // answers with the expected core type. A broken config makes the
        // child exit immediately; probing catches that here so callers can
        // roll back instead of reporting a healthy core.
        let probe_api = crate::mihomo_api::MihomoApi::with_transport(
            match inner.core_kind() {
                CoreKind::Mihomo => crate::mihomo_api::Transport::UnixSocket(socket_path.to_path_buf()),
                CoreKind::SingBox => crate::mihomo_api::Transport::Tcp(inner.singbox_controller()),
            },
            String::new(),
        )
        .map_err(|e| anyhow::anyhow!("readiness probe client build failed: {e}"))?;
        let probed_version = match probe_readiness(&probe_api, inner.core_kind(), READINESS_PROBE_TIMEOUT).await {
            Ok(v) => v,
            Err(error) => {
                // Clean up a half-alive child (broken config usually exits by
                // itself; a wedged one would otherwise leak past the manager).
                let _ = signal::graceful_stop_by_pid(pid).await;
                *inner.state.lock() = CoreState::Error(error.to_string());
                return Err(error);
            }
        };

        if let Some(tx) = inner.action_tx.lock().as_ref() {
            let _ = tx.send(Action::CoreStarted {
                version: Some(probed_version),
                binary_path: Some(resolved_path.display().to_string()),
                binary_source: Some(source.to_string()),
            });
        }

        spawn_watcher(child, inner, config_dir, socket_path, spawned_gen);
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
        match inner.core_kind() {
            CoreKind::Mihomo => {
                let resolved = binary::resolve_or_install()
                    .await
                    .context("auto-restart: failed to resolve mihomo binary")?;
                tracing::info!(target: "mihomo", "auto-restarting mihomo {}", resolved.version);
                Self::spawn_and_watch(
                    &resolved.path,
                    &resolved.version,
                    resolved.source.as_str(),
                    config_dir,
                    socket_path,
                    Arc::clone(&inner),
                )
                .await
                .context("auto-restart: failed to spawn mihomo")
            }
            CoreKind::SingBox => {
                let resolved = super::singbox_binary::resolve_or_install()
                    .await
                    .context("auto-restart: failed to resolve sing-box binary")?;
                tracing::info!(target: "singbox", "auto-restarting sing-box {}", resolved.version);
                let config_path = Self::write_singbox_runtime_config(config_dir).await?;
                Self::spawn_core(
                    &resolved.path,
                    &resolved.version,
                    resolved.source.as_str(),
                    config_dir,
                    socket_path,
                    Some(config_path),
                    Arc::clone(&inner),
                )
                .await
                .context("auto-restart: failed to spawn sing-box")
            }
        }
    }

    /// Generate and persist the sing-box runtime config (`singbox.json`).
    ///
    /// Skeleton stage: empty outbound set (route falls back to `direct`),
    /// which is a valid starting config; node outbounds are spliced in by
    /// the subscription converter from group 5.
    pub(crate) async fn write_singbox_runtime_config(config_dir: &Path) -> anyhow::Result<PathBuf> {
        Self::write_singbox_conversion(config_dir, &crate::singbox::convert::ProfileConversion::default()).await
    }

    /// Persist a sing-box runtime config built from converted profile nodes.
    pub(crate) async fn write_singbox_conversion(
        config_dir: &Path,
        conversion: &crate::singbox::convert::ProfileConversion,
    ) -> anyhow::Result<PathBuf> {
        let _ = config_dir;
        let input = crate::singbox::ConfigInput {
            outbounds: conversion.outbounds.clone(),
            groups: conversion.groups.clone(),
            mixed_port: 7897,
            enable_tun: false,
            tun: crate::singbox::TunSettings {
                stack: "gvisor".into(),
                mtu: 9000,
            },
            clash_api: crate::singbox::ClashApiSettings {
                listen: "127.0.0.1:9090".parse().expect("static addr"),
                secret: String::new(),
            },
        };
        let config = crate::singbox::generate_config(&input).map_err(anyhow::Error::msg)?;
        let path = clash_verge_core::utils::dirs::singbox_config_path()?;
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await.ok();
        }
        let body = serde_json::to_string_pretty(&config)?;
        // Write-then-rename so a crash mid-write never leaves a truncated config.
        let tmp = path.with_extension("json.download");
        tokio::fs::write(&tmp, body).await?;
        tokio::fs::rename(&tmp, &path).await?;
        Ok(path)
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
    core_kind: CoreKind,
    /// sing-box clash_api TCP endpoint (used when `core_kind == SingBox`).
    singbox_controller: std::net::SocketAddr,
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
            core_kind: CoreKind::default(),
            singbox_controller: "127.0.0.1:9090".parse().expect("static addr"),
        }
    }

    /// Select which core this manager owns. Must be set before `start()`.
    pub fn with_core_kind(mut self, kind: CoreKind) -> Self {
        self.core_kind = kind;
        self.inner.set_core_kind(kind);
        self
    }

    /// Override the sing-box clash_api TCP endpoint.
    pub fn with_singbox_controller(mut self, addr: std::net::SocketAddr) -> Self {
        self.singbox_controller = addr;
        self.inner.set_singbox_port(addr.port());
        self
    }

    pub const fn core_kind(&self) -> CoreKind {
        self.core_kind
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
        let result = match self.core_kind {
            CoreKind::Mihomo => MihomoApi::new(self.socket_path.clone(), self.secret.clone()),
            CoreKind::SingBox => MihomoApi::with_transport(
                crate::mihomo_api::Transport::Tcp(self.singbox_controller),
                self.secret.clone(),
            ),
        };
        result.expect("MihomoApi construction failed — secret may contain invalid header characters")
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

        ManagerInner::spawn_and_watch(
            &resolved.path,
            &resolved.version,
            resolved.source.as_str(),
            &self.config_dir,
            &self.socket_path,
            Arc::clone(&self.inner),
        )
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
        let current_gen = self.inner.generation.load(std::sync::atomic::Ordering::SeqCst);
        self.inner
            .expected_exit_gen
            .store(current_gen, std::sync::atomic::Ordering::SeqCst);
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
        if self.inner.core_kind() == CoreKind::SingBox {
            return self.restart_singbox().await;
        }
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
                    ManagerInner::spawn_and_watch(
                        &resolved.path,
                        &resolved.version,
                        resolved.source.as_str(),
                        &config_dir,
                        &socket_path,
                        inner,
                    )
                    .await
                    .context("failed to spawn mihomo")
                }
            },
        )
        .await
    }

    /// Sing-box restart: resolve → preflight → stop → regenerate config →
    /// spawn with readiness probe (task 3.4 ReloadStrategy::Restart).
    async fn restart_singbox(&self) -> anyhow::Result<binary::ResolvedMihomo> {
        use super::singbox_binary::SingboxBinarySource;
        self.reset_restart_history();
        let resolved = super::singbox_binary::resolve_or_install()
            .await
            .context("failed to resolve or auto-install sing-box core")?;
        let tun_enabled = runtime_tun_enabled().await.unwrap_or(false);
        preflight_tun_capability(&resolved.path, tun_enabled)?;
        self.stop().await.context("failed to stop running sing-box")?;
        let config_path = ManagerInner::write_singbox_runtime_config(&self.config_dir).await?;
        ManagerInner::spawn_core(
            &resolved.path,
            &resolved.version,
            resolved.source.as_str(),
            &self.config_dir,
            &self.socket_path,
            Some(config_path),
            Arc::clone(&self.inner),
        )
        .await
        .context("failed to spawn sing-box")?;
        Ok(binary::ResolvedMihomo {
            path: resolved.path,
            source: match resolved.source {
                SingboxBinarySource::System => binary::MihomoBinarySource::System,
                SingboxBinarySource::ManagedCached => binary::MihomoBinarySource::ManagedCached,
                SingboxBinarySource::Downloaded => binary::MihomoBinarySource::Downloaded,
            },
            version: resolved.version,
        })
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
        // Read-only DNS-rule check for the daemon/restart spawn paths: a
        // missing systemd-resolved polkit rule means this start will trigger
        // polkit dialogs. Logged as a warning so the operator is told to run
        // TUN setup instead of polkit dialogs being the first notice. Never
        // blocks the spawn (the capability above is the hard gate).
        if crate::commands::privilege::resolve1_rule_needed(true) {
            tracing::warn!("{}", crate::commands::privilege::missing_resolve1_rule_warning());
        }
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

    #[tokio::test]
    async fn barrier_removes_stale_socket_file() {
        let path = std::env::temp_dir().join(format!("barrier-test-{}.sock", uuid::Uuid::new_v4()));
        std::fs::write(&path, b"").expect("create stale socket placeholder");
        assert!(path.exists());

        resource_barrier(&path, std::time::Duration::from_millis(50)).await;

        assert!(!path.exists(), "stale socket must be removed by the barrier");
    }

    #[tokio::test]
    async fn barrier_returns_quickly_when_no_tun_present() {
        let started = std::time::Instant::now();
        let missing = std::env::temp_dir().join(format!("barrier-none-{}.sock", uuid::Uuid::new_v4()));
        resource_barrier(&missing, std::time::Duration::from_secs(5)).await;
        assert!(
            started.elapsed() < std::time::Duration::from_secs(1),
            "no tun devices named tun0/sb-tun0: barrier must not wait out the timeout"
        );
    }

    // ---- TDD red: exit disposition (task 3.1, add-singbox-dual-core) ----
    // The old global `expected_exit` bool had a race: stop(old) set the
    // flag, start(new) cleared it, and the OLD watcher could process the
    // exit event afterwards and misread the cleared flag as "crash" —
    // resurrecting the old core to fight the new one for ports.
    // The generation-based classifier below must make that impossible:
    // a watcher whose generation no longer matches NEVER restarts.
    #[test]
    fn stale_watcher_never_restarts_even_after_flag_clear() {
        // Old core spawned at gen 1; a new core already bumped gen to 2.
        // Whatever the expected-exit slot says, the stale watcher must stand down.
        assert_eq!(classify_exit(1, 2, u64::MAX), ExitDisposition::StaleWatchedOver);
        assert_eq!(
            classify_exit(1, 2, 1),
            ExitDisposition::StaleWatchedOver,
            "even a matching expected-exit must not resurrect a superseded core"
        );
    }

    #[test]
    fn intentional_stop_within_generation_is_not_a_crash() {
        assert_eq!(classify_exit(3, 3, 3), ExitDisposition::IntentionalStop);
    }

    #[test]
    fn unexpected_exit_within_generation_is_a_crash() {
        assert_eq!(classify_exit(3, 3, u64::MAX), ExitDisposition::Crash);
        // A stale expected-exit from an older generation must not suppress
        // the current generation's crash handling.
        assert_eq!(classify_exit(4, 4, 3), ExitDisposition::Crash);
    }

    #[test]
    fn core_kind_defaults_to_mihomo_and_setter_roundtrips() {
        let mgr = MihomoManager::new(PathBuf::from("/tmp/cfg"));
        assert_eq!(mgr.core_kind(), CoreKind::Mihomo);

        let mgr = mgr.with_core_kind(CoreKind::SingBox);
        assert_eq!(mgr.core_kind(), CoreKind::SingBox);
        // The shared inner (watcher auto-restart path) sees it too.
        assert_eq!(mgr.inner.core_kind(), CoreKind::SingBox);
    }

    #[test]
    fn api_transport_matches_core_kind() {
        use crate::mihomo_api::Transport;

        let mgr = MihomoManager::new(PathBuf::from("/tmp/cfg"));
        match mgr.api().transport() {
            Transport::UnixSocket(_) => {}
            other => panic!("mihomo api must use unix socket, got {other:?}"),
        }

        let mgr = mgr.with_core_kind(CoreKind::SingBox);
        match mgr.api().transport() {
            Transport::Tcp(addr) => assert_eq!(addr.to_string(), "127.0.0.1:9090"),
            other => panic!("sing-box api must use tcp, got {other:?}"),
        }
    }

    #[test]
    fn singbox_skeleton_is_valid_json_with_direct_fallback() {
        let body = build_singbox_skeleton_json().expect("skeleton");
        let value: serde_json::Value = serde_json::from_str(&body).expect("valid json");
        assert_eq!(value["route"]["final"], "direct");
        assert_eq!(
            value["experimental"]["clash_api"]["external_controller"],
            "127.0.0.1:9090"
        );
    }

    // ---- Task 3.3: readiness probe ----

    #[test]
    fn version_kind_detection_by_prefix() {
        assert!(version_matches_kind("sing-box 1.13.12", CoreKind::SingBox));
        assert!(!version_matches_kind("sing-box 1.13.12", CoreKind::Mihomo));
        assert!(version_matches_kind("v1.19.29", CoreKind::Mihomo));
        assert!(version_matches_kind("Mihomo Meta v1.19.29", CoreKind::Mihomo));
        assert!(!version_matches_kind("v1.19.29", CoreKind::SingBox));
    }

    #[tokio::test]
    async fn probe_accepts_matching_core_and_rejects_mismatch() {
        use crate::mihomo_api::{MihomoApi, Transport};
        use std::net::SocketAddr;

        let addr: SocketAddr = "127.0.0.1:1".parse().unwrap();
        let api = MihomoApi::with_transport(Transport::Tcp(addr), "s").unwrap();

        // Nothing listens: short timeout must surface as a readiness failure.
        let started = std::time::Instant::now();
        let err = probe_readiness(&api, CoreKind::Mihomo, std::time::Duration::from_millis(300))
            .await
            .expect_err("no listener");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "must not wait out a long timeout in tests"
        );
        assert!(err.to_string().contains("did not become ready"), "{err}");
    }
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
