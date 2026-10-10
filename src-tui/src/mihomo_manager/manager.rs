// Foundation module — public surface is wired up by Plan 02-03 (CLI
// dispatch + start/stop wiring).

use std::collections::VecDeque;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU16, AtomicU64, Ordering};
use std::time::Duration;

use anyhow::Context;
use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use serde::Serialize;
use tokio::process::Command;
use tokio::sync::mpsc;

use crate::app::{Action, CoreState};
use crate::mihomo_api::MihomoApi;
use crate::mihomo_manager::{binary, pidfile, signal, watcher::spawn_watcher};

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
///
/// Which proxy core this manager owns. Selecting the kind decides the
/// binary resolver, spawn arguments, runtime config file, and API
/// transport. The canonical definition lives in `pidfile` so the
/// on-disk [`crate::mihomo_manager::pidfile::CoreRecord`] can carry
/// the same enum and a sing-box reader cannot adopt a mihomo record.
pub use crate::mihomo_manager::pidfile::CoreKind;

pub struct ManagerInner {
    pub state: Mutex<CoreState>,
    pub action_tx: Mutex<Option<mpsc::Sender<Action>>>,
    pub started_at: Mutex<Option<DateTime<Utc>>>,
    pub restart_history: Mutex<VecDeque<DateTime<Utc>>>,
    pub pid: Mutex<Option<u32>>,
    /// Path of the resolved mihomo binary (set on start; None before first
    /// start). Used by TUN capability setup.
    pub resolved_binary: Mutex<Option<PathBuf>>,
    /// Set by `stop()` so the watcher knows this exit was intentional and
    /// should NOT trigger an auto-restart. Kept alongside the generation
    /// counter for back-compat with the cross-process pidfile intent path
    /// (owner main's pidfile/`clash-verge-cli stop` lifecycle).
    pub expected_exit: AtomicBool,
    pub restarting: AtomicBool,
    /// True while this process's watcher supervises the core it spawned
    /// (as opposed to a core adopted from another process's pid record).
    pub owns_child: AtomicBool,
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
    /// Controller secret resolved (and, on a fresh install or a weak value,
    /// rotated + persisted) by `enhance::resolve_controller_secret` at spawn
    /// time. It overrides the `MihomoManager`'s construction-time snapshot of
    /// `config.yaml`: the manager is built *before* the rotation, so its
    /// snapshot still holds the template's `set-your-secret` and every
    /// controller call would answer 401 — fatal for sing-box, whose clash_api
    /// is a TCP transport that really enforces the bearer secret.
    secret_override: Mutex<Option<String>>,
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
        rule_sets: Vec::new(),
        route_rules: Vec::new(),
        dns: None,
    };
    let config = crate::singbox::generate_config(&input).map_err(anyhow::Error::msg)?;
    serde_json::to_string_pretty(&config).map_err(Into::into)
}

/// Resource-release barrier (task 3.2, add-singbox-dual-core).
///
/// Called between stopping an old core and spawning the next one. Two jobs:
/// 1. Remove a stale external-controller unix socket — dead processes do
///    not clean it up on SIGKILL, and a leftover file blocks the rebind.
///    Recheck kernel state and private directory ownership before unlink.
/// 2. Poll until TUN devices from either core are gone. Both cores hijack
///    the default route; overlapping TUN lifetimes can blackhole traffic.
///
/// Socket uncertainty fails closed; TUN timeout remains best-effort.
pub(super) async fn resource_barrier(socket_path: &Path, timeout: std::time::Duration) -> anyhow::Result<()> {
    super::controller_socket::remove_stale(socket_path)
        .map_err(|error| anyhow::anyhow!("Controller socket {}: {error:#}", socket_path.display()))?;

    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let tun0 = net_iface_exists("tun0");
        let sb_tun0 = net_iface_exists("sb-tun0");
        if !tun0 && !sb_tun0 {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            tracing::warn!(
                target: "mihomo",
                "resource barrier timeout: tun device still present (tun0={tun0}, sb-tun0={sb_tun0})"
            );
            return Ok(());
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
    pub(crate) async fn send_action(&self, action: Action) {
        // The predecessor exiting during an explicit restart is not a final
        // stop notification; the replacement publishes ready or an error.
        if matches!(action, Action::CoreExited(_)) && self.restarting.load(std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        let tx = self.action_tx.lock().clone();
        if let Some(tx) = tx {
            let generation = self.generation.load(std::sync::atomic::Ordering::SeqCst);
            let _ = tx
                .send(Action::CoreGeneration {
                    generation,
                    action: Box::new(action),
                })
                .await;
        }
    }
    pub fn new() -> Self {
        Self {
            state: Mutex::new(CoreState::Stopped),
            action_tx: Mutex::new(None),
            started_at: Mutex::new(None),
            restart_history: Mutex::new(VecDeque::new()),
            pid: Mutex::new(None),
            resolved_binary: Mutex::new(None),
            expected_exit: AtomicBool::new(false),
            restarting: AtomicBool::new(false),
            owns_child: AtomicBool::new(false),
            generation: AtomicU64::new(0),
            expected_exit_gen: AtomicU64::new(u64::MAX),
            core_kind: AtomicU8::new(0),
            singbox_port: AtomicU16::new(9090),
            secret_override: Mutex::new(None),
        }
    }

    /// Publish the secret that was just resolved/persisted for this spawn.
    ///
    /// P1 (reviewer): `start` completes the rotation in the PARENT process
    /// before it launches the detached supervisor. The manager was built
    /// from the pre-rotation `config.yaml`, so without this its snapshot
    /// still carries `set-your-secret` and every controller call is
    /// rejected — a 15s start timeout on a perfectly healthy sing-box
    /// (whose clash_api is a TCP transport that enforces the bearer).
    pub fn set_secret_override(&self, secret: String) {
        *self.secret_override.lock() = Some(secret);
    }

    /// Drop the published secret (an explicit `set_secret` supersedes it).
    pub fn clear_secret_override(&self) {
        *self.secret_override.lock() = None;
    }

    /// The secret last resolved by the spawn path, if any.
    pub fn secret_override(&self) -> Option<String> {
        self.secret_override.lock().clone()
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
        let kind = inner.core_kind();
        Self::spawn_core_as(
            kind,
            resolved_path,
            _version,
            source,
            config_dir,
            socket_path,
            config_path,
            inner,
            true,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn spawn_core_as(
        kind: CoreKind,
        resolved_path: &Path,
        _version: &str,
        source: &str,
        config_dir: &Path,
        socket_path: &Path,
        config_path: Option<PathBuf>,
        inner: Arc<ManagerInner>,
        publish: bool,
    ) -> anyhow::Result<()> {
        let config_path = config_path.or_else(|| match kind {
            CoreKind::Mihomo => clash_verge_core::utils::dirs::clash_path().ok(),
            CoreKind::SingBox => clash_verge_core::utils::dirs::singbox_config_path().ok(),
        });
        // #48/#49: resolve (and, when needed, rotate + persist) the shared
        // controller secret BEFORE anything is spawned. On a fresh install
        // config.yaml does not exist yet; composing it here is what makes
        // `start` work out of the box instead of failing with a bare
        // ENOENT from `controller_secret_from_config`.
        let secret = crate::enhance::resolve_controller_secret()
            .await
            .context("failed to prepare the controller secret in the clash config; no core was started")?;
        // Publish it to the live manager: it was built from the pre-rotation
        // config.yaml, so its snapshot still carries the template secret and
        // every controller call would be rejected (401) on sing-box.
        inner.set_secret_override(secret);
        // Validate control-plane auth before a child exists; errors here
        // cannot leak an unsupervised process or invalidate rollback.
        let probe_api = api_for_core(
            kind,
            socket_path,
            inner.singbox_controller(),
            controller_secret_from_config(config_path.as_deref())?,
        )?;
        // #49: name the missing path up front. A bare `os error 2` from
        // `Command::spawn` is indistinguishable between "binary missing" and
        // "runtime config missing", which is what made the original report
        // blame the core binary for a missing config.yaml.
        if !resolved_path.exists() {
            anyhow::bail!(
                "core binary '{}' does not exist; install it or point `verge_mihomo_version`/`proxy_core` at a valid binary. No core was started.",
                resolved_path.display()
            );
        }
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
        resource_barrier(socket_path, std::time::Duration::from_secs(5)).await?;

        *inner.resolved_binary.lock() = Some(resolved_path.to_path_buf());
        let mut command = Command::new(resolved_path);
        match kind {
            CoreKind::Mihomo => {
                command.arg("-d").arg(config_dir);
                if let Ok(config_path) = config_path
                    .clone()
                    .map(Ok)
                    .unwrap_or_else(clash_verge_core::utils::dirs::clash_path)
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
                if !path.exists() {
                    anyhow::bail!(
                        "generated sing-box config '{}' does not exist; run `clash-verge-cli profile use <id>` or `apply runtime` to generate it. No core was started.",
                        path.display()
                    );
                }
                command.arg("run").arg("-c").arg(path);
            }
        }
        // Output is piped into this process's tracing, so the spawner must
        // stay alive as the core's supervisor (a pipe to an exited process
        // kills mihomo with SIGPIPE). `clash-verge-cli start` therefore runs
        // a detached `start --foreground` supervisor rather than spawning here.
        // NOTE: the `match inner.core_kind()` arm above already appends
        // `-ext-ctl-unix <socket_path>` for the mihomo case; sing-box binds
        // its controller via the generated JSON's `clash_api.listen`.
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

        let started_at = Utc::now();
        let core_kind = kind;
        *inner.state.lock() = CoreState::Running;
        *inner.pid.lock() = Some(pid);
        *inner.started_at.lock() = Some(started_at);
        // Owner main's pidfile/adopt/cross-process stop lifecycle: this
        // process now supervises the core it just spawned, the pidfile
        // lets later CLI invocations adopt_running_core, and we reset the
        // legacy `expected_exit` flag so a future exit is treated as a
        // crash until stop() re-arms it. The recorded kind disambiguates
        // the on-disk record (a mihomo manager must never adopt a
        // sing-box record or vice versa — see `pidfile::read_live_for_kind`).
        inner.owns_child.store(true, std::sync::atomic::Ordering::SeqCst);
        if let Err(error) = pidfile::write(
            &pidfile::path_for(socket_path),
            // #54: record the resolved executable so a later CLI process
            // can adopt this core by identity, whatever the binary is
            // called (`verge-sing-box` included).
            pidfile::CoreRecord::with_kind_and_exe(
                pid,
                started_at,
                core_kind,
                std::fs::canonicalize(resolved_path)
                    .ok()
                    .map(|path| path.to_string_lossy().into_owned()),
            ),
        ) {
            tracing::warn!(target: "mihomo", "failed to record the core pid: {error}");
        }
        inner.expected_exit.store(false, std::sync::atomic::Ordering::SeqCst);
        // Sing-box branch generation-race protection (task 3.1): bump the
        // generation AFTER the pidfile is on disk so a watcher from a
        // previous generation can never resurrect this one on its exit.
        let spawned_gen = inner.generation.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
        inner
            .expected_exit_gen
            .store(u64::MAX, std::sync::atomic::Ordering::SeqCst);

        // Task 3.3: do not report success until the controller actually
        // answers with the expected core type. A broken config makes the
        // child exit immediately; probing catches that here so callers can
        // roll back instead of reporting a healthy core.
        let probed_version = match probe_readiness(&probe_api, core_kind, READINESS_PROBE_TIMEOUT).await {
            Ok(v) => v,
            Err(error) => {
                // P1-1 (reviewer): the readiness probe can fail in two ways
                // — the child exited on its own (bad config) or it is wedged.
                // Either way we must FULLY clean up the in-memory runtime
                // state so a subsequent `start` does not see a stale pid /
                // started_at and re-try the adopted-core bail path. The
                // pidfile still names us at the time of the kill, so
                // remove_if can confidently drop it.
                let _ = signal::graceful_stop_by_pid(pid).await;
                rollback_failed_spawn(&inner, socket_path, pid, error.to_string());
                return Err(error);
            }
        };

        if publish {
            inner.set_core_kind(kind);
            inner
                .send_action(Action::CoreStarted {
                    version: Some(probed_version),
                    binary_path: Some(resolved_path.display().to_string()),
                    binary_source: Some(source.to_string()),
                })
                .await;
        }

        // The desktop proxy follows the core (no-op unless
        // `enable_system_proxy` is on). Apply before the watcher exists so a
        // core that exits immediately is released after this, never raced.
        // Owner main's lifecycle: this is the pair of `release_on_core_stop`
        // in stop()/watcher() so the system proxy never points at a port that
        // just closed.
        crate::sys_proxy::apply_on_core_start().await;
        // Generation-aware watcher: passes `spawned_gen` so a watcher bound
        // to a superseded spawn stands down on its exit event (task 3.1).
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
                let config_path = Self::write_singbox_full(config_dir).await?;
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

    /// Read the active profile's YAML from disk, if one is selected and
    /// readable. None means "generate the bare skeleton".
    pub(crate) async fn active_profile_yaml() -> anyhow::Result<Option<String>> {
        let store = crate::profile_store::store::ProfileStore::snapshot().await?;
        let Some(uid) = store.current_uid() else {
            return Ok(None);
        };
        let item = store
            .items()
            .into_iter()
            .find(|item| item.uid.as_deref() == Some(uid.as_str()))
            .ok_or_else(|| anyhow::anyhow!("selected profile {uid} is missing"))?;
        let raw = crate::runtime_config::load_profile_yaml(&item)
            .await
            .map_err(anyhow::Error::msg)?;
        if crate::subscribe::from_url::is_singbox_json_profile(&raw) {
            return Ok(Some(raw));
        }
        let mapping = serde_yaml_ng::from_str(&raw).context("invalid composed profile YAML")?;
        let (mapping, _) = crate::services::profile::prepare_profile_dns_from_settings(&uid, mapping)
            .await
            .map_err(anyhow::Error::msg)?;
        Ok(Some(serde_yaml_ng::to_string(&mapping)?))
    }

    /// Generate and persist the sing-box runtime config from whatever the
    /// active profile currently holds (task 7.5): converted outbounds/groups,
    /// route rules, stored logical rules, rule-sets and structured DNS.
    pub(crate) async fn write_singbox_full(config_dir: &Path) -> anyhow::Result<PathBuf> {
        let yaml = Self::active_profile_yaml().await?;
        let enable_tun = runtime_tun_enabled().await.unwrap_or(false);
        let (path, parts) = Self::write_singbox_assembled(config_dir, yaml.as_deref(), enable_tun).await?;
        // Start, restart and crash auto-restart all funnel through here, so
        // this is where the losses of the config the core is about to run
        // become known: the TUI notice and `status --json` read the record.
        // A native sing-box subscription passes through untouched and
        // records nothing.
        crate::runtime_config::record_singbox_degradation(&parts).await;
        Ok(path)
    }

    /// The runtime config a start must run, honouring the recovery mode.
    ///
    /// A NORMAL start/restart regenerates the config from the active profile
    /// (and may create or overwrite the file); a RECOVERY start consumes the
    /// frozen, already-validated file verbatim and fails when it is absent.
    /// The two are decided HERE so the start and the restart path cannot
    /// disagree — the recovery restriction once leaked into the ordinary
    /// restart, which then demanded a file it should have generated.
    ///
    /// The runtime config THIS launch must serve, prepared BEFORE the old
    /// core goes away.
    ///
    /// `mode` travels with the launch (A5, reviewer):
    ///
    /// - [`SupervisorLaunch::Regenerate`] — a normal cold start/restart:
    ///   generate the runtime config from the active profile, creating it
    ///   when it is missing.
    /// - [`SupervisorLaunch::UseExistingConfig`] — serve the file already on
    ///   disk, verbatim. It is either an apply's *verified candidate* (frozen,
    ///   `sing-box check`ed and installed by the transaction) or the
    ///   *restored previous config* of a rollback recovery.
    ///
    /// Regenerating in the second case is the A5 defect: it rebuilds the
    /// config from whatever the source says NOW — re-running a Script chain
    /// item, picking up a profile edited since the check — so the core would
    /// serve bytes that were never verified, and the restored config would be
    /// overwritten in the same breath.
    pub(crate) async fn runtime_config_for_start(
        &self,
        config_dir: &Path,
        mode: crate::commands::start::SupervisorLaunch,
    ) -> anyhow::Result<PathBuf> {
        if mode == crate::commands::start::SupervisorLaunch::Regenerate {
            return Self::write_singbox_full(config_dir).await;
        }
        let path = Self::existing_singbox_config().await?;
        tracing::info!(
            target: "singbox",
            "launch mode UseExistingConfig: serving the runtime config {} as-is (no regeneration)",
            path.display()
        );
        Ok(path)
    }

    /// The existing, validated runtime config, or a clear error.
    async fn existing_singbox_config() -> anyhow::Result<PathBuf> {
        let path = clash_verge_core::utils::dirs::singbox_config_path()?;
        if !tokio::fs::try_exists(&path).await? {
            anyhow::bail!(
                "launch mode UseExistingConfig: no restored {} to serve; \
the previous configuration could not be recovered",
                path.display()
            );
        }
        Ok(path)
    }

    /// Persist a sing-box runtime config assembled from the given profile
    /// YAML (None → bare skeleton). Returns the written path plus the parts
    /// so callers can build a degradation report without re-converting.
    pub(crate) async fn write_singbox_assembled(
        config_dir: &Path,
        yaml: Option<&str>,
        enable_tun: bool,
    ) -> anyhow::Result<(PathBuf, SingboxParts)> {
        let path = clash_verge_core::utils::dirs::singbox_config_path()?;
        Self::write_singbox_assembled_to(config_dir, yaml, enable_tun, &path).await
    }

    /// Assemble and atomically write to an explicit destination. Runtime
    /// applies use a candidate path, validate it, and only then replace the
    /// formal config consumed by the core.
    pub(crate) async fn write_singbox_assembled_to(
        config_dir: &Path,
        yaml: Option<&str>,
        enable_tun: bool,
        destination: &Path,
    ) -> anyhow::Result<(PathBuf, SingboxParts)> {
        let _ = config_dir;
        crate::singbox::capabilities::for_version(crate::mihomo_manager::core_policy::SINGBOX_POLICY_VERSION)
            .ok_or_else(|| anyhow::anyhow!("missing pinned sing-box configuration capability matrix"))?;
        // Honour the CLI's own `verge_mixed_port` override so sing-box binds the
        // same non-conflicting port as the mihomo runtime config.
        let mixed_port = crate::enhance::effective_mixed_port().await;
        let core_config = clash_verge_core::config::IClashTemp::new().await;
        let tun = profile_tun_settings(yaml, &core_config.0).map_err(anyhow::Error::msg)?;
        let clash_api = crate::singbox::ClashApiSettings {
            listen: configured_singbox_controller(&core_config.0)?,
            // #48: never write the template's public `set-your-secret` (or an
            // empty secret) into singbox.json. Rotate + persist into the shared
            // config.yaml first, so the CLI and the generated clash_api agree.
            secret: crate::enhance::resolve_controller_secret()
                .await
                .context("cannot generate the sing-box clash_api controller secret")?,
        };

        // Native sing-box JSON profile passthrough: preserve the provider's own
        // outbounds, route, and dns, and only enforce the CLI-owned control plane
        // (inbounds + clash_api + log). Avoids lossy conversion through the Clash model.
        if let Some(text) = yaml
            && crate::subscribe::from_url::is_singbox_json_profile(text)
        {
            let mut config: serde_json::Value =
                serde_json::from_str(text).context("failed to parse native sing-box JSON profile")?;
            crate::singbox::config_gen::validate_native_config(&config).map_err(anyhow::Error::msg)?;
            crate::singbox::config_gen::apply_control_plane(&mut config, mixed_port, enable_tun, &tun, &clash_api);
            let home = clash_verge_core::utils::dirs::app_home_dir()?;
            apply_native_sidecars(&mut config, &home)?;
            crate::singbox::config_gen::validate_native_config(&config).map_err(anyhow::Error::msg)?;
            crate::singbox::config_gen::validate_control_plane_security(&config).map_err(anyhow::Error::msg)?;
            let path = destination.to_path_buf();
            if let Some(parent) = path.parent() {
                tokio::fs::create_dir_all(parent).await.ok();
            }
            write_generated_json(&path, &config)?;

            let outbounds = config
                .get("outbounds")
                .and_then(serde_json::Value::as_array)
                .cloned()
                .unwrap_or_default();
            let parts = SingboxParts {
                conversion: crate::singbox::convert::ProfileConversion {
                    outbounds,
                    groups: vec![],
                    skipped: vec![],
                    degraded: vec![],
                    notes: vec![],
                },
                profile_used: true,
                route_rules: vec![],
                rule_sets: vec![],
                dns_section: None,
                default_domain_resolver: None,
            };
            return Ok((path, parts));
        }

        let parts = SingboxParts::assemble(yaml).await?;
        let input = crate::singbox::ConfigInput {
            outbounds: parts.conversion.outbounds.clone(),
            groups: parts.conversion.groups.clone(),
            rule_sets: parts.rule_sets.clone(),
            route_rules: parts.route_rules.clone(),
            mixed_port,
            enable_tun,
            tun,
            clash_api,
            dns: parts.dns_section.clone(),
        };
        let mut config = crate::singbox::generate_config(&input).map_err(anyhow::Error::msg)?;
        // 1.12+ bootstrap: tell the core which DNS server resolves outbound
        // node domains (see D8).
        if let Some(resolver) = &parts.default_domain_resolver {
            config["route"]["default_domain_resolver"] = serde_json::json!(resolver);
        }
        let path = destination.to_path_buf();
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await.ok();
        }
        crate::singbox::config_gen::validate_native_config(&config).map_err(anyhow::Error::msg)?;
        crate::singbox::config_gen::validate_control_plane_security(&config).map_err(anyhow::Error::msg)?;
        write_generated_json(&path, &config)?;
        Ok((path, parts))
    }
}

/// Everything assembled for one sing-box config generation pass.
pub(crate) struct SingboxParts {
    pub conversion: crate::singbox::convert::ProfileConversion,
    /// Whether a profile document fed the conversion (drives the status
    /// report wording).
    pub profile_used: bool,
    pub route_rules: Vec<serde_json::Value>,
    pub rule_sets: Vec<serde_json::Value>,
    pub dns_section: Option<serde_json::Value>,
    pub default_domain_resolver: Option<String>,
}

impl SingboxParts {
    async fn assemble(yaml: Option<&str>) -> anyhow::Result<Self> {
        let (conversion, profile_used) = match yaml {
            Some(y) => (
                crate::singbox::convert::convert_profile(y).map_err(anyhow::Error::msg)?,
                true,
            ),
            None => (crate::singbox::convert::ProfileConversion::default(), false),
        };
        let home = clash_verge_core::utils::dirs::app_home_dir().ok();
        let stored_rule_sets: Vec<serde_json::Value> = match &home {
            Some(home) => crate::singbox::load_rule_sets(home).map_err(anyhow::Error::msg)?,
            None => Vec::new(),
        };
        // Profile rules degrade gracefully (#52): unrepresentable rules are
        // reported instead of aborting the whole configuration, and the geo
        // rule-sets they reference are materialized alongside them.
        let stored_tags: std::collections::HashSet<String> = stored_rule_sets
            .iter()
            .filter_map(|set| set.get("tag").and_then(serde_json::Value::as_str))
            .map(str::to_owned)
            .collect();
        let profile_routes = match yaml {
            Some(y) => profile_route_rules(y, &conversion.outbound_tags(), &stored_tags).map_err(anyhow::Error::msg)?,
            None => ProfileRouteRules {
                rules: Vec::new(),
                slots: Vec::new(),
                rule_sets: Vec::new(),
                skipped: Vec::new(),
            },
        };
        let profile_route_values = profile_routes.rules;
        let rule_sets = merge_rule_sets(stored_rule_sets, profile_routes.rule_sets);
        // The editor's interleaved order is authoritative: a logical rule the
        // user placed above a profile `MATCH` must stay above that catch-all,
        // or the core would never evaluate it. A sidecar without order
        // information keeps the historical append-after-profile behaviour.
        //
        // The order indexes the ORIGINAL composed profile rule list, so it is
        // interleaved against the original slots (rules the conversion
        // dropped keep their slot identity and are skipped) — never against
        // the compressed converted list.
        let profile_route_slots: Vec<Option<serde_json::Value>> = profile_routes
            .slots
            .iter()
            .map(|slot| slot.map(|index| profile_route_values[index].clone()))
            .collect();
        let route_rules = match &home {
            Some(home) => {
                let stored = crate::singbox::load_rule_order(home).map_err(anyhow::Error::msg)?;
                // A subscription refresh can replace the profile rule list
                // under the sidecar; every stored index stays in range and
                // silently points at a different rule. Detect that and fall
                // back to append-after, reported like any other loss.
                //
                // The identity compared here is the COMPOSED rule list — the
                // very list the slots above were built from and the one the
                // stored `Profile(i)` indices address. The editor's save
                // fingerprints the same list (see
                // `runtime_config::composed_profile_rules`), so a fresh save
                // can no longer be misread as a refresh.
                let (order, drift) = match yaml.and_then(|y| crate::routing::load_profile_rules(y).ok()) {
                    Some(profile_rules) => {
                        stored.resolve_profile_drift(crate::singbox::profile_rule_fingerprint(&profile_rules))
                    }
                    // No parseable profile rule list: nothing to compare
                    // against, so the stored order is used as-is.
                    None => (stored.clone(), None),
                };
                let merged = crate::singbox::interleave_route_slots(&profile_route_slots, &order);
                if let Some(note) = &drift {
                    tracing::warn!(target: "config", "{note}");
                }
                (merged, drift)
            }
            None => (
                crate::singbox::interleave_route_slots(&profile_route_slots, &crate::singbox::RuleOrder::default()),
                None,
            ),
        };
        let (route_rules, route_order_note) = route_rules;
        // Rules the conversion could not express are reported, never fatal.
        let route_report = profile_routes.skipped;
        let mut conversion = conversion;
        let stored_dns = match &home {
            Some(home) => crate::singbox::load_dns_spec(home).map_err(anyhow::Error::msg)?,
            None => crate::singbox::DnsConfigSpec::default(),
        };
        // Profile DNS degrades gracefully like the rules layer: what the
        // typed model cannot express is reported in `conversion.notes`
        // instead of aborting the whole configuration.
        let dns_report = yaml
            .map(crate::singbox::dns::dns_conversion_from_clash_yaml)
            .transpose()
            .map_err(anyhow::Error::msg)?
            .unwrap_or_default();
        let profile_dns = dns_report.spec.clone();
        let (dns_spec, dns_notes) = if home
            .as_ref()
            .is_some_and(|home| home.join(crate::singbox::DNS_CONFIG_FILE).exists())
        {
            // A stored DNS override wins; the profile's DNS was not consulted,
            // so its degradations are not this run's report.
            (stored_dns, Vec::new())
        } else {
            (profile_dns.unwrap_or(stored_dns), dns_report.notes)
        };
        let default_domain_resolver = crate::singbox::dns::default_domain_resolver(&dns_spec);
        let dns_section = crate::singbox::dns::build_dns_section(&dns_spec).map_err(anyhow::Error::msg)?;
        conversion.notes.extend(route_report);
        conversion.notes.extend(route_order_note);
        conversion.notes.extend(dns_notes);
        Ok(Self {
            conversion,
            profile_used,
            route_rules,
            rule_sets,
            dns_section,
            default_domain_resolver,
        })
    }
}

/// A process forked by `pid`, read from `/proc/<pid>/task/*/children`.
///
/// Linux exposes the child list per thread group; the union over the group's
/// threads is the process's direct children. A pid that is gone (or a
/// `/proc` that is not mounted) simply yields nothing — ownership then falls
/// back to "nothing can be attributed", never to "everything can".
fn direct_child_pids(pid: u32) -> Vec<u32> {
    let Ok(entries) = std::fs::read_dir(format!("/proc/{pid}/task")) else {
        return Vec::new();
    };
    let mut children = Vec::new();
    for entry in entries.flatten() {
        let Ok(body) = std::fs::read_to_string(entry.path().join("children")) else {
            continue;
        };
        children.extend(body.split_whitespace().filter_map(|token| token.parse::<u32>().ok()));
    }
    children.sort_unstable();
    children.dedup();
    children
}

/// What one supervisor launch owns: the processes that supervisor forked.
///
/// Cleanup after a failed launch may only touch those. The pid record on
/// disk is SHARED — it names whichever instance wrote it last — so it can
/// never be the ownership proof on its own.
pub(crate) struct LaunchOwnership {
    supervisor_pid: u32,
}

impl LaunchOwnership {
    /// Bind a launch to the supervisor it started: the only processes its
    /// cleanup may stop are the ones that supervisor forked.
    pub(crate) fn new(supervisor_pid: u32) -> Self {
        Self { supervisor_pid }
    }

    /// Every descendant of the supervisor, breadth-first.
    ///
    /// Must be read BEFORE the supervisor is killed: once it exits, its
    /// children are reparented to init and no longer identify it.
    fn descendant_pids(&self) -> Vec<u32> {
        let mut found: Vec<u32> = Vec::new();
        let mut queue = direct_child_pids(self.supervisor_pid);
        while let Some(pid) = queue.pop() {
            if found.contains(&pid) {
                continue;
            }
            found.push(pid);
            queue.extend(direct_child_pids(pid));
        }
        found
    }
}

/// Test seam (A5, reviewer): stands in for the spawn half of an OWNED
/// restart, so a test can observe which [`SupervisorLaunch`](crate::commands::start::SupervisorLaunch)
/// mode the owned branch was asked for — in particular that a rollback
/// recovery keeps `UseExistingConfig` instead of falling back to
/// `Regenerate`, and that it does not re-run the authorization policy.
#[cfg(test)]
pub(crate) type OwnedRestartHook = fn(crate::commands::start::SupervisorLaunch) -> anyhow::Result<()>;

#[cfg(test)]
static OWNED_RESTART: std::sync::Mutex<Option<OwnedRestartHook>> = std::sync::Mutex::new(None);

#[cfg(test)]
fn owned_restart_hook() -> Option<OwnedRestartHook> {
    *OWNED_RESTART.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Arm the owned-restart seam for one test; the guard disarms it on drop.
#[cfg(test)]
pub(crate) fn install_owned_restart_hook(hook: OwnedRestartHook) -> anyhow::Result<OwnedRestartHookGuard> {
    *OWNED_RESTART.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(hook);
    Ok(OwnedRestartHookGuard)
}

#[cfg(test)]
pub(crate) struct OwnedRestartHookGuard;

#[cfg(test)]
impl Drop for OwnedRestartHookGuard {
    fn drop(&mut self) {
        *OWNED_RESTART.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
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
            singbox_controller: "127.0.0.1:9090".parse().expect("static addr"),
        }
    }

    /// Select which core this manager owns. Must be set before `start()`.
    pub fn with_core_kind(self, kind: CoreKind) -> Self {
        self.inner.set_core_kind(kind);
        self
    }

    /// Override the sing-box clash_api TCP endpoint.
    pub fn with_singbox_controller(mut self, addr: std::net::SocketAddr) -> Self {
        self.singbox_controller = addr;
        self.inner.set_singbox_port(addr.port());
        self
    }

    pub fn core_kind(&self) -> CoreKind {
        self.inner.core_kind()
    }

    /// The sing-box clash_api TCP endpoint this manager talks to (#54:
    /// sing-box's controller is TCP-only, so user-facing messages must
    /// name this address, not the unix socket).
    pub fn singbox_controller_addr(&self) -> std::net::SocketAddr {
        self.singbox_controller
    }

    pub fn with_socket(mut self, socket_path: PathBuf) -> Self {
        self.socket_path = socket_path;
        self
    }

    pub fn with_secret(mut self, secret: String) -> Self {
        self.secret = secret;
        self
    }

    /// Take over a core that another process started for this controller
    /// socket (recorded in its pid file), so `stop`, `restart`, and `status`
    /// work across CLI invocations. No-op when this manager already tracks a
    /// core or none is running.
    ///
    /// P0-3 (reviewer): the adoption check MUST be core-kind aware —
    /// a mihomo manager must never adopt a sing-box record (the API
    /// transports differ and a sing-box controller on the same socket
    /// dir cannot answer mihomo's HTTP calls), and vice versa. The
    /// kind-aware read is delegated to `pidfile::read_live_for_kind`.
    pub fn adopt_running_core(&self) {
        if self.inner.pid.lock().is_some() {
            return;
        }
        let kind = self.core_kind();
        let singbox_endpoint = match kind {
            CoreKind::Mihomo => None,
            CoreKind::SingBox => Some(self.singbox_controller),
        };
        if let Some(record) = pidfile::read_live_for_kind(
            &pidfile::path_for(&self.socket_path),
            &self.socket_path,
            kind,
            singbox_endpoint,
        ) {
            *self.inner.pid.lock() = Some(record.pid);
            *self.inner.started_at.lock() = record.started_at();
            *self.inner.state.lock() = CoreState::Running;
        }
    }

    /// Whether this process spawned the core and supervises it.
    pub fn owns_child(&self) -> bool {
        self.inner.owns_child.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Install the action channel sender. Called by the TUI after it has
    /// spawned its action loop. The CLI mode leaves this unset and just
    /// ignores watcher events.
    pub fn set_action_tx(&self, tx: mpsc::Sender<Action>) {
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
        // An explicit assignment supersedes anything the spawn path published.
        self.inner.clear_secret_override();
    }

    /// Publish a secret resolved before the supervisor was launched.
    ///
    /// P1 (reviewer): `start` completes the rotation in the PARENT, which
    /// built this manager from the pre-rotation `config.yaml`; without the
    /// override its snapshot still carries `set-your-secret` and every
    /// controller call is rejected (fatal on sing-box's authenticated TCP
    /// clash_api).
    pub fn set_secret_override(&self, secret: String) {
        self.inner.set_secret_override(secret);
    }

    pub fn set_socket_path(&mut self, path: PathBuf) {
        self.socket_path = path;
    }

    /// Build a MihomoApi client targeting this manager's socket with
    /// bearer auth from the configured secret.
    pub fn current_generation(&self) -> u64 {
        self.inner.generation.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// The secret the controller client must present.
    ///
    /// Resolution order: the value the spawn path published (it also
    /// persisted it into `config.yaml`), else the manager's own snapshot when
    /// that snapshot is a real secret, else the last secret resolved anywhere
    /// in this process, else the snapshot verbatim. Every one of the secret's
    /// producers (spawn, sing-box config generation) runs before the first
    /// controller call, so a template `set-your-secret` snapshot never reaches
    /// the wire.
    pub fn effective_secret(&self) -> String {
        if let Some(resolved) = self.inner.secret_override() {
            return resolved;
        }
        if !self.secret.trim().is_empty() && !crate::enhance::is_placeholder_secret(&self.secret) {
            return self.secret.clone();
        }
        crate::enhance::last_resolved_secret().unwrap_or_else(|| self.secret.clone())
    }

    pub fn api(&self) -> MihomoApi {
        let secret = self.effective_secret();
        let result = match self.core_kind() {
            CoreKind::Mihomo => MihomoApi::new(self.socket_path.clone(), secret),
            CoreKind::SingBox => {
                MihomoApi::with_transport(crate::mihomo_api::Transport::Tcp(self.singbox_controller), secret)
            }
        };
        result
            .expect("MihomoApi construction failed — secret may contain invalid header characters")
            .for_core(self.core_kind())
    }

    /// A guided operation never takes lifecycle ownership of an adopted core.
    /// Kept separate from the GUI scan so tests exercise the policy without /proc.
    pub fn guided_owner_check(&self, gui_running: bool) -> anyhow::Result<()> {
        if gui_running {
            super::gui_isolation::check(
                &self.config_dir,
                &self.socket_path,
                if self.owns_child() { self.pid() } else { None },
                None,
            )
            .context(
                "A Clash Verge GUI instance is running; changing this core requires explicitly isolated CLI resources",
            )?;
        }
        if self.pid().is_some() && !self.owns_child() {
            anyhow::bail!(
                "This core belongs to another supervisor. Change it through its owner; the TUI cannot stop or switch an attached core."
            );
        }
        Ok(())
    }

    fn guided_record_check(&self, target: CoreKind) -> anyhow::Result<()> {
        self.guided_record_check_with(target, || std::fs::read_to_string("/proc/net/unix"))
    }

    pub fn guided_preflight(&self, generation: u64, target: CoreKind) -> anyhow::Result<()> {
        self.guided_owner_check(super::ownership::gui_process_running())?;
        if generation != self.current_generation() || self.inner.restarting.load(Ordering::SeqCst) {
            anyhow::bail!("Core ownership/generation changed since checking; retry the operation");
        }
        self.guided_record_check(target)
    }

    /// Read-only guard for choosing the next core while no CLI core is active.
    /// GUI coexistence is safe here: no GUI lifecycle or runtime file is used.
    fn stopped_selection_preflight(&self, generation: u64, old_kind: CoreKind, target: CoreKind) -> anyhow::Result<()> {
        if generation != self.current_generation()
            || old_kind != self.core_kind()
            || !matches!(self.state(), CoreState::Stopped | CoreState::Error(_))
            || self.pid().is_some()
        {
            anyhow::bail!("Core ownership/generation or selection changed; retry the stopped selection");
        }
        self.guided_record_check(target)?;
        if old_kind == CoreKind::SingBox && target != CoreKind::SingBox {
            self.guided_record_check(CoreKind::SingBox)?;
        }
        Ok(())
    }

    fn commit_stopped_selection(
        &self,
        generation: u64,
        old_kind: CoreKind,
        target: CoreKind,
        cancelled: &AtomicBool,
    ) -> anyhow::Result<()> {
        check_guided_cancel(cancelled)?;
        self.stopped_selection_preflight(generation, old_kind, target)?;
        if self.inner.restarting.swap(true, Ordering::SeqCst) {
            anyhow::bail!("A core lifecycle operation is already in progress");
        }
        let _busy = LifecycleBusy(&self.inner.restarting);
        let home = clash_verge_core::utils::dirs::app_home_dir()?;
        persist_stopped_selection(&home, target, || {
            check_guided_cancel(cancelled)?;
            self.stopped_selection_preflight(generation, old_kind, target)
        })?;
        self.inner.set_core_kind(target);
        *self.inner.state.lock() = CoreState::Stopped;
        // A committed choice invalidates older confirmations even though no
        // process was spawned. Shared clones observe the same selection epoch.
        self.inner.generation.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    fn guided_record_check_with(
        &self,
        target: CoreKind,
        read_status: impl FnOnce() -> std::io::Result<String>,
    ) -> anyhow::Result<()> {
        let socket = super::controller_socket::inspect_with(&self.socket_path, read_status)
            .map_err(|error| anyhow::anyhow!("Controller socket {}: {error:#}", self.socket_path.display()))?;
        let record = pidfile::read_record(&pidfile::path_for(&self.socket_path));
        let live = record.as_ref().is_some_and(|record| pidfile::is_running(record.pid));
        check_guided_record(
            record.as_ref().map(|record| record.pid),
            live,
            self.pid(),
            self.owns_child(),
            socket == super::controller_socket::SocketState::Bound,
        )
        .map_err(|error| anyhow::anyhow!("Controller socket {}: {error:#}", self.socket_path.display()))?;
        self.guided_tcp_controller_check_with(
            target,
            |pid, address| super::gui_isolation::owns_listener(pid, address, "tcp"),
            super::ownership::gui_process_running,
        )
    }

    fn guided_tcp_controller_check_with(
        &self,
        target: CoreKind,
        owns_listener: impl FnOnce(u32, std::net::SocketAddr) -> anyhow::Result<bool>,
        gui_running: impl FnOnce() -> bool,
    ) -> anyhow::Result<()> {
        // Retain the existing owned SingBox restart policy. Capability-bearing
        // children may be non-dumpable even to their unprivileged supervisor;
        // the record guard above still protects attached/changed ownership.
        if self.core_kind() == CoreKind::SingBox && self.owns_child() && self.pid().is_some() {
            return Ok(());
        }
        if target == CoreKind::SingBox {
            // Binding a temporary listener detects a foreign owner without
            // sending HTTP to, adopting, or signalling that controller.
            if let Err(error) = std::net::TcpListener::bind(self.singbox_controller) {
                let owned = if error.kind() == std::io::ErrorKind::AddrInUse
                    && self.owns_child()
                    && let Some(pid) = self.pid()
                {
                    match owns_listener(pid, self.singbox_controller) {
                        Ok(owned) => owned,
                        Err(error)
                            if self.core_kind() == CoreKind::Mihomo
                                && error
                                    .downcast_ref::<std::io::Error>()
                                    .is_some_and(|error| error.kind() == std::io::ErrorKind::PermissionDenied)
                                && !gui_running() =>
                        {
                            // A capability-bearing owned child may be non-dumpable.
                            // Defer this one check; launch always verifies that the
                            // target TCP port is free after the owned stop, before
                            // starting or contacting any replacement.
                            return Ok(());
                        }
                        Err(error) => {
                            return Err(error).context(
                                "cannot verify owned predecessor's controller listener; no process was stopped",
                            );
                        }
                    }
                } else {
                    false
                };
                if !owned {
                    return Err(error)
                        .context("sing-box controller port belongs to another process; stop it through its owner");
                }
            }
        }
        Ok(())
    }

    /// Consume the exact verified candidate. All preparation precedes stop;
    /// cancellation after stop is cooperative so rollback finishes before exit.
    pub async fn apply_prepared_core(
        &self,
        prepared: &binary::PreparedCore,
        expected_generation: u64,
        start_if_stopped: bool,
        enable_tun: bool,
        cancelled: &AtomicBool,
        stages: Option<mpsc::Sender<()>>,
    ) -> anyhow::Result<()> {
        let old_kind = self.core_kind();
        let _config_lock = crate::runtime_config::RUNTIME_CONFIG_IO.lock().await;
        if !start_if_stopped && self.state() != CoreState::Running {
            return self.commit_stopped_selection(expected_generation, old_kind, prepared.kind, cancelled);
        }
        self.guided_preflight(expected_generation, prepared.kind)?;
        let generation = expected_generation;
        let was_running = self.state() == CoreState::Running;
        let old_binary = self.binary_path();
        if was_running && old_binary.is_none() {
            anyhow::bail!("Cannot switch: the owned core executable is unknown; no process was stopped.");
        }
        let home = clash_verge_core::utils::dirs::app_home_dir()?;
        let target_config = match prepared.kind {
            CoreKind::Mihomo => clash_verge_core::utils::dirs::clash_path()?,
            CoreKind::SingBox => clash_verge_core::utils::dirs::singbox_config_path()?,
        };
        std::fs::create_dir_all(&home)?;
        let candidate = tempfile::NamedTempFile::new_in(&home)?.into_temp_path();
        let files = GuidedFileSnapshot::capture([
            target_config.clone(),
            clash_verge_core::utils::dirs::verge_path()?,
            home.join(super::ownership::OWNERSHIP_MARKER),
        ])?;
        let selections: Vec<(String, String)> = if was_running {
            self.api()
                .get_proxies()
                .await?
                .proxies
                .into_iter()
                .filter(|(group, data)| group != "GLOBAL" && data.group_type.eq_ignore_ascii_case("selector"))
                .filter_map(|(group, data)| data.now.map(|node| (group, node)))
                .collect()
        } else {
            Vec::new()
        };
        preflight_tun_capability(&prepared.path, enable_tun)?;
        // Kept out of the arm so a committed core switch can report what the
        // conversion lost, like the apply path does.
        let mut converted_parts: Option<SingboxParts> = None;
        match prepared.kind {
            CoreKind::SingBox => {
                let yaml = ManagerInner::active_profile_yaml().await?;
                let (_, parts) =
                    ManagerInner::write_singbox_assembled_to(&self.config_dir, yaml.as_deref(), enable_tun, &candidate)
                        .await?;
                validate_guided_conversion(&parts.conversion)?;
                converted_parts = Some(parts);
                crate::runtime_config::prevalidate_singbox_config(&prepared.path, &candidate)
                    .await
                    .map_err(anyhow::Error::msg)?;
            }
            CoreKind::Mihomo => {
                let yaml = ManagerInner::active_profile_yaml().await?;
                let mut config = match yaml {
                    Some(yaml) if crate::subscribe::from_url::is_singbox_json_profile(&yaml) => {
                        anyhow::bail!("Selected native sing-box JSON has no lossless Clash conversion; select a Clash subscription before switching to mihomo");
                    }
                    Some(yaml) => serde_yaml_ng::from_str::<serde_yaml_ng::Mapping>(&yaml)
                        .context("Selected native sing-box JSON has no lossless Clash conversion; select a Clash subscription before switching to mihomo")?,
                    None => clash_verge_core::config::IClashTemp::new().await.0,
                };
                crate::enhance::apply_verge_ports(&mut config).await;
                let config = crate::enhance::prepare_runtime_config(config, enable_tun);
                std::fs::write(&candidate, serde_yaml_ng::to_string(&config)?)?;
            }
        }
        let target_selections = guided_target_selections(prepared.kind, &candidate, &selections)?;
        if super::ownership::gui_process_running() {
            super::gui_isolation::check(
                &self.config_dir,
                &self.socket_path,
                if was_running && self.owns_child() {
                    self.pid()
                } else {
                    None
                },
                Some((&candidate, prepared.kind)),
            )?;
        }
        let target_api = api_for_core(
            prepared.kind,
            &self.socket_path,
            self.singbox_controller,
            controller_secret_from_config(Some(&candidate))?,
        )?;
        check_guided_cancel(cancelled)?;
        self.guided_owner_check(super::ownership::gui_process_running())?;
        if generation != self.current_generation() {
            anyhow::bail!("Core ownership/generation changed during preparation; retry. No process was stopped.");
        }
        if let Some(stages) = stages {
            let _ = stages.try_send(());
        }
        if self.inner.restarting.swap(true, Ordering::SeqCst) {
            anyhow::bail!("A core lifecycle operation is already in progress");
        }
        let _busy = LifecycleBusy(&self.inner.restarting);
        let launch = was_running || start_if_stopped;
        let stopped = AtomicBool::new(false);
        let target_started = AtomicBool::new(false);
        let outcome = orchestrate_guided_switch(
            || async {
                check_guided_cancel(cancelled)?;
                self.guided_owner_check(super::ownership::gui_process_running())?;
                self.guided_record_check(prepared.kind)?;
                if generation != self.current_generation() {
                    anyhow::bail!("Core ownership/generation changed before switching; retry");
                }
                Ok(())
            },
            was_running,
            || async {
                self.stop().await?;
                stopped.store(true, Ordering::SeqCst);
                Ok(())
            },
            || async {
                check_guided_cancel(cancelled)?;
                std::fs::rename(&candidate, &target_config)?;
                if launch {
                    ensure_guided_controller_released(prepared.kind, self.singbox_controller)?;
                    if super::ownership::gui_process_running() {
                        super::gui_isolation::check(
                            &self.config_dir,
                            &self.socket_path,
                            None,
                            Some((&target_config, prepared.kind)),
                        )?;
                    }
                    crate::enhance::ensure_mixed_port_available()
                        .await
                        .map_err(anyhow::Error::msg)?;
                    ManagerInner::spawn_core_as(
                        prepared.kind,
                        &prepared.path,
                        &prepared.version,
                        &prepared.source,
                        &self.config_dir,
                        &self.socket_path,
                        Some(target_config.clone()),
                        self.inner(),
                        false,
                    )
                    .await?;
                    target_started.store(true, Ordering::SeqCst);
                    for (group, node) in &target_selections {
                        target_api.select_proxy(group, node).await?;
                    }
                }
                Ok(())
            },
            || async {
                check_guided_cancel(cancelled)?;
                self.guided_owner_check(super::ownership::gui_process_running())?;
                if launch && (self.pid().is_none() || self.state() != CoreState::Running) {
                    anyhow::bail!("Prepared target exited before the selection could be committed");
                }
                // Fresh read retains all unknown YAML fields and unrelated edits.
                let mut config = clash_verge_core::config::IVerge::new().await;
                config.proxy_core = Some(
                    match prepared.kind {
                        CoreKind::Mihomo => "mihomo",
                        CoreKind::SingBox => "singbox",
                    }
                    .into(),
                );
                config.save_file().await?;
                check_guided_cancel(cancelled)?;
                if prepared.kind == CoreKind::SingBox {
                    super::ownership::write_ownership_marker_at(&home, "singbox")?;
                } else {
                    let marker = home.join(super::ownership::OWNERSHIP_MARKER);
                    match std::fs::remove_file(marker) {
                        Ok(()) => {}
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                        Err(e) => return Err(e.into()),
                    }
                }
                check_guided_cancel(cancelled)?;
                self.inner.set_core_kind(prepared.kind);
                *self.inner.resolved_binary.lock() = Some(prepared.path.clone());
                if !launch {
                    *self.inner.state.lock() = CoreState::Stopped;
                }
                Ok(())
            },
            || async {
                if target_started.load(Ordering::SeqCst) {
                    self.stop().await?;
                }
                files.restore()?;
                self.inner.set_core_kind(old_kind);
                *self.inner.resolved_binary.lock() = old_binary.clone();
                if stopped.load(Ordering::SeqCst) && was_running {
                    let old_binary = old_binary.as_ref().context("Old executable missing for rollback")?;
                    ManagerInner::spawn_core_as(
                        old_kind,
                        old_binary,
                        "",
                        "rollback",
                        &self.config_dir,
                        &self.socket_path,
                        None,
                        self.inner(),
                        false,
                    )
                    .await?;
                    for (group, node) in &selections {
                        self.api().select_proxy(group, node).await?;
                    }
                }
                Ok(())
            },
        )
        .await;
        if let Err(error) = &outcome
            && self.pid().is_none()
            && stopped.load(Ordering::SeqCst)
        {
            *self.inner.state.lock() = CoreState::Error(error.to_string());
        }
        // Only a committed switch may claim losses; a rolled-back one must
        // leave the previous report standing.
        if outcome.is_ok()
            && let Some(parts) = converted_parts
        {
            crate::runtime_config::record_singbox_degradation(&parts).await;
        }
        outcome
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
    /// A normal cold start: regenerate the runtime config from the active
    /// profile. Equivalent to `start_with(SupervisorLaunch::Regenerate)`.
    pub async fn start(&self) -> anyhow::Result<binary::ResolvedMihomo> {
        self.start_with(crate::commands::start::SupervisorLaunch::Regenerate)
            .await
    }

    /// Cold start with an explicit launch mode (A5, reviewer).
    ///
    /// `UseExistingConfig` is what the foreground supervisor uses when it was
    /// launched to serve a specific runtime config — the apply's verified
    /// candidate, or the config a failed apply restored — instead of
    /// generating a fresh one from whatever the profile says now.
    pub async fn start_with(
        &self,
        mode: crate::commands::start::SupervisorLaunch,
    ) -> anyhow::Result<binary::ResolvedMihomo> {
        // Sing-box cold start has its own resolve→preflight→config→spawn
        // pipeline; the mihomo path below preserves owner main's pidfile
        // guards so an adopted core always wins over a re-spawn attempt.
        if self.core_kind() == CoreKind::SingBox {
            // P0 leftover: cross-kind pidfile guard BEFORE delegating to
            // start_singbox. The sing-box TCP foreign-controller guard only
            // checks 127.0.0.1:9090, which mihomo does not bind, so without
            // this disk-side check a running mihomo could be bypassed and
            // `spawn_core` would clobber the mihomo record with kind=SingBox
            // — leaving the mihomo supervisor with a stale pidfile and any
            // cross-process `stop` targeting the wrong pid.
            check_cross_kind_record(&self.socket_path, self.core_kind())?;
            return self.start_singbox(mode).await;
        }
        // P0 leftover: same guard for the mihomo branch. The Unix-socket
        // foreign-controller guard below also catches a running mihomo,
        // but the disk check is the single source of truth and keeps the
        // behaviour symmetrical for both directions.
        check_cross_kind_record(&self.socket_path, self.core_kind())?;
        // Owner main's pidfile/alive guard: if we already track a live core
        // (whether we spawned it or adopted it), refuse to start another.
        if let Some(pid) = self.pid()
            && pidfile::is_running(pid)
        {
            anyhow::bail!("mihomo is already running (pid {pid}); use `restart` to replace it");
        }
        // Owner main's foreign-controller guard: no usable pid record but
        // something already serves the controller — a second core would
        // only fail its binds.
        if self.api().version().await.is_ok() {
            anyhow::bail!(
                "a mihomo core already answers on {} without a clash-verge-cli pid record; \
stop it where it was started",
                self.socket_path.display()
            );
        }
        // Resolve first so a resolve/preflight failure leaves any running
        // core untouched; only then stop the tracked predecessor (re-pressing
        // `s` must never stack a second child on the same ports/socket).
        let resolved = Self::resolve_and_preflight().await.context("failed to start mihomo")?;

        self.stop_tracked_predecessor().await?;

        // With our own core stopped, any remaining holder of the mixed port is
        // a foreign process (typically the Clash Verge GUI's root service on its
        // own 7897 default). Fail with guidance instead of spawning a core that
        // cannot bind and letting both sides flap.
        crate::enhance::ensure_mixed_port_available()
            .await
            .map_err(anyhow::Error::msg)?;

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
            // Tell a supervisor in another process (a TUI, `start
            // --foreground`) that this exit is intended, so it does not
            // auto-restart the core.
            let intent = pidfile::stop_intent_path_for(&self.socket_path);
            if !self.owns_child()
                && let Err(error) = pidfile::mark_stop_intent(&intent, pid)
            {
                tracing::warn!(target: "mihomo", "failed to record the stop intent: {error}");
            }
            let stopped = signal::graceful_stop_by_pid(pid).await;
            // Normally consumed by the supervisor's watcher; clear it when no
            // supervisor was left to read it.
            pidfile::take_stop_intent(&intent, pid);
            stopped?;
            pidfile::remove_if(&pidfile::path_for(&self.socket_path), pid);
        }
        // No PID — already stopped or never started (idempotent).

        *self.inner.pid.lock() = None;
        {
            let mut state = self.inner.state.lock();
            *state = CoreState::Stopped;
        }
        // Never leave the desktop pointing at the port we just closed.
        crate::sys_proxy::release_on_core_stop().await;

        self.inner.send_action(Action::CoreExited(0)).await;

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
        self.restart_with(crate::commands::start::SupervisorLaunch::Regenerate)
            .await
    }

    /// Restart with an explicit launch mode (A5, reviewer).
    ///
    /// Both authorization entry points' OWNED branches route through here
    /// WITH their mode: an owned child restarts in place, and a recovery
    /// (`UseExistingConfig`) must serve the config the transaction restored —
    /// not a regeneration from the newer active profile.
    pub async fn restart_with(
        &self,
        mode: crate::commands::start::SupervisorLaunch,
    ) -> anyhow::Result<binary::ResolvedMihomo> {
        // Test seam (A5): an owned restart would resolve a real core binary
        // and spawn it; the hook replaces that whole step so a test can
        // observe the LAUNCH MODE an owned restart was asked for — including
        // the rollback recovery, which must not silently regenerate.
        #[cfg(test)]
        if let Some(hook) = owned_restart_hook() {
            hook(mode)?;
            return Ok(binary::ResolvedMihomo {
                path: std::path::PathBuf::from("/bin/true"),
                source: binary::MihomoBinarySource::System,
                version: "owned-restart-fixture".to_string(),
            });
        }
        if self.inner.restarting.swap(true, std::sync::atomic::Ordering::SeqCst) {
            anyhow::bail!("a core restart is already in progress");
        }
        struct RestartGuard<'a>(&'a AtomicBool);
        impl Drop for RestartGuard<'_> {
            fn drop(&mut self) {
                self.0.store(false, std::sync::atomic::Ordering::SeqCst);
            }
        }
        let _restart_guard = RestartGuard(&self.inner.restarting);
        if self.inner.core_kind() == CoreKind::SingBox {
            return self.restart_singbox(mode).await;
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

    /// Sing-box cold start: foreign-ctrl guard → resolve → preflight →
    /// stop tracked predecessor → port → generate config → spawn.
    ///
    /// The order matches the mihomo `start()` branch: resolve and the
    /// TUN capability preflight MUST run before any stop so that a
    /// resolve/preflight failure leaves the currently running core
    /// untouched (reviewer P0-1). The mihomo branch already enforces
    /// this; the sing-box branch did not — fixing it removes the one
    /// ordering hazard where `s` could knock the core offline and
    /// then fail to bring up a replacement.
    ///
    /// Production wiring delegates to [`orchestrate_start_singbox`] so
    /// the exact order is enforced by a higher-order helper that the
    /// unit tests drive with fake steps.
    async fn start_singbox(
        &self,
        mode: crate::commands::start::SupervisorLaunch,
    ) -> anyhow::Result<binary::ResolvedMihomo> {
        use super::singbox_binary::SingboxBinarySource;
        self.reset_restart_history();

        let resolved = orchestrate_start_singbox(
            || async {
                // P1-2 (reviewer): foreign TCP controller guard. The mihomo
                // start() branch checks `self.api().version().await.is_ok()`
                // and bails before any work; the sing-box branch was missing
                // the parallel guard and would happily resolve + kill the
                // predecessor only to fail its bind, flapping both sides.
                if self.api().version().await.is_ok() {
                    anyhow::bail!(
                        "a sing-box core already answers on {} without a clash-verge-cli pid record; \
stop it where it was started",
                        self.singbox_controller
                    );
                }
                Ok(())
            },
            Self::resolve_and_preflight_singbox,
            // resolve_and_preflight_singbox already runs the preflight; the
            // pipeline slot stays so the helper signature mirrors
            // orchestrate_restart and the order is unit-testable.
            |_resolved| async move { Ok(()) },
            || async { self.stop_tracked_predecessor().await },
            || async {
                crate::enhance::ensure_mixed_port_available()
                    .await
                    .map_err(anyhow::Error::msg)
            },
            |res| {
                // Clone the owned pieces so the future does not borrow
                // across an await point (the mihomo `restart_singbox`
                // branch does this for the same reason).
                let (path, version, source, config_dir, socket_path, inner) = (
                    res.path.clone(),
                    res.version.clone(),
                    res.source,
                    self.config_dir.clone(),
                    self.socket_path.clone(),
                    Arc::clone(&self.inner),
                );
                async move {
                    // A5 (reviewer): the launch mode travels with the
                    // launch — a supervisor told to serve the config on
                    // disk must not regenerate it from the active profile.
                    let config_path = ManagerInner::runtime_config_for_start(&inner, &config_dir, mode).await?;
                    ManagerInner::spawn_core(
                        &path,
                        &version,
                        source.as_str(),
                        &config_dir,
                        &socket_path,
                        Some(config_path),
                        inner,
                    )
                    .await
                    .context("failed to spawn sing-box")?;
                    Ok(())
                }
            },
        )
        .await?;

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

    /// Mirror of [`resolve_and_preflight`] for the sing-box branch —
    /// resolves the binary and runs the read-only TUN capability check,
    /// without ever touching the running core.
    async fn resolve_and_preflight_singbox() -> anyhow::Result<super::singbox_binary::ResolvedSingBox> {
        let resolved = super::singbox_binary::resolve_or_install()
            .await
            .context("failed to resolve or auto-install sing-box core")?;
        let tun_enabled = runtime_tun_enabled().await.unwrap_or(false);
        preflight_tun_capability(&resolved.path, tun_enabled)?;
        Ok(resolved)
    }

    /// Stop a core this manager tracks as running, when one exists.
    /// `stop()` is idempotent when no pid is tracked.
    async fn stop_tracked_predecessor(&self) -> anyhow::Result<()> {
        if self.pid().is_some() {
            tracing::warn!(
                target: "mihomo",
                "start: a core (pid {:?}) is already running — stopping it first",
                self.pid()
            );
            self.stop().await?;
        }
        Ok(())
    }

    async fn restart_singbox(
        &self,
        mode: crate::commands::start::SupervisorLaunch,
    ) -> anyhow::Result<binary::ResolvedMihomo> {
        use super::singbox_binary::SingboxBinarySource;
        self.reset_restart_history();
        let resolved = super::singbox_binary::resolve_or_install()
            .await
            .context("failed to resolve or auto-install sing-box core")?;
        let tun_enabled = runtime_tun_enabled().await.unwrap_or(false);
        preflight_tun_capability(&resolved.path, tun_enabled)?;
        // P1 (reviewer): prepare the runtime config BEFORE the old core goes
        // away. A normal start/restart REGENERATES it (and may overwrite a
        // stale/missing file); only a recovery start — which must serve the
        // restored file verbatim — requires it to already exist. Doing this
        // first also means a generation failure (an unconvertible profile, a
        // full disk) leaves the running service untouched instead of
        // dropping it and then failing to start a replacement.
        let config_path = ManagerInner::runtime_config_for_start(&self.inner, &self.config_dir, mode)
            .await
            .context("failed to prepare the sing-box runtime config; the running core was left untouched")?;
        self.stop().await.context("failed to stop running sing-box")?;
        crate::enhance::ensure_mixed_port_available()
            .await
            .map_err(anyhow::Error::msg)?;
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

    /// Replace the running core with a fresh one that picks up a
    /// regenerated configuration, when this process did **not** spawn the
    /// running core (#55).
    ///
    /// sing-box has no hot reload, so applying a profile means restarting
    /// it. `start` runs the core under a detached `start --foreground`
    /// supervisor, so in every *later* CLI invocation (and in a TUI
    /// attached to a service-started core) `owns_child()` is false and the
    /// old check refused with "externally managed core" — making
    /// `profile use` / `profile update --reload` impossible.
    ///
    /// A core with a verified pid record (see
    /// [`pidfile::read_live_for_kind`], which also validates the executable
    /// and the controller endpoint) *is* ours to replace: the old
    /// supervisor stands down through the stop-intent marker written by
    /// [`MihomoManager::stop`] and the replacement is handed to a new
    /// detached supervisor. Only a core with no pid record at all — one
    /// somebody else started — is refused.
    pub async fn restart_through_supervisor(&self) -> anyhow::Result<()> {
        self.restart_through_supervisor_with(crate::commands::start::SupervisorLaunch::Regenerate)
            .await
    }

    /// [`restart_through_supervisor`] with an explicit launch mode; a
    /// recovery asks the replacement to serve the config already on disk.
    pub async fn restart_through_supervisor_with(
        &self,
        mode: crate::commands::start::SupervisorLaunch,
    ) -> anyhow::Result<()> {
        let authorization = self.capture_restart_authorization().await?;
        self.launch_replacement(authorization.as_ref(), mode).await
    }

    /// One replacement launch, given the authorization captured at
    /// transaction entry (A5, reviewer).
    ///
    /// - `None` — an OWNED child: restart it in place, honouring `mode`.
    ///   The authorization was taken at entry precisely so this half does not
    ///   re-run [`supervisor_restart_policy`]: a first failure clears the
    ///   pid/ownership, and a recovery that asked for permission again would
    ///   be refused by the very no-record rule that only ever meant "this
    ///   core belongs to somebody else".
    /// - `Some(receipt)` — an ADOPTED core: replace it through a detached
    ///   supervisor launched with `mode`, using the receipt as the proof this
    ///   transaction may do so.
    pub async fn launch_replacement(
        &self,
        authorization: Option<&RestartAuthorization>,
        mode: crate::commands::start::SupervisorLaunch,
    ) -> anyhow::Result<()> {
        match authorization {
            None => self.restart_with(mode).await.map(|_| ()),
            Some(receipt) => self.apply_restart_authorization_with(receipt, mode).await,
        }
    }

    /// Validate the restart authorization for THIS manager right now, and
    /// capture what it was granted for.
    ///
    /// `None` means "an owned child" (restarted in place, no record
    /// involved). `Some(_)` means the core is ours to replace through a
    /// detached supervisor, and the returned [`RestartAuthorization`] is the
    /// proof: the running kind, the recorded pid and the recorded
    /// executable, all checked at this instant.
    pub async fn capture_restart_authorization(&self) -> anyhow::Result<Option<RestartAuthorization>> {
        let configured = configured_core_kind().await;
        supervisor_restart_policy(self.owns_child(), self.pid(), self.core_kind(), configured)?;
        if self.owns_child() {
            return Ok(None);
        }
        let socket = self.socket_path.clone();
        let record = pidfile::read_record(&pidfile::path_for(&socket));
        Ok(Some(RestartAuthorization {
            kind: self.core_kind(),
            pid: self
                .pid()
                .ok_or_else(|| anyhow::anyhow!("the authorized core lost its pid record mid-transaction"))?,
            exe: record.as_ref().and_then(|record| record.exe.clone()),
        }))
    }

    /// Replace the authorized core with a fresh supervisor-launched one.
    ///
    /// P1 (reviewer): this is the recovery half of a transaction. The first
    /// restart attempt stops the adopted core — which clears the manager's
    /// pid — and then fails to launch; the rollback retry cannot use
    /// [`Self::restart_through_supervisor`] any more because that re-runs
    /// the policy, and a core with no pid record is somebody else's. The
    /// authorization captured at entry is the receipt: it is honoured for
    /// this call only, so the global "no record → refuse" rule is untouched.
    pub async fn apply_restart_authorization(&self, authorization: &RestartAuthorization) -> anyhow::Result<()> {
        self.apply_restart_authorization_with(authorization, crate::commands::start::SupervisorLaunch::Regenerate)
            .await
    }

    /// [`apply_restart_authorization`] with an explicit launch mode.
    ///
    /// `UseExistingConfig` is the RECOVERY half of the apply transaction:
    /// the previous config has already been restored on disk, so the
    /// replacement supervisor is told to serve that file instead of
    /// regenerating it from the active profile. Without this the recovery
    /// re-generated the newer profile the apply had just failed to install,
    /// and the "restored" service was the config the user was rejecting.
    ///
    /// A5 (reviewer): the OWNED branch forwards `mode` too — the owned
    /// restart is a launch like any other and must not silently regenerate
    /// what the transaction just restored (or re-run the chain that produced
    /// the verified candidate).
    pub async fn apply_restart_authorization_with(
        &self,
        authorization: &RestartAuthorization,
        mode: crate::commands::start::SupervisorLaunch,
    ) -> anyhow::Result<()> {
        if self.owns_child() {
            return self.restart_with(mode).await.map(|_| ());
        }
        self.authorize_still_ours(authorization)?;
        // A partially started previous attempt (supervisor alive, core not
        // yet answering) must be cleared first, or the recovery would leave
        // two supervisors fighting over one controller socket.
        self.stop().await.context("failed to stop the recorded core")?;
        let log = crate::commands::start::supervisor_log_path(&self.config_dir);
        let launched_at = std::time::SystemTime::now();
        let mut supervisor = crate::commands::start::launch_supervisor_via(&self.config_dir, &log, mode)?;
        let ownership = LaunchOwnership::new(supervisor.id());
        // Readiness and identity verification share ONE failure path: once
        // the supervisor has been launched, every failure — a timeout, an
        // early exit, or a core that answers but cannot be attributed to this
        // restart — must take the supervisor and the core it spawned back
        // down. Leaving the new core alive would hold the controller socket
        // and the mixed port while the caller rolls the config back to A.
        let outcome = match crate::commands::start::wait_until_ready(self, &mut supervisor, &log).await {
            Ok(()) => self.verify_replacement_core(authorization, launched_at),
            Err(error) => Err(error),
        };
        if let Err(error) = outcome {
            self.cleanup_failed_launch(&ownership, &mut supervisor).await;
            return Err(error);
        }
        // The replacement belongs to the new supervisor; adopt its pid
        // record so the rest of this process sees the running core.
        self.adopt_running_core();
        Ok(())
    }

    /// Stop everything THIS launch created, and nothing else.
    ///
    /// P1 (reviewer): the old cleanup read the SHARED pid record and
    /// signalled the pid it named. That record is written by whichever
    /// instance touched it last, so a concurrent launch that overwrote it —
    /// or a recycled pid — made this transaction kill a process that has
    /// nothing to do with it. Cleanup is bound to the supervisor handle this
    /// launch captured: only processes that supervisor forked (read from
    /// `/proc` BEFORE the supervisor is killed, since killing it reparents
    /// them) are stopped.
    ///
    /// A5 (reviewer): this is the ONE launch-failure cleanup. Every launch
    /// that can fail after a supervisor exists — the apply transaction's
    /// restart AND the plain `start` in `commands::start::run` — routes its
    /// failure here, so no launch path can leave a supervisor (or the core it
    /// forked) holding the controller socket.
    pub(crate) async fn cleanup_failed_launch(
        &self,
        ownership: &LaunchOwnership,
        supervisor: &mut std::process::Child,
    ) {
        // Enumerate first: once the supervisor is gone its children are
        // reparented to init and can no longer be attributed to it.
        let mut owned = ownership.descendant_pids();
        crate::commands::start::reap_supervisor(supervisor);
        // The record is only ever a hint about which pid to look at, never
        // the proof: the process still has to be one this launch forked.
        let path = pidfile::path_for(&self.socket_path);
        if let Some(record) = pidfile::read_record(&path)
            && owned.contains(&record.pid)
        {
            owned.retain(|pid| *pid != record.pid);
            owned.push(record.pid);
        }
        if owned.is_empty() {
            tracing::warn!(
                target: "mihomo",
                "no process could be attributed to the failed launch (supervisor pid {}); nothing was stopped",
                ownership.supervisor_pid
            );
        }
        for pid in owned {
            match signal::graceful_stop_by_pid(pid).await {
                Ok(()) => {
                    pidfile::remove_if(&path, pid);
                    tracing::info!(
                        target: "mihomo",
                        "stopped pid {pid} left behind by the failed launch of supervisor {}",
                        ownership.supervisor_pid
                    );
                }
                Err(error) => {
                    tracing::warn!(
                        target: "mihomo",
                        "failed to stop pid {pid} left behind by the failed launch of supervisor {}: {error}",
                        ownership.supervisor_pid
                    );
                }
            }
        }
    }

    /// Require the core answering the controller to be THIS replacement.
    ///
    /// Fails when the pid record on disk is not a live core of this kind, or
    /// when it names the core the transaction was replacing / a core that
    /// started before the replacement was launched. "Something answers on the
    /// port with a plausible version" is exactly what a predecessor (or a
    /// foreign process that took the socket during the failure window) looks
    /// like, and accepting it reports a recovery that never happened.
    ///
    /// The record is read raw (`read_record`) rather than adopted: full
    /// adoption also validates the process shape (`/proc` cmdline/exe), which
    /// is [`Self::adopt_running_core`]'s job — here only the identity of the
    /// record itself decides which core answered.
    fn verify_replacement_core(
        &self,
        authorization: &RestartAuthorization,
        launched_at: std::time::SystemTime,
    ) -> anyhow::Result<()> {
        let path = pidfile::path_for(&self.socket_path);
        let record = pidfile::read_record(&path)
            .filter(|record| record.kind == self.core_kind() && pidfile::is_running(record.pid))
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "the replacement core answered on {} but left no live {} pid record; \
it cannot be attributed to this restart",
                    self.socket_path.display(),
                    self.core_kind().as_str()
                )
            })?;
        if record.pid == authorization.pid {
            anyhow::bail!(
                "pid {} on {} is still the core this transaction was replacing; the replacement \
never started",
                record.pid,
                self.socket_path.display()
            );
        }
        // Core records carry whole unix seconds, so allow a small truncation
        // tolerance before calling the record stale.
        let cutoff = chrono::DateTime::<Utc>::from(launched_at) - chrono::Duration::seconds(2);
        if record.started_at().is_some_and(|started| started < cutoff) {
            anyhow::bail!(
                "the core answering on {} (pid {}) predates the replacement this restart launched; \
the recovery is not serving its own core",
                self.socket_path.display(),
                record.pid
            );
        }
        Ok(())
    }

    /// Re-check an authorization against what is on disk right now. A
    /// foreign core that took the controller socket while the transaction
    /// was failing is refused — the receipt only covers the core it named.
    fn authorize_still_ours(&self, authorization: &RestartAuthorization) -> anyhow::Result<()> {
        let path = pidfile::path_for(&self.socket_path);
        match pidfile::read_record(&path) {
            Some(record) if record.kind == authorization.kind => {
                if record.pid != authorization.pid {
                    // Only a core with a *different* record may be replaceable
                    // here: that is our own replacement from an earlier
                    // attempt in the same transaction.
                    let same_exe = record.exe == authorization.exe;
                    if !same_exe {
                        anyhow::bail!(
                            "a {} core (pid {}) now holds {}; it is not the core this transaction was \
authorized to replace — stop it where it was started",
                            record.kind.as_str(),
                            record.pid,
                            self.socket_path.display()
                        );
                    }
                }
                Ok(())
            }
            // No record at all: the authorized core is gone, which is exactly
            // the state a failed first attempt leaves behind.
            None => Ok(()),
            Some(record) => anyhow::bail!(
                "a {} core (pid {}) is recorded for {}, not the {} this transaction was authorized \
to replace; switch the core first",
                record.kind.as_str(),
                record.pid,
                self.socket_path.display(),
                authorization.kind.as_str(),
            ),
        }
    }

    /// CLI `core use` while nothing is running: persist the selection
    /// (verge.yaml `proxy_core` plus the sing-box ownership marker) after
    /// the same read-only stopped-state preflight the TUI guided switch
    /// uses, so both surfaces share one transaction (#56).
    pub fn select_core_while_stopped(&self, target: CoreKind) -> anyhow::Result<()> {
        let generation = self.current_generation();
        let old_kind = self.core_kind();
        self.commit_stopped_selection(generation, old_kind, target, &AtomicBool::new(false))
    }

    /// Return CoreStatus with live version info if mihomo is running.
    /// Status from this process's own view, without probing the controller
    /// (`version` is `None`).
    pub fn local_status(&self) -> CoreStatus {
        CoreStatus {
            state: self.state(),
            pid: self.pid(),
            uptime_secs: self.uptime().map(|d| d.num_seconds()),
            version: None,
            socket_path: self.socket_path.clone(),
            config_dir: self.config_dir.clone(),
        }
    }

    pub async fn status(&self) -> CoreStatus {
        let mut status = self.local_status();
        status.version = self.api().version().await.ok().map(|v| v.version);
        // A GUI-owned Mihomo process is not a child of this manager, but its
        // configured controller is still authoritative for CLI status.
        status.state = observed_state(status.state, status.version.as_deref());
        status
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

/// P1-1 (reviewer): clear the in-memory runtime state after a spawn
/// failure so the next `start` does not see a stale pid / started_at
/// from a half-alive core that was just killed for failing the readiness
/// probe. Without this, a subsequent invocation would observe a `pid`
/// that is no longer alive and either bail on the adopted-core guard
/// (because pidfile::is_running returns false for a reaped pid, but
/// `self.pid()` returns `Some`) or transition through inconsistent
/// states across retries.
///
/// Pure helper (no awaits, no I/O beyond pidfile removal). Shared by
/// `spawn_core`'s readiness-failure path and unit-testable in isolation.
pub(crate) fn rollback_failed_spawn(inner: &ManagerInner, socket_path: &Path, pid: u32, error_message: String) {
    pidfile::remove_if(&pidfile::path_for(socket_path), pid);
    inner.owns_child.store(false, std::sync::atomic::Ordering::SeqCst);
    *inner.pid.lock() = None;
    *inner.started_at.lock() = None;
    *inner.state.lock() = CoreState::Error(error_message);
}

/// P0 leftover guard: refuse to start a core of `self_kind` when a
/// different-kind record is on disk AND its pid is still alive.
///
/// Both cores share the `mihomo.pid` filename, so the on-disk record is
/// the only thing that disambiguates "a mihomo is running" from "a
/// sing-box is running" once two processes or two cores have touched
/// the same data dir. Without this guard, the sing-box `start` path
/// would bypass the running mihomo (the TCP foreign-controller only
/// probes 127.0.0.1:9090, which mihomo does not bind) and
/// `spawn_core` would overwrite the mihomo record with kind=SingBox,
/// leaving the mihomo supervisor with a stale pidfile and any
/// cross-process `stop` targeting the wrong pid.
///
/// Pure helper: only reads the on-disk record + checks `/proc/<pid>/stat`
/// liveness; no manager state, no awaits. Unit-testable in isolation by
/// staging a temporary record and a known-dead pid.
/// The proof a configuration transaction was granted to replace a core
/// this process did not spawn.
///
/// Captured once, at transaction entry, by
/// [`MihomoManager::capture_restart_authorization`]; the same transaction
/// then carries it into the rollback recovery after its first launch failed
/// and cleared the pid. It is deliberately NOT a way to relax
/// [`supervisor_restart_policy`]: the policy still refuses any core with no
/// record at capture time, and this receipt only names the one core the
/// policy already accepted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestartAuthorization {
    /// Core kind the running record carried when the authorization was taken.
    pub kind: CoreKind,
    /// Pid the manager had adopted.
    pub pid: u32,
    /// Executable the record named, when it named one (#54).
    pub exe: Option<String>,
}

/// Whether a configuration restart may replace the running core (#55).
///
/// - an owned child is always restartable in place;
/// - a core with a verified pid record is restartable through a detached
///   supervisor, but only while the configured `proxy_core` still names
///   the running kind — otherwise the supervisor would bring up a
///   *different* core than the one being reconfigured (#56: switch first);
/// - a core with no pid record belongs to somebody else and is refused.
pub(crate) fn supervisor_restart_policy(
    owns_child: bool,
    pid: Option<u32>,
    running: CoreKind,
    configured: CoreKind,
) -> anyhow::Result<()> {
    if owns_child {
        return Ok(());
    }
    if pid.is_none() {
        anyhow::bail!(
            "cannot apply settings to a {} core with no clash-verge-cli pid record; \
it was started outside this CLI — stop it where it was started",
            running.as_str()
        );
    }
    if running != configured {
        anyhow::bail!(
            "a {} core is running but verge.yaml selects {}; switch the core first \
(`clash-verge-cli core use {}`) so the replacement matches the running one",
            running.as_str(),
            configured.as_str(),
            configured.as_str()
        );
    }
    Ok(())
}

/// The core `verge.yaml` selects (`proxy_core`), with the same
/// fail-safe-to-mihomo validation `IVerge` applies. Read on every
/// lifecycle decision so a core switch written by the TUI, a restore or a
/// hand edit is always honoured (#56).
pub async fn configured_core_kind() -> CoreKind {
    let verge = clash_verge_core::config::IVerge::new().await;
    if verge.get_valid_proxy_core() == "singbox" {
        CoreKind::SingBox
    } else {
        CoreKind::Mihomo
    }
}

pub(crate) fn check_cross_kind_record(socket_path: &Path, self_kind: CoreKind) -> anyhow::Result<()> {
    let record_path = pidfile::path_for(socket_path);
    let Some(record) = pidfile::read_record(&record_path) else {
        return Ok(());
    };
    // Same kind: the existing in-memory `self.pid()` and foreign-controller
    // guards handle the rest.
    if record.kind == self_kind {
        return Ok(());
    }
    // Different kind, but the recorded pid is dead: the record is stale
    // (kernel reused it or the original process died). The new spawn will
    // overwrite it harmlessly.
    if !pidfile::is_running(record.pid) {
        return Ok(());
    }
    anyhow::bail!(
        "a {} core (pid {}) is already recorded for this controller socket; \
stop it first before starting {}",
        record.kind.as_str(),
        record.pid,
        self_kind.as_str()
    )
}

fn check_guided_cancel(cancelled: &AtomicBool) -> anyhow::Result<()> {
    if cancelled.load(Ordering::SeqCst) {
        anyhow::bail!("Core operation cancelled; previous selection retained");
    }
    Ok(())
}

fn check_guided_record(
    record_pid: Option<u32>,
    record_live: bool,
    tracked_pid: Option<u32>,
    owns_child: bool,
    socket_bound: bool,
) -> anyhow::Result<()> {
    if tracked_pid.is_some() && (!owns_child || record_pid != tracked_pid || !record_live) {
        anyhow::bail!(
            "Owned core record changed or disappeared; no lifecycle mutation is allowed. Retry after checking the owner."
        );
    }
    if tracked_pid.is_none() && record_live {
        anyhow::bail!(
            "A core belongs to another CLI supervisor; manage it through its owner. No process or controller was contacted."
        );
    }
    if tracked_pid.is_none() && socket_bound {
        anyhow::bail!(
            "An external controller socket has no CLI ownership record; manage it through its owner. The socket was left untouched."
        );
    }
    Ok(())
}

fn controller_secret_from_config(path: Option<&Path>) -> anyhow::Result<String> {
    let Some(path) = path else {
        return Ok(String::new());
    };
    // #49: a missing config file is a fresh install, not an error. The caller
    // has already composed/persisted config.yaml via `resolve_controller_secret`,
    // and an absent per-core runtime config simply means "no secret here".
    if !path.exists() {
        return Ok(String::new());
    }
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read the runtime config at {}", path.display()))?;
    let value: serde_json::Value = if text.trim_start().starts_with('{') {
        serde_json::from_str(&text)?
    } else {
        serde_yaml_ng::from_str(&text)?
    };
    Ok(value
        .get("secret")
        .or_else(|| value.pointer("/experimental/clash_api/secret"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string())
}

fn ensure_guided_controller_released(kind: CoreKind, controller: std::net::SocketAddr) -> anyhow::Result<()> {
    if kind == CoreKind::SingBox {
        std::net::TcpListener::bind(controller)
            .context("target sing-box controller is still occupied after the owned stop; no target was started")?;
    }
    Ok(())
}

pub(crate) fn configured_singbox_controller(config: &serde_yaml_ng::Mapping) -> anyhow::Result<std::net::SocketAddr> {
    let address = config
        .get("external-controller")
        .and_then(serde_yaml_ng::Value::as_str)
        .filter(|value| !value.is_empty())
        .unwrap_or("127.0.0.1:9090");
    let address: std::net::SocketAddr = address.parse().context("invalid sing-box controller address")?;
    // Sing-box retains its existing fixed IPv4 loopback host. Share the saved
    // controller port without restricting mihomo's independent Unix/TCP setup.
    Ok(std::net::SocketAddr::from((
        [127, 0, 0, 1],
        if address.port() == 0 { 9090 } else { address.port() },
    )))
}

fn api_for_core(
    kind: CoreKind,
    socket: &Path,
    controller: std::net::SocketAddr,
    secret: String,
) -> anyhow::Result<MihomoApi> {
    MihomoApi::with_transport(
        match kind {
            CoreKind::Mihomo => crate::mihomo_api::Transport::UnixSocket(socket.to_path_buf()),
            CoreKind::SingBox => crate::mihomo_api::Transport::Tcp(controller),
        },
        secret,
    )
    .map(|api| api.for_core(kind))
    .map_err(Into::into)
}

fn guided_target_selections(
    kind: CoreKind,
    config: &Path,
    selections: &[(String, String)],
) -> anyhow::Result<Vec<(String, String)>> {
    if selections.is_empty() {
        return Ok(Vec::new());
    }
    let bytes = std::fs::read(config)?;
    let mut declared = std::collections::HashSet::<String>::new();
    let mut selectors = std::collections::HashMap::<String, Vec<String>>::new();
    match kind {
        CoreKind::Mihomo => {
            let value: serde_yaml_ng::Mapping = serde_yaml_ng::from_slice(&bytes)?;
            declared.extend(["DIRECT".into(), "REJECT".into()]);
            for key in ["proxies", "proxy-groups"] {
                for entry in value
                    .get(key)
                    .and_then(serde_yaml_ng::Value::as_sequence)
                    .into_iter()
                    .flatten()
                {
                    if let Some(name) = entry.get("name").and_then(serde_yaml_ng::Value::as_str) {
                        declared.insert(name.into());
                        if key == "proxy-groups"
                            && entry
                                .get("type")
                                .and_then(serde_yaml_ng::Value::as_str)
                                .is_some_and(|kind| {
                                    kind.eq_ignore_ascii_case("select") || kind.eq_ignore_ascii_case("selector")
                                })
                        {
                            let members = entry
                                .get("proxies")
                                .and_then(serde_yaml_ng::Value::as_sequence)
                                .into_iter()
                                .flatten()
                                .filter_map(serde_yaml_ng::Value::as_str)
                                .map(str::to_owned)
                                .collect();
                            selectors.insert(name.into(), members);
                        }
                    }
                }
            }
        }
        CoreKind::SingBox => {
            let value: serde_json::Value = serde_json::from_slice(&bytes)?;
            let outbounds = value["outbounds"]
                .as_array()
                .context("Prepared sing-box config has no outbounds")?;
            for entry in outbounds {
                if let Some(tag) = entry["tag"].as_str() {
                    declared.insert(tag.into());
                    if entry["type"] == "selector" {
                        let members = entry["outbounds"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter_map(serde_json::Value::as_str)
                            .map(str::to_owned)
                            .collect();
                        selectors.insert(tag.into(), members);
                    }
                }
            }
        }
    }
    let mut mapped = Vec::new();
    for (group, node) in selections {
        let members = selectors.get(group).with_context(|| {
            format!(
                "Selected group {group:?} has no compatible {} selector; old core remains running",
                kind.as_str()
            )
        })?;
        let legal = |name: &str| declared.contains(name) && members.iter().any(|member| member == name);
        // A user-defined, case-sensitive name wins over a reserved-name alias.
        let alias = match (kind, node.as_str()) {
            (CoreKind::SingBox, "DIRECT") => Some("direct"),
            (CoreKind::SingBox, "REJECT") => Some("block"),
            (CoreKind::Mihomo, "direct") => Some("DIRECT"),
            (CoreKind::Mihomo, "block") => Some("REJECT"),
            _ => None,
        };
        let target = if legal(node) {
            node.as_str()
        } else if let Some(alias) = alias.filter(|alias| legal(alias)) {
            alias
        } else {
            anyhow::bail!(
                "Selected node {node:?} has no declared member in target group {group:?}; old core remains running"
            );
        };
        mapped.push((group.clone(), target.into()));
    }
    Ok(mapped)
}

fn validate_guided_conversion(conversion: &crate::singbox::convert::ProfileConversion) -> anyhow::Result<()> {
    if !conversion.skipped.is_empty() || !conversion.degraded.is_empty() {
        anyhow::bail!(
            "Current subscription cannot be converted without losing fields. Skipped: {}; unsupported/degraded: {}. Old core remains unchanged.",
            conversion.skipped.join("; "),
            conversion.degraded.join("; ")
        );
    }
    Ok(())
}

/// Snapshot exact bytes, including unknown YAML/JSON fields. Only transaction
/// owned paths are restored; binaries are version-addressed and remain usable.
struct GuidedFileSnapshot(Vec<(PathBuf, Option<Vec<u8>>)>);

struct LifecycleBusy<'a>(&'a AtomicBool);
impl Drop for LifecycleBusy<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// Only the CLI selection and its marker participate. Parse the raw mapping
/// strictly, retaining GUI/unknown fields; stage each replacement atomically
/// and restore exact old bytes if either write or the final guard fails.
fn persist_stopped_selection(
    home: &Path,
    kind: CoreKind,
    mut guard: impl FnMut() -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    use std::io::Write as _;
    let verge = home.join("verge.yaml");
    let marker = home.join(super::ownership::OWNERSHIP_MARKER);
    let files = GuidedFileSnapshot::capture([verge.clone(), marker.clone()])?;
    let original = &files.0[0].1;
    let mut config: serde_yaml_ng::Mapping = match original {
        Some(bytes) => serde_yaml_ng::from_slice(bytes)
            .context("cannot select core: invalid verge.yaml; existing settings left untouched")?,
        None => serde_yaml_ng::Mapping::new(),
    };
    config.insert("proxy_core".into(), kind.as_str().into());
    let mut config_candidate = tempfile::NamedTempFile::new_in(home)?;
    config_candidate.write_all(serde_yaml_ng::to_string(&config)?.as_bytes())?;
    config_candidate.as_file().sync_all()?;
    let mut marker_candidate = if kind == CoreKind::SingBox {
        let mut file = tempfile::NamedTempFile::new_in(home)?;
        let record = super::ownership::OwnershipMarker {
            owner: "tui".into(),
            core: "singbox".into(),
            pid: std::process::id(),
        };
        file.write_all(&serde_json::to_vec(&record)?)?;
        file.as_file().sync_all()?;
        Some(file)
    } else {
        None
    };
    guard()?;
    // Detect another writer before publishing anything, including GUI changes.
    let fresh = GuidedFileSnapshot::capture([verge.clone(), marker.clone()])?;
    if fresh.0 != files.0 {
        anyhow::bail!("Core selection settings changed during preparation; retry");
    }
    let outcome = (|| -> anyhow::Result<()> {
        if let Some(candidate) = marker_candidate.take() {
            candidate.persist(&marker)?;
        } else {
            match std::fs::remove_file(&marker) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        guard()?;
        config_candidate.persist(&verge)?;
        guard()?;
        Ok(())
    })();
    if let Err(error) = outcome {
        return match files.restore() {
            Ok(()) => Err(error),
            Err(rollback) => Err(anyhow::anyhow!("{error:#}; selection rollback failed: {rollback:#}")),
        };
    }
    Ok(())
}

impl GuidedFileSnapshot {
    fn capture(paths: impl IntoIterator<Item = PathBuf>) -> anyhow::Result<Self> {
        let mut files = Vec::new();
        for path in paths {
            let bytes = match std::fs::read(&path) {
                Ok(bytes) => Some(bytes),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => return Err(error.into()),
            };
            files.push((path, bytes));
        }
        Ok(Self(files))
    }

    fn restore(&self) -> anyhow::Result<()> {
        use std::io::Write as _;
        for (path, previous) in &self.0 {
            if let Some(bytes) = previous {
                let mut candidate =
                    tempfile::NamedTempFile::new_in(path.parent().context("snapshot path has no parent")?)?;
                candidate.write_all(bytes)?;
                candidate.as_file().sync_all()?;
                candidate.persist(path)?;
            } else {
                match std::fs::remove_file(path) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error.into()),
                }
            }
        }
        Ok(())
    }
}

/// Existing injected orchestration extended with commit and awaited rollback.
/// Production and tests share this exact ordering, without executing a core.
async fn orchestrate_guided_switch<P, S, L, C, R>(
    recheck: impl FnOnce() -> P,
    was_running: bool,
    stop: impl FnOnce() -> S,
    launch: impl FnOnce() -> L,
    commit: impl FnOnce() -> C,
    rollback: impl FnOnce() -> R,
) -> anyhow::Result<()>
where
    P: Future<Output = anyhow::Result<()>>,
    S: Future<Output = anyhow::Result<()>>,
    L: Future<Output = anyhow::Result<()>>,
    C: Future<Output = anyhow::Result<()>>,
    R: Future<Output = anyhow::Result<()>>,
{
    recheck().await?;
    let outcome = async {
        if was_running {
            stop().await?;
        }
        launch().await?;
        commit().await
    }
    .await;
    if let Err(error) = outcome {
        return match rollback().await {
            Ok(()) => Err(error.context("Previous core selection restored")),
            Err(restore) => Err(error.context(format!("Rollback also failed: {restore}"))),
        };
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

/// Sing-box cold-start orchestration (reviewer P0-1).
///
/// Mirrors [`orchestrate_restart`] for the sing-box branch, with two
/// extra slots at the front so the production start path can plug in
/// the foreign-controller guard and (later) any other pre-resolve
/// precondition. The order is fixed and tested by
/// `start_singbox_pipeline_orders_steps_*`:
///
///   foreign → resolve → preflight → stop → port → spawn
///
/// A failure in any step short-circuits; the in-memory `inner.pid` is
/// only ever written by `spawn` (the last step). Stop failure is
/// tolerated (`let _ = ...`), matching `orchestrate_restart`'s
/// historical contract.
#[allow(clippy::too_many_arguments)]
async fn orchestrate_start_singbox<ForeignFut, ResolveFut, PreflightFut, StopFut, PortFut, SpawnFut>(
    foreign_controller_check: impl FnOnce() -> ForeignFut,
    resolve: impl FnOnce() -> ResolveFut,
    preflight: impl FnOnce(&super::singbox_binary::ResolvedSingBox) -> PreflightFut,
    stop: impl FnOnce() -> StopFut,
    port_check: impl FnOnce() -> PortFut,
    spawn: impl FnOnce(&super::singbox_binary::ResolvedSingBox) -> SpawnFut,
) -> anyhow::Result<super::singbox_binary::ResolvedSingBox>
where
    ForeignFut: Future<Output = anyhow::Result<()>>,
    ResolveFut: Future<Output = anyhow::Result<super::singbox_binary::ResolvedSingBox>>,
    PreflightFut: Future<Output = anyhow::Result<()>>,
    StopFut: Future<Output = anyhow::Result<()>>,
    PortFut: Future<Output = anyhow::Result<()>>,
    SpawnFut: Future<Output = anyhow::Result<()>>,
{
    foreign_controller_check().await?;
    let resolved = resolve().await?;
    preflight(&resolved).await?;
    let _ = stop().await;
    port_check().await?;
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
    #[tokio::test]
    async fn stopped_selection_ignores_profiles_tun_and_candidate_without_starting() {
        let home = private_socket_fixture();
        let _guard = crate::profile_store::store::tests::claim_test_app_home(home.path().to_path_buf()).await;
        std::fs::write(
            home.path().join("profiles.yaml"),
            "current: missing\nitems: [{uid: missing, type: remote, file: absent.yaml}]\n",
        )
        .unwrap();
        std::fs::write(
            home.path().join("verge.yaml"),
            "proxy_core: singbox\nenable_tun_mode: true\nfuture: {keep: yes}\n",
        )
        .unwrap();
        std::fs::write(home.path().join("config.yaml"), "previous mihomo runtime").unwrap();
        std::fs::write(home.path().join("singbox.json"), "previous singbox runtime").unwrap();
        let manager = MihomoManager::new(home.path().to_path_buf())
            .with_socket(home.path().join("controller.sock"))
            .with_singbox_controller("127.0.0.1:0".parse().unwrap())
            .with_core_kind(CoreKind::SingBox);
        let shared = manager.clone();
        let prepared = binary::PreparedCore {
            kind: CoreKind::Mihomo,
            path: home.path().join("not-an-executable"),
            source: "fixture".into(),
            version: "v1.19.27".into(),
        };
        manager
            .apply_prepared_core(
                &prepared,
                manager.current_generation(),
                false,
                true,
                &AtomicBool::new(false),
                None,
            )
            .await
            .expect("stopped selection does not prepare config or require TUN");
        assert_eq!(shared.core_kind(), CoreKind::Mihomo);
        assert_eq!(manager.state(), CoreState::Stopped);
        assert!(manager.pid().is_none());
        assert!(
            manager.binary_path().is_none(),
            "selection must not mutate executable cache"
        );
        assert_eq!(
            std::fs::read_to_string(home.path().join("config.yaml")).unwrap(),
            "previous mihomo runtime"
        );
        assert_eq!(
            std::fs::read_to_string(home.path().join("singbox.json")).unwrap(),
            "previous singbox runtime"
        );
        let verge: serde_yaml_ng::Mapping =
            serde_yaml_ng::from_str(&std::fs::read_to_string(home.path().join("verge.yaml")).unwrap()).unwrap();
        assert_eq!(verge["proxy_core"], serde_yaml_ng::Value::from("mihomo"));
        assert_eq!(verge["future"]["keep"], serde_yaml_ng::Value::from("yes"));
        assert_eq!(shared.current_generation(), 1);
        let stale = manager
            .apply_prepared_core(&prepared, 0, false, false, &AtomicBool::new(false), None)
            .await;
        assert!(stale.unwrap_err().to_string().contains("generation"));
        assert_eq!(manager.current_generation(), 1);
        let prepared = binary::PreparedCore {
            kind: CoreKind::SingBox,
            ..prepared
        };
        *manager.inner.state.lock() = CoreState::Error("earlier startup failed".into());
        manager
            .apply_prepared_core(&prepared, 1, false, true, &AtomicBool::new(false), None)
            .await
            .unwrap();
        assert_eq!(shared.core_kind(), CoreKind::SingBox);
        assert_eq!(shared.current_generation(), 2);
        assert_eq!(
            super::super::ownership::read_ownership_marker_at(home.path())
                .unwrap()
                .core,
            "singbox"
        );
        assert!(manager.pid().is_none());
        assert!(manager.binary_path().is_none());
        assert_eq!(manager.state(), CoreState::Stopped);
    }

    #[test]
    fn stopped_selection_transaction_rolls_back_marker_and_exact_yaml_at_each_failure() {
        for target in [CoreKind::Mihomo, CoreKind::SingBox] {
            for fail_at in [1, 2, 3] {
                let home = tempfile::tempdir().unwrap();
                let yaml = b"# retain exact old bytes\nproxy_core: mihomo\nfuture: {keep: yes}\n";
                let marker = b"previous ownership marker";
                std::fs::write(home.path().join("verge.yaml"), yaml).unwrap();
                std::fs::write(home.path().join(super::super::ownership::OWNERSHIP_MARKER), marker).unwrap();
                let mut checks = 0;
                let result = persist_stopped_selection(home.path(), target, || {
                    checks += 1;
                    if checks == fail_at {
                        anyhow::bail!("fixture commit failure");
                    }
                    Ok(())
                });
                assert!(result.is_err());
                assert_eq!(std::fs::read(home.path().join("verge.yaml")).unwrap(), yaml);
                assert_eq!(
                    std::fs::read(home.path().join(super::super::ownership::OWNERSHIP_MARKER)).unwrap(),
                    marker
                );
                assert_eq!(
                    std::fs::read_dir(home.path()).unwrap().count(),
                    2,
                    "temporary candidates cleaned up"
                );
            }
        }
    }

    #[tokio::test]
    async fn stopped_selection_rejects_cancel_busy_foreign_endpoint_and_record_without_mutation() {
        let home = private_socket_fixture();
        let _guard = crate::profile_store::store::tests::claim_test_app_home(home.path().to_path_buf()).await;
        let yaml = b"proxy_core: mihomo\nfuture: {keep: true}\n";
        std::fs::write(home.path().join("verge.yaml"), yaml).unwrap();
        let socket = home.path().join("controller.sock");
        let manager = MihomoManager::new(home.path().to_path_buf())
            .with_socket(socket.clone())
            .with_singbox_controller("127.0.0.1:0".parse().unwrap());
        let prepared = binary::PreparedCore {
            kind: CoreKind::SingBox,
            path: home.path().join("absent"),
            source: "fixture".into(),
            version: "1.14.2".into(),
        };
        assert!(
            manager
                .apply_prepared_core(&prepared, 0, false, true, &AtomicBool::new(true), None)
                .await
                .is_err()
        );
        manager.inner.restarting.store(true, Ordering::SeqCst);
        assert!(
            manager
                .apply_prepared_core(&prepared, 0, false, true, &AtomicBool::new(false), None)
                .await
                .is_err()
        );
        manager.inner.restarting.store(false, Ordering::SeqCst);
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        assert!(
            manager
                .apply_prepared_core(&prepared, 0, false, true, &AtomicBool::new(false), None)
                .await
                .is_err()
        );
        drop(listener);
        std::fs::remove_file(&socket).unwrap();
        pidfile::write(
            &pidfile::path_for(&socket),
            pidfile::CoreRecord::with_kind(std::process::id(), Utc::now(), CoreKind::Mihomo),
        )
        .unwrap();
        assert!(
            manager
                .apply_prepared_core(&prepared, 0, false, true, &AtomicBool::new(false), None)
                .await
                .is_err()
        );
        assert_eq!(std::fs::read(home.path().join("verge.yaml")).unwrap(), yaml);
        assert_eq!(manager.core_kind(), CoreKind::Mihomo);
        assert_eq!(manager.current_generation(), 0);
        assert!(manager.binary_path().is_none());
        assert!(!home.path().join(super::super::ownership::OWNERSHIP_MARKER).exists());
    }

    #[test]
    fn singbox_controller_uses_configured_loopback_port() {
        let config: serde_yaml_ng::Mapping = serde_yaml_ng::from_str("external-controller: 127.0.0.1:49715\n").unwrap();
        assert_eq!(configured_singbox_controller(&config).unwrap().port(), 49715);
        for controller in ["0.0.0.0:9097", "[::1]:9097", "192.0.2.1:9097"] {
            let config: serde_yaml_ng::Mapping =
                serde_yaml_ng::from_str(&format!("external-controller: '{controller}'\n")).unwrap();
            assert_eq!(
                configured_singbox_controller(&config).unwrap().to_string(),
                "127.0.0.1:9097"
            );
        }
    }

    #[test]
    fn guided_selection_builtin_direct_block_roundtrip_matches_target_members() {
        let home = tempfile::tempdir().unwrap();
        let mihomo = home.path().join("mihomo.yaml");
        let singbox = home.path().join("singbox.json");
        std::fs::write(&mihomo, "proxy-groups:\n  - {name: DirectGroup, type: select, proxies: [DIRECT]}\n  - {name: RejectGroup, type: select, proxies: [REJECT]}\n").unwrap();
        std::fs::write(&singbox, r#"{"outbounds":[{"tag":"direct","type":"direct"},{"tag":"block","type":"block"},{"tag":"DirectGroup","type":"selector","outbounds":["direct"]},{"tag":"RejectGroup","type":"selector","outbounds":["block"]}]}"#).unwrap();
        let selections = vec![
            ("DirectGroup".into(), "DIRECT".into()),
            ("RejectGroup".into(), "REJECT".into()),
        ];
        let to_box = guided_target_selections(CoreKind::SingBox, &singbox, &selections).unwrap();
        assert_eq!(
            to_box,
            vec![
                ("DirectGroup".into(), "direct".into()),
                ("RejectGroup".into(), "block".into())
            ]
        );
        assert_eq!(
            guided_target_selections(CoreKind::Mihomo, &mihomo, &to_box).unwrap(),
            selections
        );
    }

    #[test]
    fn guided_selection_legal_exact_names_take_precedence_over_builtin_aliases() {
        let home = tempfile::tempdir().unwrap();
        let mihomo = home.path().join("mihomo.yaml");
        let singbox = home.path().join("singbox.json");
        std::fs::write(&mihomo, "proxies: [{name: direct, type: ss}, {name: block, type: ss}, {name: Direct, type: ss}]\nproxy-groups: [{name: G, type: select, proxies: [direct, block, Direct, DIRECT, REJECT]}]\n").unwrap();
        std::fs::write(&singbox, r#"{"outbounds":[{"tag":"direct","type":"direct"},{"tag":"block","type":"block"},{"tag":"DIRECT","type":"socks"},{"tag":"REJECT","type":"socks"},{"tag":"Direct","type":"socks"},{"tag":"G","type":"selector","outbounds":["DIRECT","REJECT","Direct","direct","block"]}]}"#).unwrap();
        for name in ["direct", "block", "Direct"] {
            let selection = vec![("G".into(), name.into())];
            assert_eq!(
                guided_target_selections(CoreKind::Mihomo, &mihomo, &selection).unwrap(),
                selection
            );
        }
        for name in ["DIRECT", "REJECT", "Direct"] {
            let selection = vec![("G".into(), name.into())];
            assert_eq!(
                guided_target_selections(CoreKind::SingBox, &singbox, &selection).unwrap(),
                selection
            );
        }
    }

    #[test]
    fn guided_selection_missing_group_member_or_leaf_is_rejected_before_switching() {
        let home = tempfile::tempdir().unwrap();
        for (kind, raw) in [
            (
                CoreKind::Mihomo,
                "proxy-groups: [{name: G, type: select, proxies: [ghost, DIRECT]}]\n",
            ),
            (
                CoreKind::SingBox,
                r#"{"outbounds":[{"tag":"direct","type":"direct"},{"tag":"G","type":"selector","outbounds":["ghost","direct"]}]}"#,
            ),
        ] {
            let config = home.path().join(kind.as_str());
            std::fs::write(&config, raw).unwrap();
            for selection in [("missing", "DIRECT"), ("G", "missing"), ("G", "ghost")] {
                let error = guided_target_selections(kind, &config, &[(selection.0.into(), selection.1.into())])
                    .expect_err("missing selector membership or declared node must fail before stop");
                assert!(error.to_string().contains("old core remains running"));
            }
        }
    }

    fn private_socket_fixture() -> tempfile::TempDir {
        use std::os::unix::fs::PermissionsExt;
        let home = tempfile::tempdir().unwrap();
        std::fs::set_permissions(home.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        home
    }

    /// P1 (reviewer): the recovery receipt only covers the core the policy
    /// already accepted. A foreign core that took the controller socket
    /// while the transaction was failing is still refused.
    #[test]
    fn a_recovery_authorization_only_covers_the_core_it_named() {
        let home = private_socket_fixture();
        let socket = home.path().join("controller.sock");
        let manager = MihomoManager::new(home.path().to_path_buf()).with_socket(socket.clone());
        let authorized = RestartAuthorization {
            kind: CoreKind::Mihomo,
            pid: 4242,
            exe: Some("/opt/verge-mihomo".to_string()),
        };
        let record_path = pidfile::path_for(&socket);

        // No record: the authorized core is gone (the failed first attempt).
        assert!(manager.authorize_still_ours(&authorized).is_ok());

        // Our own replacement from an earlier attempt in this transaction.
        pidfile::write(
            &record_path,
            pidfile::CoreRecord::with_kind_and_exe(
                5151,
                Utc::now(),
                CoreKind::Mihomo,
                Some("/opt/verge-mihomo".to_string()),
            ),
        )
        .unwrap();
        assert!(manager.authorize_still_ours(&authorized).is_ok());

        // A different executable under a different pid is somebody else's.
        pidfile::write(
            &record_path,
            pidfile::CoreRecord::with_kind_and_exe(
                6161,
                Utc::now(),
                CoreKind::Mihomo,
                Some("/usr/bin/other".to_string()),
            ),
        )
        .unwrap();
        assert!(
            manager.authorize_still_ours(&authorized).is_err(),
            "a foreign core must not be replaceable through an old receipt"
        );

        // A different KIND is a core switch, never a recovery.
        pidfile::write(
            &record_path,
            pidfile::CoreRecord::with_kind_and_exe(
                5151,
                Utc::now(),
                CoreKind::SingBox,
                Some("/opt/verge-mihomo".to_string()),
            ),
        )
        .unwrap();
        assert!(manager.authorize_still_ours(&authorized).is_err());

        // And the global policy still refuses a core with no record at all.
        assert!(
            supervisor_restart_policy(false, None, CoreKind::Mihomo, CoreKind::Mihomo).is_err(),
            "the no-pid foreign-core refusal must stay in place"
        );
    }

    #[test]
    fn guided_socket_stale_without_record_allows_both_target_kinds() {
        for kind in [CoreKind::Mihomo, CoreKind::SingBox] {
            let home = private_socket_fixture();
            let socket = home.path().join("controller.sock");
            drop(std::os::unix::net::UnixListener::bind(&socket).unwrap());
            let manager = MihomoManager::new(home.path().to_path_buf())
                .with_socket(socket.clone())
                .with_singbox_controller("127.0.0.1:0".parse().unwrap());
            assert!(socket.exists());
            let result = manager.guided_record_check(kind);
            assert!(result.is_ok(), "{kind:?}: {result:?}");
            assert!(socket.exists(), "read-only preflight must not unlink");
        }
    }

    #[test]
    fn guided_socket_live_even_with_dead_record_refuses_both_target_kinds() {
        use std::os::unix::fs::MetadataExt;
        for kind in [CoreKind::Mihomo, CoreKind::SingBox] {
            let home = private_socket_fixture();
            let socket = home.path().join("controller.sock");
            let _listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
            let inode = std::fs::symlink_metadata(&socket).unwrap().ino();
            let record = pidfile::CoreRecord::with_kind(u32::MAX, Utc::now(), CoreKind::Mihomo);
            pidfile::write(&pidfile::path_for(&socket), record.clone()).unwrap();
            let manager = MihomoManager::new(home.path().to_path_buf())
                .with_socket(socket.clone())
                .with_singbox_controller("127.0.0.1:0".parse().unwrap());
            let error = manager.guided_record_check(kind).unwrap_err().to_string();
            assert!(error.contains(socket.to_str().unwrap()), "{kind:?}: {error}");
            assert!(error.contains("ownership record"), "{error}");
            assert!(socket.exists());
            assert_eq!(std::fs::symlink_metadata(&socket).unwrap().ino(), inode);
            assert_eq!(pidfile::read_record(&pidfile::path_for(&socket)), Some(record));
        }
    }

    #[test]
    fn guided_socket_unknown_kernel_status_fails_closed_for_both_target_kinds() {
        for kind in [CoreKind::Mihomo, CoreKind::SingBox] {
            let home = private_socket_fixture();
            let socket = home.path().join("controller.sock");
            drop(std::os::unix::net::UnixListener::bind(&socket).unwrap());
            let manager = MihomoManager::new(home.path().to_path_buf())
                .with_socket(socket.clone())
                .with_singbox_controller("127.0.0.1:0".parse().unwrap());
            for malformed in [false, true] {
                let result = manager.guided_record_check_with(kind, || {
                    if malformed {
                        Ok(String::from("unrecognized kernel table"))
                    } else {
                        Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
                    }
                });
                assert!(result.is_err(), "{kind:?}: {result:?}");
                assert!(socket.exists());
                assert!(pidfile::read_record(&pidfile::path_for(&socket)).is_none());
            }
        }
    }

    #[test]
    fn guided_socket_unlinked_live_endpoint_refuses_both_target_kinds() {
        let home = private_socket_fixture();
        let socket = home.path().join("controller.sock");
        let _listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        std::fs::remove_file(&socket).unwrap(); // unlink only our listener's fixture pathname
        let manager = MihomoManager::new(home.path().to_path_buf())
            .with_socket(socket.clone())
            .with_singbox_controller("127.0.0.1:0".parse().unwrap());
        for kind in [CoreKind::Mihomo, CoreKind::SingBox] {
            assert!(manager.guided_record_check(kind).is_err(), "{kind:?}");
            assert!(
                manager
                    .guided_record_check_with(kind, || Err(std::io::ErrorKind::PermissionDenied.into()))
                    .is_err()
            );
        }
        assert!(!socket.exists());
    }

    #[tokio::test]
    async fn guided_socket_apply_boundary_rechecks_new_live_endpoint() {
        use std::os::unix::fs::MetadataExt;
        let home = private_socket_fixture();
        let socket = home.path().join("controller.sock");
        drop(std::os::unix::net::UnixListener::bind(&socket).unwrap());
        let manager = MihomoManager::new(home.path().to_path_buf())
            .with_socket(socket.clone())
            .with_singbox_controller("127.0.0.1:0".parse().unwrap());
        manager.guided_record_check(CoreKind::Mihomo).unwrap();
        std::fs::remove_file(&socket).unwrap(); // replace only our own fixture
        let _listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let inode = std::fs::symlink_metadata(&socket).unwrap().ino();
        for kind in [CoreKind::Mihomo, CoreKind::SingBox] {
            assert!(manager.guided_record_check(kind).is_err());
        }
        assert!(resource_barrier(&socket, Duration::ZERO).await.is_err());
        assert_eq!(std::fs::symlink_metadata(&socket).unwrap().ino(), inode);
    }

    #[test]
    fn guided_core_unadopted_live_record_and_external_socket_refuse_without_mutation() {
        assert!(check_guided_record(Some(10), true, None, false, true).is_err());
        assert!(check_guided_record(None, false, None, false, true).is_err());
        assert!(check_guided_record(Some(11), true, Some(10), true, true).is_err());
        assert!(check_guided_record(Some(10), true, Some(10), true, true).is_ok());
        assert!(check_guided_record(Some(10), false, None, false, true).is_err());
    }

    #[test]
    fn guided_controller_owned_mihomo_listener_allows_singbox_target_but_foreign_refuses() {
        let home = private_socket_fixture();
        let socket = home.path().join("controller.sock");
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let manager = MihomoManager::new(home.path().to_path_buf())
            .with_socket(socket.clone())
            .with_singbox_controller(address);
        let record = pidfile::CoreRecord::with_kind(std::process::id(), Utc::now(), CoreKind::Mihomo);
        pidfile::write(&pidfile::path_for(&socket), record.clone()).unwrap();
        *manager.inner.pid.lock() = Some(std::process::id());
        *manager.inner.state.lock() = CoreState::Running;
        manager.inner.owns_child.store(true, Ordering::SeqCst);
        manager
            .guided_record_check(CoreKind::SingBox)
            .expect("actual socket inode proves this owned Mihomo predecessor holds the target port");
        assert_eq!(manager.core_kind(), CoreKind::Mihomo);
        assert_eq!(manager.pid(), Some(std::process::id()));
        assert_eq!(manager.current_generation(), 0);
        let foreign = MihomoManager::new(home.path().to_path_buf())
            .with_socket(home.path().join("other.sock"))
            .with_singbox_controller(address);
        assert!(
            foreign.guided_record_check(CoreKind::SingBox).is_err(),
            "an unowned listener must still refuse"
        );
        assert_eq!(listener.local_addr().unwrap(), address);
    }

    #[test]
    fn guided_controller_owned_singbox_keeps_restart_without_new_fd_inspection() {
        let home = private_socket_fixture();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let manager = MihomoManager::new(home.path().to_path_buf())
            .with_singbox_controller(listener.local_addr().unwrap())
            .with_core_kind(CoreKind::SingBox);
        *manager.inner.pid.lock() = Some(std::process::id());
        manager.inner.owns_child.store(true, Ordering::SeqCst);
        manager
            .guided_tcp_controller_check_with(
                CoreKind::SingBox,
                |_, _| anyhow::bail!("new FD inspection must not run on the existing owned SingBox restart path"),
                || panic!("existing owned SingBox restart must not invoke the new permission fallback"),
            )
            .unwrap();
        manager.inner.owns_child.store(false, Ordering::SeqCst);
        assert!(
            manager
                .guided_tcp_controller_check_with(
                    CoreKind::SingBox,
                    |_, _| panic!("attached cores must not get an ownership proof hook"),
                    || false
                )
                .is_err()
        );
    }

    #[test]
    fn guided_controller_capability_permission_denial_defers_only_owned_mihomo_without_gui() {
        let home = private_socket_fixture();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let manager = MihomoManager::new(home.path().to_path_buf()).with_singbox_controller(address);
        *manager.inner.pid.lock() = Some(std::process::id());
        manager.inner.owns_child.store(true, Ordering::SeqCst);
        let permission = |_, _| Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied).into());
        manager
            .guided_tcp_controller_check_with(CoreKind::SingBox, permission, || false)
            .unwrap();
        assert!(
            manager
                .guided_tcp_controller_check_with(CoreKind::SingBox, permission, || true)
                .is_err()
        );
        assert!(
            manager
                .guided_tcp_controller_check_with(CoreKind::SingBox, |_, _| Ok(false), || false)
                .is_err()
        );
        assert!(
            manager
                .guided_tcp_controller_check_with(
                    CoreKind::SingBox,
                    |_, _| Err(std::io::Error::from(std::io::ErrorKind::NotFound).into()),
                    || false
                )
                .is_err()
        );
        manager.inner.owns_child.store(false, Ordering::SeqCst);
        assert!(
            manager
                .guided_tcp_controller_check_with(CoreKind::SingBox, permission, || false)
                .is_err()
        );
        assert!(
            ensure_guided_controller_released(CoreKind::SingBox, address).is_err(),
            "deferred permission never bypasses the mandatory post-stop port guard"
        );
        drop(listener);
        ensure_guided_controller_released(CoreKind::SingBox, address).unwrap();
    }

    #[tokio::test]
    async fn guided_controller_deferred_port_failure_rolls_back_before_any_target_spawn() {
        let home = tempfile::tempdir().unwrap();
        let settings = home.path().join("verge.yaml");
        std::fs::write(&settings, "proxy_core: mihomo\nfuture: {keep: true}\n").unwrap();
        let snapshot = GuidedFileSnapshot::capture([settings.clone()]).unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let events = parking_lot::Mutex::new(Vec::new());
        let result = orchestrate_guided_switch(
            || async {
                events.lock().push("prevalidated");
                Ok(())
            },
            true,
            || async {
                events.lock().push("stop-owned");
                Ok(())
            },
            || async {
                events.lock().push("port-check");
                ensure_guided_controller_released(CoreKind::SingBox, address)?;
                events.lock().push("spawn-target");
                Ok(())
            },
            || async {
                events.lock().push("commit");
                Ok(())
            },
            || async {
                events.lock().push("rollback-owned");
                snapshot.restore()
            },
        )
        .await;
        assert!(result.is_err());
        assert_eq!(
            *events.lock(),
            ["prevalidated", "stop-owned", "port-check", "rollback-owned"]
        );
        assert_eq!(
            std::fs::read_to_string(&settings).unwrap(),
            "proxy_core: mihomo\nfuture: {keep: true}\n"
        );
        assert_eq!(
            listener.local_addr().unwrap(),
            address,
            "foreign listener is never contacted, stopped or replaced"
        );
    }

    #[test]
    fn guided_core_explicit_target_transport_does_not_publish_shared_selection() {
        let manager = MihomoManager::new(std::env::temp_dir());
        let target = api_for_core(
            CoreKind::SingBox,
            manager.socket_path(),
            "127.0.0.1:12345".parse().unwrap(),
            "fixture-secret".into(),
        )
        .unwrap();
        assert_eq!(target.core_kind(), CoreKind::SingBox);
        assert!(matches!(target.transport(), crate::mihomo_api::Transport::Tcp(_)));
        assert_eq!(manager.clone().core_kind(), CoreKind::Mihomo);
        assert!(matches!(
            manager.api().transport(),
            crate::mihomo_api::Transport::UnixSocket(_)
        ));
    }

    #[tokio::test]
    async fn guided_core_cancellation_during_commit_restores_prior_contents() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("verge.yaml");
        std::fs::write(&path, "proxy_core: mihomo\nunknown: preserved").unwrap();
        let files = GuidedFileSnapshot::capture([path.clone()]).unwrap();
        let cancelled = AtomicBool::new(false);
        let result = orchestrate_guided_switch(
            || async { Ok(()) },
            false,
            || async { Ok(()) },
            || async { Ok(()) },
            || async {
                std::fs::write(&path, "proxy_core: singbox").unwrap();
                cancelled.store(true, Ordering::SeqCst);
                tokio::task::yield_now().await;
                check_guided_cancel(&cancelled)
            },
            || async { files.restore() },
        )
        .await;
        assert!(result.is_err());
        assert_eq!(
            std::fs::read_to_string(path).unwrap(),
            "proxy_core: mihomo\nunknown: preserved"
        );
    }
    // ---- F2 / F3: supervisor-launch ownership and cleanup ----------------

    /// A fake sing-box clash_api that always answers, so a launch reaches the
    /// READY point and the failure under test is the identity check alone.
    fn answering_controller() -> std::net::SocketAddr {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let addr = listener.local_addr().unwrap();
        let listener = tokio::net::TcpListener::from_std(listener).unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(async move {
                    let mut buffer = [0u8; 4096];
                    if stream.read(&mut buffer).await.is_err() {
                        return;
                    }
                    let body = r#"{"version":"1.19.0"}"#;
                    let _ = stream
                        .write_all(
                            format!(
                                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                                body.len()
                            )
                            .as_bytes(),
                        )
                        .await;
                });
            }
        });
        addr
    }

    /// The pid file the forking stand-in supervisor writes its child's pid
    /// into (the seam is a plain fn pointer, so it cannot capture).
    static STANDIN_CORE_PID_FILE: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();

    /// A stand-in supervisor that forks a REAL child process standing in for
    /// the core and records its pid in [`STANDIN_CORE_PID_FILE`]. The child
    /// is a genuine descendant, so the cleanup path is exercised across the
    /// process boundary instead of through in-process bookkeeping.
    fn forking_supervisor(
        _config_dir: &Path,
        log: &Path,
        _mode: crate::commands::start::SupervisorLaunch,
    ) -> anyhow::Result<std::process::Child> {
        if let Some(dir) = log.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(log, b"stand-in supervisor\n")?;
        let core_pid_file = STANDIN_CORE_PID_FILE
            .get()
            .ok_or_else(|| anyhow::anyhow!("the stand-in supervisor was launched without a pid file"))?
            .display()
            .to_string();
        Ok(std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(format!("sleep 300 & echo $! > {core_pid_file}; wait"))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()?)
    }

    fn wait_for_pid_file(path: &std::path::Path) -> u32 {
        for _ in 0..200 {
            if let Ok(body) = std::fs::read_to_string(path)
                && let Ok(pid) = body.trim().parse::<u32>()
            {
                return pid;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        panic!("the stand-in core never reported its pid");
    }

    /// A receipt for a predecessor that is no longer running: the launch it
    /// authorizes must bring up a core that is NOT that pid.
    fn stopped_predecessor_receipt() -> RestartAuthorization {
        RestartAuthorization {
            kind: CoreKind::SingBox,
            pid: 0x00C0_FFEE,
            exe: Some("/opt/verge-mihomo".to_string()),
        }
    }

    /// P1 (reviewer), F2: the core's API answers, but the pidfile write the
    /// supervisor should have done failed — the identity check then fails.
    /// The launched supervisor AND the core it forked must both be stopped:
    /// leaving the new core alive would hold the controller socket and the
    /// mixed port while the caller rolls the config back to A.
    #[tokio::test]
    async fn an_identity_failure_after_a_ready_api_stops_the_launched_core() {
        use crate::profile_store::store::tests::{claim_test_app_home, test_app_home_root};
        let root = test_app_home_root();
        let _home = claim_test_app_home(root.clone()).await;
        let home = tempfile::tempdir().unwrap();
        let controller = answering_controller();
        let config_dir = home.path().join("run");
        std::fs::create_dir_all(&config_dir).unwrap();
        let socket = home.path().join("controller.sock");
        let manager = MihomoManager::new(config_dir)
            .with_socket(socket.clone())
            .with_singbox_controller(controller)
            .with_core_kind(CoreKind::SingBox)
            .with_secret("ownership-fixture".to_string());
        let core_pid_file = home.path().join("core.pid");
        STANDIN_CORE_PID_FILE.set(core_pid_file.clone()).ok();

        let _launcher = crate::commands::start::install_supervisor_launcher(forking_supervisor).expect("install seam");
        let error = manager
            .apply_restart_authorization_with(
                &stopped_predecessor_receipt(),
                crate::commands::start::SupervisorLaunch::Regenerate,
            )
            .await
            .expect_err("no pid record means the replacement cannot be attributed");
        assert!(error.to_string().contains("no live singbox pid record"), "{error}");

        let core = wait_for_pid_file(&core_pid_file);
        assert!(
            !pidfile::is_running(core),
            "the core this launch forked must be stopped, not left holding the ports (pid {core})"
        );
        // Nothing of anyone else's was removed: the record was never written.
        assert!(!pidfile::path_for(&socket).exists());
    }

    /// P1 (reviewer), F3: the shared pid record names ANOTHER live instance.
    /// A launch that fails must stop only what it forked — this record, and
    /// the process it names, belong to somebody else.
    #[tokio::test]
    async fn a_failed_launch_leaves_another_instances_core_and_record_alone() {
        use crate::profile_store::store::tests::{claim_test_app_home, test_app_home_root};
        let root = test_app_home_root();
        let _home = claim_test_app_home(root.clone()).await;
        let home = tempfile::tempdir().unwrap();
        let controller = answering_controller();
        let config_dir = home.path().join("run");
        std::fs::create_dir_all(&config_dir).unwrap();
        let socket = home.path().join("controller.sock");
        let manager = MihomoManager::new(config_dir)
            .with_socket(socket.clone())
            .with_singbox_controller(controller)
            .with_core_kind(CoreKind::SingBox)
            .with_secret("ownership-fixture".to_string());

        // Somebody else's live core, recorded in the SHARED record.
        #[allow(clippy::zombie_processes)]
        let foreign = std::process::Command::new("/bin/sleep")
            .arg("300")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let foreign_pid = foreign.id();
        let record_path = pidfile::path_for(&socket);
        pidfile::write(
            &record_path,
            pidfile::CoreRecord::with_kind_and_exe(
                foreign_pid,
                Utc::now(),
                CoreKind::SingBox,
                Some("/bin/sleep".into()),
            ),
        )
        .unwrap();

        // The receipt names that instance, so the transaction is allowed to
        // proceed and replace it — it is the CONCURRENT launch that then
        // fails, and its cleanup must not reach back into this record.
        let receipt = RestartAuthorization {
            kind: CoreKind::SingBox,
            pid: foreign_pid,
            exe: Some("/bin/sleep".into()),
        };

        // A supervisor that stays up but never records a core of its own: the
        // controller keeps answering (it belongs to the instance above), so
        // readiness succeeds and the identity check is what fails.
        fn idle_supervisor(
            _config_dir: &Path,
            log: &Path,
            _mode: crate::commands::start::SupervisorLaunch,
        ) -> anyhow::Result<std::process::Child> {
            if let Some(dir) = log.parent() {
                std::fs::create_dir_all(dir)?;
            }
            std::fs::write(log, b"stand-in supervisor\n")?;
            Ok(std::process::Command::new("/bin/sleep")
                .arg("300")
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()?)
        }
        let _launcher = crate::commands::start::install_supervisor_launcher(idle_supervisor).expect("install seam");
        let error = manager
            .apply_restart_authorization_with(&receipt, crate::commands::start::SupervisorLaunch::Regenerate)
            .await
            .expect_err("the answering core is not the replacement this launch forked");
        assert!(error.to_string().contains("never started"), "{error}");

        assert!(
            pidfile::is_running(foreign_pid),
            "the shared record names another instance's core; cleanup must not signal it"
        );
        assert_eq!(
            pidfile::read_record(&record_path).map(|record| record.pid),
            Some(foreign_pid),
            "cleanup must not delete a record it does not own"
        );
        let _ = nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(foreign_pid as i32),
            nix::sys::signal::Signal::SIGKILL,
        );
    }

    /// P2 (reviewer), F4: the recovery restriction ("the runtime config must
    /// already exist") leaked into the ORDINARY start/restart path: the
    /// missing-file branch called the recovery check first, so its
    /// regeneration line was unreachable and `restart` with a missing
    /// `singbox.json` just failed. A normal start regenerates (and may
    /// overwrite); a recovery requires the restored file to be there.
    #[tokio::test]
    async fn a_normal_start_regenerates_a_missing_runtime_config_while_a_recovery_requires_one() {
        use crate::profile_store::store::tests::claim_test_app_home;
        let home = tempfile::tempdir().unwrap();
        let _home = claim_test_app_home(home.path().to_path_buf()).await;
        std::fs::write(
            home.path().join("verge.yaml"),
            "proxy_core: singbox\nverge_mixed_port: 39192\n",
        )
        .unwrap();
        std::fs::write(
            home.path().join("config.yaml"),
            "mixed-port: 39192\nport: 0\nsocks-port: 0\nsecret: f4-fixture-secret\nexternal-controller: 127.0.0.1:19992\ntun: {enable: false}\n",
        )
        .unwrap();
        std::fs::write(
            home.path().join("profiles.yaml"),
            "current: Rstart\nitems:\n  - {uid: Rstart, type: remote, name: f4, file: base.yaml}\n",
        )
        .unwrap();
        std::fs::create_dir_all(home.path().join("profiles")).unwrap();
        std::fs::write(
            home.path().join("profiles/base.yaml"),
            "proxies: []\nrules:\n  - MATCH,DIRECT\n",
        )
        .unwrap();

        let formal = clash_verge_core::utils::dirs::singbox_config_path().unwrap();
        let _ = tokio::fs::remove_file(&formal).await;
        assert!(!formal.exists(), "precondition: the runtime config is missing");

        let config_dir = home.path().join("run");
        std::fs::create_dir_all(&config_dir).unwrap();
        let socket = home.path().join("controller.sock");
        let normal = MihomoManager::new(config_dir.clone())
            .with_socket(socket.clone())
            .with_singbox_controller("127.0.0.1:19992".parse().unwrap())
            .with_core_kind(CoreKind::SingBox);
        let path = normal
            .inner
            .runtime_config_for_start(&config_dir, crate::commands::start::SupervisorLaunch::Regenerate)
            .await
            .expect("a normal start regenerates the missing runtime config");
        assert!(path.exists(), "the regenerated config must exist");
        assert_eq!(path, formal);
        let body = std::fs::read_to_string(&formal).unwrap();
        assert!(
            body.contains("clash_api"),
            "the regenerated config is a real sing-box config"
        );

        // A recovery start, by contrast, must NOT regenerate: it exists to
        // serve the file the failed apply restored — or the candidate an apply
        // just verified and installed (A5: the same mode, same contract).
        let _ = tokio::fs::remove_file(&formal).await;
        let recovery = normal.clone();
        let error = recovery
            .inner
            .runtime_config_for_start(&config_dir, crate::commands::start::SupervisorLaunch::UseExistingConfig)
            .await
            .expect_err("a recovery has nothing to serve when the file is gone");
        assert!(
            error
                .to_string()
                .contains("previous configuration could not be recovered"),
            "{error}"
        );
        assert!(
            !formal.exists(),
            "a failed recovery must not leave a generated config behind"
        );

        // With the file restored, the recovery serves it verbatim.
        std::fs::write(&formal, "{\"marker\":\"restored\"}").unwrap();
        let served = recovery
            .inner
            .runtime_config_for_start(&config_dir, crate::commands::start::SupervisorLaunch::UseExistingConfig)
            .await
            .unwrap();
        assert_eq!(served, formal);
        assert_eq!(
            std::fs::read_to_string(&formal).unwrap(),
            "{\"marker\":\"restored\"}",
            "a recovery must not regenerate over the restored config"
        );

        // The two modes are independent: a recovery leaves no ambient state
        // behind, so the next ordinary start still regenerates a missing
        // runtime config (A5: the mode travels with the launch).
        let _ = tokio::fs::remove_file(&formal).await;
        let regenerated = recovery
            .inner
            .runtime_config_for_start(&config_dir, crate::commands::start::SupervisorLaunch::Regenerate)
            .await
            .expect("a normal start after a recovery still regenerates");
        assert!(
            regenerated.exists() && regenerated == formal,
            "a recovery must not leave the next normal start unable to generate"
        );
    }

    /// P2 (reviewer), F4, REAL chain: the ordinary start path
    /// (`commands::daemon::run` → `manager.start` → `start_singbox`) comes up
    /// on a profile whose runtime JSON does not exist yet. Before the fix it
    /// demanded the file (the recovery restriction had leaked into the
    /// ordinary start), so a normal start could never create it.
    ///
    /// Tagged `#[ignore]` like the repository's other real-core e2e tests
    /// (it spawns a real binary and binds ports); runs with:
    /// `cargo test -p clash-verge-cli -- --ignored`.
    #[tokio::test]
    #[ignore = "spawns a real sing-box core; run: cargo test -p clash-verge-cli -- --ignored"]
    async fn a_normal_foreground_start_regenerates_a_missing_runtime_config() {
        if crate::mihomo_manager::singbox_binary::candidate_without_install().is_none() {
            eprintln!("skipping: no sing-box binary found");
            return;
        }
        use crate::profile_store::store::tests::claim_test_app_home;
        let home = tempfile::tempdir().unwrap();
        let _home = claim_test_app_home(home.path().to_path_buf()).await;

        let controller = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
            listener.local_addr().expect("addr")
        };
        let mixed_port = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
            listener.local_addr().expect("addr").port()
        };
        let socket = home.path().join("controller.sock");
        std::fs::write(
            home.path().join("config.yaml"),
            format!(
                "mixed-port: {mixed_port}\nexternal-controller: {controller}\nexternal-controller-unix: {}\n\
secret: f4-fixture-secret\ntun: {{enable: false}}\n",
                socket.display()
            ),
        )
        .unwrap();
        std::fs::write(
            home.path().join("verge.yaml"),
            format!("proxy_core: singbox\nverge_mixed_port: {mixed_port}\n"),
        )
        .unwrap();
        std::fs::write(
            home.path().join("profiles.yaml"),
            "current: Rfg4\nitems:\n  - {uid: Rfg4, type: remote, name: f4, file: base.yaml}\n",
        )
        .unwrap();
        std::fs::create_dir_all(home.path().join("profiles")).unwrap();
        std::fs::write(
            home.path().join("profiles/base.yaml"),
            "proxies: []\nrules:\n  - DOMAIN,f4-marker.example,DIRECT\n  - MATCH,DIRECT\n",
        )
        .unwrap();
        let formal = clash_verge_core::utils::dirs::singbox_config_path().unwrap();
        let _ = std::fs::remove_file(&formal);
        assert!(!formal.exists(), "precondition: the runtime config is missing");

        let config_dir = home.path().join("run");
        std::fs::create_dir_all(&config_dir).unwrap();
        // The REAL start path (`commands::daemon::run`'s `manager.start()`),
        // against a real sing-box binary. The default launch mode is a NORMAL
        // start (no recovery env var).
        let manager = MihomoManager::new(config_dir.clone())
            .with_socket(socket.clone())
            .with_singbox_controller(controller)
            .with_core_kind(CoreKind::SingBox);
        let started = manager.start().await;
        let record = pidfile::path_for(&socket);
        if let Some(core) = pidfile::read_record(&record) {
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(core.pid as i32),
                nix::sys::signal::Signal::SIGKILL,
            );
            let _ = std::fs::remove_file(&record);
        }
        let _ = manager.stop().await;
        started.expect("a normal start with a missing runtime config must succeed");
        assert!(
            formal.exists(),
            "a normal start must regenerate the missing runtime config"
        );
        assert!(
            std::fs::read_to_string(&formal).unwrap().contains("f4-marker.example"),
            "the active profile must be served"
        );

        // The RESTART path: the runtime JSON is gone again. A normal restart
        // must regenerate it instead of demanding the file, and must do so
        // BEFORE the old core is stopped (asserted by the restart
        // succeeding: the old code stopped the core and then failed).
        let _ = tokio::fs::remove_file(&formal).await;
        assert!(!formal.exists(), "precondition: the runtime config is gone again");
        let restarted = manager.restart().await;
        if let Some(core) = pidfile::read_record(&record) {
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(core.pid as i32),
                nix::sys::signal::Signal::SIGKILL,
            );
            let _ = std::fs::remove_file(&record);
        }
        let _ = manager.stop().await;
        restarted.expect("a normal restart must regenerate a missing runtime config");
        assert!(
            formal.exists() && std::fs::read_to_string(&formal).unwrap().contains("f4-marker.example"),
            "the restarted core must serve a regenerated runtime config"
        );
        let body = std::fs::read_to_string(&formal).expect("generated config");
        assert!(body.contains("f4-marker.example"), "the active profile must be served");
    }

    #[tokio::test]
    async fn guided_core_clash_subscription_prepares_nodes_groups_match_dns_before_stop() {
        use crate::profile_store::store::tests::{claim_test_app_home, test_app_home_root};
        let root = test_app_home_root();
        let _home = claim_test_app_home(root.clone()).await;
        let dir = tempfile::tempdir().unwrap();
        let staged = dir.path().join("prepared.json");
        let yaml = "proxies:\n  - {name: edge, type: ss, server: edge.example, port: 443, cipher: aes-128-gcm, password: fixture}\nproxy-groups:\n  - {name: PROXY, type: select, proxies: [edge, DIRECT]}\nrules: [MATCH,PROXY]\ndns:\n  enable: true\n  nameserver: [8.8.8.8]\n";
        // MATCH contains a comma and is a single rule string.
        let yaml = yaml.replace("rules: [MATCH,PROXY]", "rules: ['MATCH,PROXY']");
        let (path, parts) = ManagerInner::write_singbox_assembled_to(&root, Some(&yaml), false, &staged)
            .await
            .unwrap();
        validate_guided_conversion(&parts.conversion).unwrap();
        let config: serde_json::Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert!(
            config["outbounds"]
                .as_array()
                .unwrap()
                .iter()
                .any(|node| node["tag"] == "edge")
        );
        assert!(
            config["outbounds"]
                .as_array()
                .unwrap()
                .iter()
                .any(|node| node["tag"] == "PROXY" && node["type"] == "selector")
        );
        assert!(
            config["route"]["rules"]
                .as_array()
                .unwrap()
                .iter()
                .any(|rule| rule["outbound"] == "PROXY")
        );
        assert!(
            config["dns"]["servers"]
                .as_array()
                .is_some_and(|servers| !servers.is_empty())
        );
        assert_eq!(
            guided_target_selections(CoreKind::SingBox, &staged, &[("PROXY".into(), "DIRECT".into())]).unwrap(),
            [("PROXY".into(), "direct".into())]
        );
    }

    #[tokio::test]
    async fn script_composition_reaches_mihomo_yaml_and_singbox_conversion_for_local_and_remote() {
        let home = tempfile::tempdir().unwrap();
        let _guard = crate::profile_store::store::tests::claim_test_app_home(home.path().to_path_buf()).await;
        std::fs::write(home.path().join("verge.yaml"), "enable_tun_mode: false\nverge_mixed_port: 35123\nverge_socks_enabled: false\nverge_http_enabled: false\nverge_redir_enabled: false\nverge_tproxy_enabled: false\n").unwrap();
        std::fs::write(home.path().join("config.yaml"), "mixed-port: 35123\nexternal-controller: 127.0.0.1:49715\nsecret: fixture\nmode: rule\ntun: {enable: false}\n").unwrap();
        let source = "proxies:\n  - {name: edge, type: ss, server: edge.example, port: 443, cipher: aes-128-gcm, password: fixture}\nproxy-groups:\n  - {name: PROXY, type: select, proxies: [edge, DIRECT]}\nrules: ['MATCH,PROXY']\nfuture: {preserved: true}\n";
        std::fs::write(home.path().join("profiles/base.yaml"), source).unwrap();
        std::fs::write(home.path().join("profiles/hook.js"), "function main(c, n) { c.proxies[0].server = 'script.example'; c.rules.unshift('DOMAIN,script.example,DIRECT'); c.future.name = n; c['mixed-port'] = 1; return c; }").unwrap();
        for kind in ["local", "remote"] {
            std::fs::write(home.path().join("profiles.yaml"), format!("current: base\nitems:\n  - {{uid: base, type: {kind}, name: fixture, file: base.yaml, option: {{script: sHook}}}}\n  - {{uid: sHook, type: script, file: hook.js}}\n")).unwrap();
            let yaml = ManagerInner::active_profile_yaml().await.unwrap().unwrap();
            let mihomo: serde_yaml_ng::Mapping = serde_yaml_ng::from_str(&yaml).unwrap();
            assert_eq!(
                mihomo["proxies"][0]["server"],
                serde_yaml_ng::Value::from("script.example")
            );
            assert_eq!(
                mihomo["rules"][0],
                serde_yaml_ng::Value::from("DOMAIN,script.example,DIRECT")
            );
            assert_eq!(mihomo["future"]["preserved"], serde_yaml_ng::Value::from(true));
            assert_eq!(mihomo["future"]["name"], serde_yaml_ng::Value::from("fixture"));
            assert_eq!(mihomo["mixed-port"], serde_yaml_ng::Value::from(35123));
            let destination = home.path().join("fixture-candidate.json");
            let (_, parts) = ManagerInner::write_singbox_assembled_to(home.path(), Some(&yaml), false, &destination)
                .await
                .unwrap();
            validate_guided_conversion(&parts.conversion).unwrap();
            let singbox: serde_json::Value = serde_json::from_slice(&std::fs::read(&destination).unwrap()).unwrap();
            assert!(
                singbox["outbounds"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|outbound| outbound["tag"] == "edge" && outbound["server"] == "script.example")
            );
            assert!(singbox["route"]["rules"].as_array().unwrap().iter().any(|rule| {
                rule["domain"]
                    .as_array()
                    .is_some_and(|domains| domains.iter().any(|domain| domain == "script.example"))
            }));
            assert_eq!(
                singbox["experimental"]["clash_api"]["external_controller"],
                "127.0.0.1:49715"
            );
            assert_eq!(
                std::fs::read_to_string(home.path().join("profiles/base.yaml")).unwrap(),
                source
            );
        }
    }

    #[tokio::test]
    async fn guided_core_unsupported_critical_dns_degrades_without_touching_the_formal_file() {
        use crate::profile_store::store::tests::{claim_test_app_home, test_app_home_root};
        let root = test_app_home_root();
        let _home = claim_test_app_home(root.clone()).await;
        let dir = tempfile::tempdir().unwrap();
        let formal = dir.path().join("singbox.json");
        std::fs::write(&formal, "prior config").unwrap();
        let candidate = dir.path().join("prepared.json");
        let yaml = "dns:\n  enable: true\n  nameserver: [8.8.8.8]\n  fallback-filter: {geoip: true}\n";
        // `fallback-filter` has no typed equivalent, but a real subscription
        // ships it: the core must still start and the loss must be reported.
        let (path, parts) = ManagerInner::write_singbox_assembled_to(&root, Some(yaml), false, &candidate)
            .await
            .expect("unrepresentable DNS policy degrades with a report, never a refusal");
        assert_eq!(path, candidate);
        assert!(
            parts
                .conversion
                .notes
                .iter()
                .any(|note| note.starts_with("dns.fallback-filter:")),
            "{:?}",
            parts.conversion.notes
        );
        let config: serde_json::Value = serde_json::from_slice(&std::fs::read(&candidate).unwrap()).unwrap();
        assert!(
            config["dns"]["servers"]
                .as_array()
                .is_some_and(|servers| servers.iter().any(|server| server["server"] == "8.8.8.8")),
            "the supported nameserver still reaches the core: {config}"
        );
        assert_eq!(
            std::fs::read_to_string(formal).unwrap(),
            "prior config",
            "preparing a candidate never mutates the formal config"
        );
    }

    #[tokio::test]
    async fn guided_core_profile_dns_degradations_reach_the_status_notes() {
        use crate::profile_store::store::tests::{claim_test_app_home, test_app_home_root};
        let root = test_app_home_root();
        let _home = claim_test_app_home(root.clone()).await;
        let yaml = "dns:\n  enable: true\n  listen: 127.0.0.1:5335\n  enhanced-mode: fake-ip\n  fake-ip-filter: ['*.lan']\n  nameserver: [8.8.8.8]\n";
        let parts = SingboxParts::assemble(Some(yaml)).await.expect("assemble");
        for expected in ["dns.listen:", "dns.fake-ip-filter:"] {
            assert!(
                parts.conversion.notes.iter().any(|note| note.starts_with(expected)),
                "status surface must report {expected}: {:?}",
                parts.conversion.notes
            );
        }
        let dns = parts.dns_section.as_ref().expect("dns section");
        assert!(
            dns["servers"]
                .as_array()
                .is_some_and(|servers| servers.iter().any(|server| server["type"] == "fakeip")),
            "{dns}"
        );

        // A stored DNS override replaces the profile DNS: its degradations are
        // not part of this run's report. The shared test app home is restored
        // afterwards so the override cannot leak into another test.
        std::fs::create_dir_all(&root).unwrap();
        let override_file = root.join(crate::singbox::DNS_CONFIG_FILE);
        let preexisting = override_file.exists();
        crate::singbox::save_dns_spec(&root, &crate::singbox::DnsConfigSpec::default()).unwrap();
        let overridden = SingboxParts::assemble(Some(yaml)).await.expect("assemble");
        if !preexisting {
            std::fs::remove_file(&override_file).unwrap();
        }
        assert!(
            !overridden.conversion.notes.iter().any(|note| note.starts_with("dns.")),
            "{:?}",
            overridden.conversion.notes
        );
    }

    #[test]
    fn guided_core_readiness_secret_is_taken_from_prepared_control_plane() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("candidate");
        std::fs::write(&path, "secret: fixture-controller-secret").unwrap();
        assert_eq!(
            controller_secret_from_config(Some(&path)).unwrap(),
            "fixture-controller-secret"
        );
        std::fs::write(
            &path,
            r#"{"experimental":{"clash_api":{"secret":"fixture-json-secret"}}}"#,
        )
        .unwrap();
        assert_eq!(
            controller_secret_from_config(Some(&path)).unwrap(),
            "fixture-json-secret"
        );
    }

    #[test]
    fn guided_core_unsupported_tls_conversion_has_concrete_visible_diagnostic() {
        let yaml = "proxies:\n  - {name: secure-edge, type: vless, server: edge.example, port: 443, uuid: 00000000-0000-0000-0000-000000000000, tls: true, skip-cert-verify: invalid-fixture}\n";
        let conversion = crate::singbox::convert::convert_profile(yaml).unwrap();
        let message = validate_guided_conversion(&conversion).unwrap_err().to_string();
        assert!(message.contains("skip-cert-verify"));
        assert!(message.contains("Old core remains unchanged"));
    }
    #[tokio::test]
    async fn guided_core_switch_rechecks_then_stops_launches_and_commits() {
        let calls = parking_lot::Mutex::new(Vec::new());
        orchestrate_guided_switch(
            || async {
                calls.lock().push("recheck");
                Ok(())
            },
            true,
            || async {
                calls.lock().push("stop");
                Ok(())
            },
            || async {
                calls.lock().push("launch-ready");
                Ok(())
            },
            || async {
                calls.lock().push("commit");
                Ok(())
            },
            || async {
                calls.lock().push("rollback");
                Ok(())
            },
        )
        .await
        .unwrap();
        assert_eq!(*calls.lock(), ["recheck", "stop", "launch-ready", "commit"]);
    }

    #[tokio::test]
    async fn guided_core_failed_recheck_has_zero_mutation_hooks() {
        let touched = AtomicBool::new(false);
        let result = orchestrate_guided_switch(
            || async { anyhow::bail!("generation changed") },
            true,
            || async {
                touched.store(true, Ordering::SeqCst);
                Ok(())
            },
            || async {
                touched.store(true, Ordering::SeqCst);
                Ok(())
            },
            || async {
                touched.store(true, Ordering::SeqCst);
                Ok(())
            },
            || async {
                touched.store(true, Ordering::SeqCst);
                Ok(())
            },
        )
        .await;
        assert!(result.is_err());
        assert!(!touched.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn guided_core_each_late_failure_restores_bytes_kind_and_selection() {
        for failure in ["stop", "launch", "commit"] {
            let home = tempfile::tempdir().unwrap();
            let config = home.path().join("config");
            let marker = home.path().join("marker");
            std::fs::write(&config, "proxy_core: mihomo\nunknown: preserved\nselector: node-a").unwrap();
            std::fs::write(&marker, "prior owner").unwrap();
            let snapshot = GuidedFileSnapshot::capture([config.clone(), marker.clone()]).unwrap();
            let manager = MihomoManager::new(home.path().into());
            let clone = manager.clone();
            let calls = parking_lot::Mutex::new(Vec::new());
            let result = orchestrate_guided_switch(
                || async { Ok(()) },
                true,
                || async {
                    calls.lock().push("stop");
                    if failure == "stop" {
                        anyhow::bail!("stop failed");
                    }
                    Ok(())
                },
                || async {
                    calls.lock().push("launch");
                    std::fs::write(&config, "target config").unwrap();
                    manager.inner.set_core_kind(CoreKind::SingBox);
                    if failure == "launch" {
                        anyhow::bail!("readiness failed");
                    }
                    Ok(())
                },
                || async {
                    calls.lock().push("commit");
                    std::fs::write(&marker, "target marker").unwrap();
                    anyhow::bail!("config-save/marker failed")
                },
                || async {
                    calls.lock().push("restore/restart-old");
                    snapshot.restore()?;
                    manager.inner.set_core_kind(CoreKind::Mihomo);
                    Ok(())
                },
            )
            .await;
            assert!(result.is_err());
            assert_eq!(calls.lock().last(), Some(&"restore/restart-old"));
            assert_eq!(
                std::fs::read_to_string(&config).unwrap(),
                "proxy_core: mihomo\nunknown: preserved\nselector: node-a"
            );
            assert_eq!(std::fs::read_to_string(&marker).unwrap(), "prior owner");
            assert_eq!(clone.core_kind(), CoreKind::Mihomo);
            assert!(matches!(
                clone.api().transport(),
                crate::mihomo_api::Transport::UnixSocket(_)
            ));
        }
    }

    #[tokio::test]
    async fn guided_core_stopped_switch_never_calls_stop() {
        let stopped = AtomicBool::new(false);
        orchestrate_guided_switch(
            || async { Ok(()) },
            false,
            || async {
                stopped.store(true, Ordering::SeqCst);
                Ok(())
            },
            || async { Ok(()) },
            || async { Ok(()) },
            || async { Ok(()) },
        )
        .await
        .unwrap();
        assert!(!stopped.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn guided_core_rollback_failure_is_separately_reported() {
        let result = orchestrate_guided_switch(
            || async { Ok(()) },
            true,
            || async { Ok(()) },
            || async { anyhow::bail!("target boot failed") },
            || async { Ok(()) },
            || async { anyhow::bail!("old boot failed") },
        )
        .await
        .unwrap_err();
        let error = format!("{result:#}");
        assert!(error.contains("target boot failed"));
        assert!(error.contains("Rollback also failed: old boot failed"));
    }

    #[test]
    fn guided_core_gui_and_foreign_owners_are_read_only_rejections() {
        let manager = MihomoManager::new(std::env::temp_dir());
        assert!(
            manager
                .guided_owner_check(true)
                .unwrap_err()
                .to_string()
                .contains("GUI")
        );
        *manager.inner.pid.lock() = Some(1234);
        assert!(
            manager
                .guided_owner_check(false)
                .unwrap_err()
                .to_string()
                .contains("another supervisor")
        );
        assert_eq!(manager.pid(), Some(1234));
        assert_eq!(manager.current_generation(), 0);
    }
    #[test]
    fn guided_core_shared_selection_updates_every_clone_and_transport() {
        let manager = super::MihomoManager::new(std::env::temp_dir());
        let clone = manager.clone();
        manager.inner().set_core_kind(super::CoreKind::SingBox);
        assert_eq!(manager.core_kind(), super::CoreKind::SingBox);
        assert_eq!(clone.core_kind(), super::CoreKind::SingBox);
        assert_eq!(clone.api().core_kind(), super::CoreKind::SingBox);
        assert!(matches!(clone.api().transport(), crate::mihomo_api::Transport::Tcp(_)));
    }
    use super::*;

    #[tokio::test]
    async fn barrier_removes_stale_socket_file() {
        let home = private_socket_fixture();
        let path = home.path().join("controller.sock");
        drop(std::os::unix::net::UnixListener::bind(&path).unwrap());
        assert!(path.exists());

        resource_barrier(&path, std::time::Duration::from_millis(50))
            .await
            .unwrap();

        assert!(!path.exists(), "stale socket must be removed by the barrier");
    }

    #[tokio::test]
    async fn barrier_returns_quickly_when_no_tun_present() {
        let started = std::time::Instant::now();
        let missing = std::env::temp_dir().join(format!("barrier-none-{}.sock", uuid::Uuid::new_v4()));
        resource_barrier(&missing, std::time::Duration::from_secs(5))
            .await
            .unwrap();
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
    fn adopted_core_is_restartable_but_a_foreign_one_is_not() {
        // #55: the supervisor owns the child, so `owns_child` is false in
        // every later CLI invocation — that must not block a profile apply
        // when a verified pid record exists.
        supervisor_restart_policy(false, Some(4242), CoreKind::SingBox, CoreKind::SingBox)
            .expect("a recorded core may be replaced through its supervisor");
        // Our own child always restarts in place.
        supervisor_restart_policy(true, Some(1), CoreKind::Mihomo, CoreKind::SingBox)
            .expect("an owned child needs no record check");
        // No pid record at all: somebody else's core.
        let foreign = supervisor_restart_policy(false, None, CoreKind::SingBox, CoreKind::SingBox)
            .expect_err("an unrecorded core must be refused");
        assert!(
            foreign.to_string().contains("no clash-verge-cli pid record"),
            "{foreign}"
        );
        // A running core the configuration no longer selects (#56): the
        // supervisor would start a different core than the one being
        // reconfigured, so refuse and point at `core use`.
        let mismatch = supervisor_restart_policy(false, Some(4242), CoreKind::Mihomo, CoreKind::SingBox)
            .expect_err("a core switch must be reconciled first");
        assert!(mismatch.to_string().contains("core use singbox"), "{mismatch}");
    }

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
        let dir = tempfile::tempdir().unwrap();
        let api = MihomoApi::new(dir.path().join("missing.sock"), "s").unwrap();

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

    // ── Sing-box cold-start ordering (reviewer P0-1, P1-2). ─────────────
    // The pipeline is structured exactly like `orchestrate_restart` so we
    // can drive it with fake steps and assert that resolve+preflight run
    // BEFORE the tracked core is stopped. That ordering is the whole point
    // of the reviewer finding: a resolve/preflight failure must never
    // take down a still-healthy core.
    fn fake_singbox_resolved(path: &str) -> crate::mihomo_manager::singbox_binary::ResolvedSingBox {
        crate::mihomo_manager::singbox_binary::ResolvedSingBox {
            path: PathBuf::from(path),
            source: crate::mihomo_manager::singbox_binary::SingboxBinarySource::System,
            version: "v1.13.12".into(),
        }
    }

    fn record_ok_preflight_singbox(
        events: &Arc<Mutex<Vec<&'static str>>>,
        event: &'static str,
    ) -> impl FnOnce(&crate::mihomo_manager::singbox_binary::ResolvedSingBox) -> std::future::Ready<anyhow::Result<()>>
    {
        let events = Arc::clone(events);
        move |_resolved| {
            events.lock().push(event);
            std::future::ready(Ok(()))
        }
    }

    fn record_ok_spawn_singbox(
        events: &Arc<Mutex<Vec<&'static str>>>,
        spawned: &Arc<Mutex<Option<PathBuf>>>,
    ) -> impl FnOnce(&crate::mihomo_manager::singbox_binary::ResolvedSingBox) -> std::future::Ready<anyhow::Result<()>>
    {
        let events = Arc::clone(events);
        let spawned = Arc::clone(spawned);
        move |resolved| {
            events.lock().push("spawn");
            *spawned.lock() = Some(resolved.path.clone());
            std::future::ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn start_singbox_pipeline_orders_foreign_resolve_preflight_before_stop() {
        // Reviewer P0-1 (reviewer): the sing-box cold start must run the
        // foreign-controller guard, resolve+preflight BEFORE stopping the
        // tracked predecessor. A failure anywhere before `stop` must
        // leave the running core untouched.
        let events: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::new(Vec::new()));
        let spawned: Arc<Mutex<Option<PathBuf>>> = Arc::new(Mutex::new(None));
        let resolved = fake_singbox_resolved("/fake/sing-box");

        let result = orchestrate_start_singbox(
            {
                let events = Arc::clone(&events);
                move || {
                    events.lock().push("foreign");
                    std::future::ready(Ok(()))
                }
            },
            {
                let events = Arc::clone(&events);
                let resolved = resolved.clone();
                move || {
                    events.lock().push("resolve");
                    async move { Ok(resolved) }
                }
            },
            record_ok_preflight_singbox(&events, "preflight"),
            record_ok(&events, "stop"),
            record_ok(&events, "port"),
            record_ok_spawn_singbox(&events, &spawned),
        )
        .await;

        result.expect("success path must resolve");
        assert_eq!(
            *events.lock(),
            ["foreign", "resolve", "preflight", "stop", "port", "spawn"],
            "P0-1: resolve+preflight MUST run before stop and port"
        );
        // resolve ran exactly once and spawn reused the same binary.
        assert_eq!(*spawned.lock(), Some(PathBuf::from("/fake/sing-box")));
    }

    #[tokio::test]
    async fn start_singbox_pipeline_foreign_failure_short_circuits_before_resolve() {
        // P1-2: when something else is already serving the sing-box
        // controller, the pipeline must abort BEFORE we touch the
        // currently running core or do any resolve work.
        let events: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::new(Vec::new()));
        let spawned: Arc<Mutex<Option<PathBuf>>> = Arc::new(Mutex::new(None));

        let result = orchestrate_start_singbox(
            {
                let events = Arc::clone(&events);
                move || {
                    events.lock().push("foreign");
                    std::future::ready(Err(anyhow::anyhow!(
                        "a sing-box core already answers on 127.0.0.1:9090"
                    )))
                }
            },
            {
                let events = Arc::clone(&events);
                move || {
                    events.lock().push("resolve");
                    std::future::ready(Err(anyhow::anyhow!("should never run")))
                }
            },
            record_ok_preflight_singbox(&events, "preflight"),
            record_ok(&events, "stop"),
            record_ok(&events, "port"),
            record_ok_spawn_singbox(&events, &spawned),
        )
        .await;

        assert!(result.is_err());
        assert_eq!(
            *events.lock(),
            ["foreign"],
            "P1-2: a foreign controller must abort before resolve/stop/spawn"
        );
        assert!(spawned.lock().is_none());
    }

    #[tokio::test]
    async fn start_singbox_pipeline_preflight_failure_keeps_running_core_intact() {
        // P0-1 (reviewer): if the resolved sing-box binary lacks the TUN
        // capability (or another preflight error), the currently running
        // core must remain untouched — no `stop`, no `spawn`.
        let events: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::new(Vec::new()));
        let spawned: Arc<Mutex<Option<PathBuf>>> = Arc::new(Mutex::new(None));
        let resolved = fake_singbox_resolved("/fake/sing-box");

        let result = orchestrate_start_singbox(
            {
                let events = Arc::clone(&events);
                move || {
                    events.lock().push("foreign");
                    std::future::ready(Ok(()))
                }
            },
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
            record_ok(&events, "port"),
            record_ok_spawn_singbox(&events, &spawned),
        )
        .await;

        assert!(result.is_err());
        assert_eq!(
            *events.lock(),
            ["foreign", "resolve", "preflight"],
            "P0-1: preflight failure must NEVER trigger stop"
        );
        assert!(spawned.lock().is_none());
    }

    // ── Readiness rollback (reviewer P1-1). ───────────────────────────────
    // Verifies that the rollback helper clears ALL half-spawn runtime
    // state — not just `owns_child`, which the previous code did. The
    // subsequent `start` would otherwise see a stale pid and refuse to
    // re-spawn via the adopted-core guard.
    #[test]
    fn rollback_failed_spawn_clears_pid_started_at_and_pidfile() {
        let inner = ManagerInner::new();
        let dir = std::env::temp_dir().join(format!("cv-rollback-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("external-controller.sock");
        let pidfile_path = pidfile::path_for(&socket);

        // Mimic the spawn-time state: pid set, started_at set, owns_child
        // set, and a fresh pidfile the rollback should drop.
        let pid = std::process::id();
        *inner.pid.lock() = Some(pid);
        *inner.started_at.lock() = Some(Utc::now());
        inner.owns_child.store(true, std::sync::atomic::Ordering::SeqCst);
        let now = Utc::now();
        pidfile::write(
            &pidfile_path,
            pidfile::CoreRecord::with_kind(pid, now, CoreKind::SingBox),
        )
        .unwrap();
        *inner.state.lock() = CoreState::Running;

        rollback_failed_spawn(&inner, &socket, pid, "probe timed out".into());

        assert!(inner.pid.lock().is_none(), "pid must be cleared");
        assert!(inner.started_at.lock().is_none(), "started_at must be cleared");
        assert!(
            !inner.owns_child.load(std::sync::atomic::Ordering::SeqCst),
            "owns_child must be cleared"
        );
        assert!(!pidfile_path.exists(), "pidfile must be removed");
        match inner.state.lock().clone() {
            CoreState::Error(msg) => assert_eq!(msg, "probe timed out"),
            other => panic!("state must be Error, got {other:?}"),
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rollback_failed_spawn_does_not_touch_a_foreign_pidfile() {
        // A newer core may have replaced the pidfile between spawn and
        // rollback. remove_if should refuse to drop the new record.
        let inner = ManagerInner::new();
        let dir = std::env::temp_dir().join(format!("cv-rollback-foreign-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("external-controller.sock");
        let pidfile_path = pidfile::path_for(&socket);

        let dead_pid = std::process::id();
        let other_pid = u32::MAX;
        pidfile::write(&pidfile_path, pidfile::CoreRecord::new(other_pid, Utc::now())).unwrap();

        rollback_failed_spawn(&inner, &socket, dead_pid, "boom".into());
        assert!(inner.pid.lock().is_none());
        assert!(
            pidfile_path.exists(),
            "a pidfile naming a different pid must not be removed"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── Cross-kind record guard (P0 leftover). ────────────────────────────
    // The mihomo.pid file is shared by both cores. Without this guard,
    // a sing-box `start` could bypass a running mihomo (the TCP foreign-
    // controller only checks 127.0.0.1:9090) and `spawn_core` would
    // overwrite the mihomo record with kind=SingBox. These tests pin
    // down `check_cross_kind_record` directly so the regression is
    // caught even before the surrounding `start` plumbing changes.

    fn fresh_socket() -> (std::path::PathBuf, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("cv-xkind-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("external-controller.sock");
        (dir, socket)
    }

    #[test]
    fn cross_kind_guard_passes_when_pidfile_absent() {
        let (dir, socket) = fresh_socket();
        check_cross_kind_record(&socket, CoreKind::SingBox).expect("no record must not block start");
        check_cross_kind_record(&socket, CoreKind::Mihomo).expect("no record must not block start");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cross_kind_guard_passes_for_matching_kind_even_when_alive() {
        // A mihomo record written by THIS process; the guard must let
        // a mihomo start proceed (the in-memory pid + foreign-controller
        // guards handle the rest).
        let (dir, socket) = fresh_socket();
        let pidfile_path = pidfile::path_for(&socket);
        pidfile::write(&pidfile_path, pidfile::CoreRecord::new(std::process::id(), Utc::now())).unwrap();
        check_cross_kind_record(&socket, CoreKind::Mihomo).expect("same-kind record must not block start");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cross_kind_guard_refuses_running_singbox_for_mihomo_start() {
        // P0 leftover: a sing-box record naming a live pid must block
        // a mihomo start. Otherwise the mihomo path would overwrite the
        // sing-box record and the sing-box supervisor would see a stale
        // pid on the next cross-process invocation.
        let (dir, socket) = fresh_socket();
        let pidfile_path = pidfile::path_for(&socket);
        pidfile::write(
            &pidfile_path,
            pidfile::CoreRecord::with_kind(std::process::id(), Utc::now(), CoreKind::SingBox),
        )
        .unwrap();
        let err =
            check_cross_kind_record(&socket, CoreKind::Mihomo).expect_err("mihomo start must refuse a running singbox");
        let msg = err.to_string();
        assert!(msg.contains("singbox"), "{msg}");
        assert!(msg.contains("recorded"), "{msg}");
        assert!(
            pidfile_path.exists(),
            "the guard must NOT have removed the foreign record"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cross_kind_guard_refuses_running_mihomo_for_singbox_start() {
        // The primary reviewer-flagged scenario: a mihomo supervisor
        // recorded its pid; a separate process tries to start sing-box.
        // The TCP foreign-controller guard only checks 127.0.0.1:9090,
        // which mihomo does not bind, so without this disk check the
        // sing-box start would clobber the mihomo record.
        let (dir, socket) = fresh_socket();
        let pidfile_path = pidfile::path_for(&socket);
        pidfile::write(&pidfile_path, pidfile::CoreRecord::new(std::process::id(), Utc::now())).unwrap();
        let err = check_cross_kind_record(&socket, CoreKind::SingBox)
            .expect_err("singbox start must refuse a running mihomo");
        let msg = err.to_string();
        assert!(msg.contains("mihomo"), "{msg}");
        assert!(msg.contains("recorded"), "{msg}");
        assert!(
            pidfile_path.exists(),
            "the guard must NOT have removed the foreign record"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cross_kind_guard_tolerates_stale_different_kind_record() {
        // A different-kind record whose pid is dead (kernel recycled it
        // or the process crashed) must NOT block the new start — the
        // upcoming spawn_core will overwrite the stale record harmlessly.
        let (dir, socket) = fresh_socket();
        let pidfile_path = pidfile::path_for(&socket);
        // u32::MAX is reserved and never an actual pid in practice.
        pidfile::write(
            &pidfile_path,
            pidfile::CoreRecord::with_kind(u32::MAX, Utc::now(), CoreKind::SingBox),
        )
        .unwrap();
        check_cross_kind_record(&socket, CoreKind::Mihomo)
            .expect("a stale sing-box record must not block a mihomo start");
        check_cross_kind_record(&socket, CoreKind::SingBox)
            .expect("a stale sing-box record is harmless for same-kind too");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

fn profile_tun_settings(
    yaml: Option<&str>,
    base: &serde_yaml_ng::Mapping,
) -> Result<crate::singbox::TunSettings, String> {
    let profile: Option<serde_yaml_ng::Mapping> = yaml
        .map(serde_yaml_ng::from_str)
        .transpose()
        .map_err(|error| format!("invalid profile TUN settings: {error}"))?;
    let native_tun = profile
        .as_ref()
        .and_then(|mapping| mapping.get("inbounds"))
        .and_then(serde_yaml_ng::Value::as_sequence)
        .and_then(|inbounds| {
            inbounds
                .iter()
                .find(|inbound| inbound.get("type").and_then(serde_yaml_ng::Value::as_str) == Some("tun"))
        });
    let tun = native_tun
        .or_else(|| profile.as_ref().and_then(|mapping| mapping.get("tun")))
        .or_else(|| base.get("tun"));
    let stack = tun
        .and_then(|tun| tun.get("stack"))
        .map(|stack| stack.as_str().ok_or_else(|| "TUN stack must be a string".to_string()))
        .transpose()?
        .unwrap_or("gvisor");
    crate::singbox::config_gen::validate_tun_stack(stack)?;
    let mtu = tun
        .and_then(|tun| tun.get("mtu"))
        .map(|mtu| {
            mtu.as_u64()
                .ok_or_else(|| "TUN mtu must be an unsigned integer".to_string())
        })
        .transpose()?
        .unwrap_or(9000);
    let mtu = u16::try_from(mtu).map_err(|_| "TUN mtu exceeds 65535".to_string())?;
    Ok(crate::singbox::TunSettings {
        stack: stack.into(),
        mtu,
    })
}

/// Converted profile routing plus the report lines describing what had to
/// be approximated or dropped (#52).
#[derive(Debug)]
struct ProfileRouteRules {
    rules: Vec<serde_json::Value>,
    /// `rules[i]` is where ORIGINAL profile rule `i` landed, `None` where the
    /// conversion dropped it. The rule-order sidecar indexes the original
    /// list, so interleaving without this mapping shifts every index after
    /// the first dropped rule onto the wrong rule.
    slots: Vec<Option<usize>>,
    /// `route.rule_set` entries the converted rules reference (geo sets
    /// synthesized from `GEOIP`/`GEOSITE`, providers from `rule-providers`).
    rule_sets: Vec<serde_json::Value>,
    /// Human-readable degradation lines, surfaced by `status` and the
    /// profile commands instead of failing the whole configuration.
    skipped: Vec<String>,
}

/// Convert a clash profile's `rules:` list into sing-box route rules.
///
/// Issue #52: the previous pass required every rule to be exactly three
/// comma-separated fields and refused anything it could not map, so a
/// single `no-resolve` modifier or one `GEOIP,CN,DIRECT` line made the
/// whole subscription unusable. The mapping now degrades gracefully:
///
/// - tolerated modifiers (`no-resolve`) are dropped and reported —
///   sing-box never resolves a domain for an `ip_cidr` rule anyway;
/// - `GEOIP`/`GEOSITE` become references to the official SagerNet `.srs`
///   rule-sets, which are emitted alongside the rules;
/// - `RULE-SET,<name>` resolves against the rule-sets converted from
///   `rule-providers` (`.srs` payloads only) plus the user's stored sets;
/// - a rule that still cannot be represented — an extra match field, a
///   negated geo set, an unconvertible provider, or a target outbound the
///   conversion could not produce — is skipped and reported instead of
///   aborting the configuration with a dangling reference.
fn profile_route_rules(
    yaml: &str,
    known_outbounds: &std::collections::HashSet<String>,
    known_rule_sets: &std::collections::HashSet<String>,
) -> Result<ProfileRouteRules, String> {
    let document: serde_yaml_ng::Value = serde_yaml_ng::from_str(yaml).map_err(|error| error.to_string())?;
    let mut result = ProfileRouteRules {
        rules: Vec::new(),
        slots: Vec::new(),
        rule_sets: Vec::new(),
        skipped: Vec::new(),
    };
    // Providers that already publish a sing-box payload become rule-sets,
    // so `RULE-SET` rules can point at them.
    let providers = crate::singbox::convert::convert_rule_providers(yaml).map_err(|error| error.to_string())?;
    result.rule_sets.extend(providers.rule_sets.clone());
    result
        .skipped
        .extend(providers.skipped.iter().map(|line| format!("rules: {line}")));
    let mut available_sets: std::collections::HashSet<String> = known_rule_sets.clone();
    available_sets.extend(
        providers
            .rule_sets
            .iter()
            .filter_map(|set| set.get("tag").and_then(serde_json::Value::as_str))
            .map(str::to_owned),
    );

    let Some(rules) = document.get("rules") else {
        return Ok(result);
    };
    let rules = rules.as_sequence().ok_or("profile rules must be a list")?.clone();
    // One slot per ORIGINAL rule, so a dropped rule keeps its index identity
    // even though nothing is emitted for it.
    result.slots = vec![None; rules.len()];
    for (index, rule) in rules.iter().enumerate() {
        let rule = rule
            .as_str()
            .ok_or_else(|| format!("profile rules[{index}] must be a string"))?;
        match convert_one_rule(rule, known_outbounds, &available_sets, &mut result) {
            Ok(()) => {
                // Exactly one route rule is emitted per converted rule, so
                // the slot is the position it was appended at.
                debug_assert_eq!(result.slots[index], None, "slot {index} was already filled");
                result.slots[index] = Some(result.rules.len() - 1);
            }
            // Only a structurally broken rule list is fatal; a single
            // unrepresentable rule degrades into the report.
            Err(reason) => result.skipped.push(format!("profile rules[{index}]: {reason}")),
        }
    }
    Ok(result)
}

/// Convert one clash rule, appending to `out` what it needs (the rule
/// itself and any geo rule-set it references).
fn convert_one_rule(
    rule: &str,
    known_outbounds: &std::collections::HashSet<String>,
    known_rule_sets: &std::collections::HashSet<String>,
    out: &mut ProfileRouteRules,
) -> Result<(), String> {
    let (fields, modifiers) = crate::routing::split_clash_rule(rule);
    if modifiers.is_empty() {
        // Nothing to report for a rule that used no modifier.
    } else {
        out.skipped.push(format!(
            "profile rule {rule:?}: dropped modifier(s) {}",
            modifiers.join(", ")
        ));
    }
    if fields.first().map(String::as_str) == Some("MATCH") {
        let target = fields.get(1).ok_or("MATCH rule has no target")?.clone();
        let tag = normalize_policy_tag(&target);
        if !known_outbounds.contains(&tag) {
            return Err(format!(
                "final MATCH target {target:?} is not an available outbound in the converted config; skipped"
            ));
        }
        // clash's catch-all `MATCH` is sing-box's `route.final`, which the
        // generator already emits; the rule is kept as a catch-all so rule
        // ORDER is preserved when other rules follow it.
        out.rules.push(serde_json::json!({ "outbound": tag }));
        return Ok(());
    }
    // kind, value, target — and nothing else is representable.
    if fields.len() != 3 {
        return Err(format!("unsupported syntax or match fields ({fields:?}); skipped"));
    }
    let target = fields[2].clone();
    let model = crate::routing::from_clash_rule_str(rule);
    // A `RULE-SET` rule only survives when the referenced set exists;
    // otherwise it would abort the config with a missing rule-set error.
    if let crate::routing::IRouteRule::Simple { matches, .. } = &model
        && matches.iter().any(|field| {
            // GEOIP/GEOSITE tags are synthesized right below; only a tag
            // that is neither stored nor derivable is unresolvable.
            matches!(field, crate::routing::MatchField::RuleSet(tag)
                if !known_rule_sets.contains(tag)
                    && crate::singbox::convert::geo_rule_set(tag).is_none())
        })
    {
        return Err(format!(
            "RULE-SET {rule:?} has no sing-box rule-set (clash rule-providers are only converted from .srs payloads); skipped"
        ));
    }
    let converted = crate::routing::to_singbox_json(&model)
        .ok_or_else(|| format!("kind {} cannot be represented by sing-box 1.14.2; skipped", fields[0]))?;
    // A rule whose target the conversion could not produce would be a
    // dangling outbound reference, which aborts the whole config. Check
    // before materializing any rule-set so a dropped rule never leaves an
    // orphan download.
    let tag = normalize_policy_tag(&target);
    if !known_outbounds.contains(&tag) {
        return Err(format!(
            "target {target:?} is not an available outbound in the converted config; skipped"
        ));
    }
    // GEOIP/GEOSITE map to rule-set references; materialize the sets.
    if let Some(tags) = converted.get("rule_set").and_then(serde_json::Value::as_array) {
        for tag in tags.iter().filter_map(serde_json::Value::as_str) {
            if known_rule_sets.contains(tag) {
                continue;
            }
            let set = crate::singbox::convert::geo_rule_set(tag)
                .ok_or_else(|| format!("rule-set {tag:?} is unknown; skipped"))?;
            out.rule_sets.push(set);
        }
    }
    let mut rule = converted;
    rule["outbound"] = serde_json::json!(tag);
    out.rules.push(rule);
    Ok(())
}

/// sing-box's built-in policy outbound tags.
fn normalize_policy_tag(target: &str) -> String {
    match target {
        "DIRECT" => "direct".into(),
        "REJECT" => "block".into(),
        other => other.into(),
    }
}

/// Merge generated geo/provider rule-sets with the user's stored ones.
/// Stored sets win on a tag clash; duplicates are dropped so
/// `config_gen::validate_references` sees unique tags.
fn merge_rule_sets(stored: Vec<serde_json::Value>, generated: Vec<serde_json::Value>) -> Vec<serde_json::Value> {
    let mut merged = stored;
    let mut tags: std::collections::HashSet<String> = merged
        .iter()
        .filter_map(|set| set.get("tag").and_then(serde_json::Value::as_str))
        .map(str::to_owned)
        .collect();
    for set in generated {
        let Some(tag) = set.get("tag").and_then(serde_json::Value::as_str).map(str::to_owned) else {
            continue;
        };
        if tags.insert(tag) {
            merged.push(set);
        }
    }
    merged
}

fn write_generated_json(path: &Path, config: &serde_json::Value) -> anyhow::Result<()> {
    use std::io::Write as _;
    let parent = path.parent().context("runtime config has no parent directory")?;
    std::fs::create_dir_all(parent)?;
    let mut staged = tempfile::NamedTempFile::new_in(parent)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        staged
            .as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    staged.write_all(&serde_json::to_vec_pretty(config)?)?;
    staged.as_file().sync_all()?;
    staged.persist(path).map_err(|error| error.error)?;
    Ok(())
}

fn apply_native_sidecars(config: &mut serde_json::Value, home: &Path) -> anyhow::Result<()> {
    if home.join(crate::singbox::RULE_SETS_FILE).exists() {
        config["route"]["rule_set"] =
            serde_json::json!(crate::singbox::load_rule_sets(home).map_err(anyhow::Error::msg)?);
    }
    if home.join(crate::singbox::LOGICAL_RULES_FILE).exists() {
        let rules = crate::singbox::load_logical_rules(home).map_err(anyhow::Error::msg)?;
        config["route"]["rules"] = serde_json::json!(
            rules
                .iter()
                .filter_map(crate::routing::to_singbox_json)
                .collect::<Vec<_>>()
        );
    }
    if home.join(crate::singbox::DNS_CONFIG_FILE).exists() {
        let dns = crate::singbox::load_dns_spec(home).map_err(anyhow::Error::msg)?;
        let generated = crate::singbox::dns::build_dns_section(&dns).map_err(anyhow::Error::msg)?;
        if !config["dns"].is_object() {
            config["dns"] = serde_json::json!({});
        }
        let owned = config["dns"].as_object_mut().expect("object initialized");
        owned.remove("servers");
        owned.remove("rules");
        if let Some(serde_json::Value::Object(fields)) = generated {
            owned.extend(fields);
        }
        if let Some(resolver) = crate::singbox::dns::default_domain_resolver(&dns) {
            config["route"]["default_domain_resolver"] = serde_json::json!(resolver);
        } else {
            config["route"]
                .as_object_mut()
                .context("native route is not an object")?
                .remove("default_domain_resolver");
        }
    }
    Ok(())
}

#[cfg(test)]
mod alignment_regressions {
    use super::*;

    #[test]
    fn profile_tun_preserves_supported_stack_and_rejects_malformed_or_mihomo_only_stack() {
        let base = serde_yaml_ng::Mapping::new();
        for stack in ["gvisor", "system", "mixed"] {
            let yaml = format!("tun:\n  stack: {stack}\n  mtu: 1500\n");
            let settings = profile_tun_settings(Some(&yaml), &base).unwrap();
            assert_eq!(settings.stack, stack);
            assert_eq!(settings.mtu, 1500);
        }
        assert!(profile_tun_settings(Some("tun: {stack: mips}"), &base).is_err());
        assert!(profile_tun_settings(Some("tun: {stack: 7}"), &base).is_err());
        let native = r#"{"inbounds":[{"type":"tun","stack":"gvisor","mtu":1400}]}"#;
        assert_eq!(profile_tun_settings(Some(native), &base).unwrap().mtu, 1400);
    }

    fn tags(names: &[&str]) -> std::collections::HashSet<String> {
        names.iter().map(|name| (*name).to_string()).collect()
    }

    #[test]
    fn profile_routing_preserves_final_policy_and_order() {
        let outbounds = tags(&["Proxy", "direct", "block"]);
        let converted = profile_route_rules(
            "rules: ['DOMAIN,example.com,DIRECT', 'MATCH,Proxy']",
            &outbounds,
            &Default::default(),
        )
        .unwrap();
        assert_eq!(converted.rules[0]["outbound"], "direct");
        assert_eq!(converted.rules[0]["domain"], serde_json::json!(["example.com"]));
        assert_eq!(converted.rules[1], serde_json::json!({"outbound":"Proxy"}));
        assert!(converted.skipped.is_empty(), "{:?}", converted.skipped);
    }

    #[test]
    fn no_resolve_geo_rules_and_srs_providers_now_convert_with_a_report() {
        // #52: every one of these used to abort the whole configuration.
        let outbounds = tags(&["PROXY", "direct", "block"]);
        let yaml = "rules: [\n  'GEOIP,CN,DIRECT',\n  'GEOSITE,geolocation-!cn,REJECT',\n  'IP-CIDR,10.0.0.0/8,DIRECT,no-resolve',\n  'RULE-SET,ads,REJECT',\n  'RULE-SET,cn,PROXY',\n  'MATCH,PROXY',\n]\nrule-providers:\n  ads:\n    behavior: domain\n    url: https://example.com/ads.srs\n  cn:\n    behavior: classical\n    url: https://example.com/cn.list\n";
        let converted = profile_route_rules(yaml, &outbounds, &Default::default()).unwrap();
        assert_eq!(
            converted.rules.len(),
            5,
            "the classical-provider rule is skipped: {:?}",
            converted.rules
        );
        assert_eq!(converted.rules[0]["rule_set"], serde_json::json!(["geoip-cn"]));
        assert_eq!(
            converted.rules[1]["rule_set"],
            serde_json::json!(["geosite-geolocation-!cn"])
        );
        assert_eq!(converted.rules[2]["ip_cidr"], serde_json::json!(["10.0.0.0/8"]));
        assert_eq!(converted.rules[2]["outbound"], "direct");
        assert_eq!(converted.rules[3]["rule_set"], serde_json::json!(["ads"]));
        assert_eq!(converted.rules[3]["outbound"], "block");
        // A classical provider has no .srs payload: that ONE rule degrades.
        assert!(
            converted
                .skipped
                .iter()
                .any(|line| line.contains("RULE-SET") && line.contains("cn")),
            "{:?}",
            converted.skipped
        );
        assert_eq!(converted.rules[4], serde_json::json!({"outbound": "PROXY"}));
        // geo sets + the .srs provider are materialized for the references.
        let tags: Vec<&str> = converted
            .rule_sets
            .iter()
            .filter_map(|set| set.get("tag").and_then(serde_json::Value::as_str))
            .collect();
        assert_eq!(tags, vec!["ads", "geoip-cn", "geosite-geolocation-!cn"]);
        // The dropped no-resolve modifier is reported, not silently lost.
        assert!(
            converted.skipped.iter().any(|line| line.contains("no-resolve")),
            "{:?}",
            converted.skipped
        );
    }

    #[test]
    fn unrepresentable_and_dangling_rules_degrade_instead_of_failing() {
        let outbounds = tags(&["PROXY", "direct", "block"]);
        let yaml = "rules: [\n  'IP-CIDR,10.0.0.0/8,udp,DIRECT',\n  'GEOIP,!cn,REJECT',\n  'SRC-IP-CIDR,10.0.0.0/8,DIRECT',\n  'DOMAIN,a.com,MissingGroup',\n  'MATCH,PROXY',\n]\n";
        let converted = profile_route_rules(yaml, &outbounds, &Default::default()).unwrap();
        assert_eq!(
            converted.rules,
            vec![serde_json::json!({"outbound":"PROXY"})],
            "only the final policy survives"
        );
        assert_eq!(converted.skipped.len(), 4, "{:?}", converted.skipped);
        assert!(converted.skipped.iter().any(|line| line.contains("MissingGroup")));
        assert!(converted.skipped.iter().any(|line| line.contains("SRC-IP-CIDR")));
    }

    #[test]
    fn structurally_broken_rule_lists_are_still_refused() {
        // Degradation covers semantics, never a malformed document.
        assert!(profile_route_rules("rules: 7", &tags(&["direct"]), &Default::default()).is_err());
        assert!(
            profile_route_rules("rules: [7]", &tags(&["direct"]), &Default::default())
                .unwrap_err()
                .contains("must be a string")
        );
    }

    #[test]
    fn merged_rule_sets_keep_stored_sets_and_drop_duplicates() {
        let merged = merge_rule_sets(
            vec![serde_json::json!({"tag":"ads","type":"local","path":"a.srs"})],
            vec![
                serde_json::json!({"tag":"ads","type":"remote","url":"https://x/ads.srs"}),
                serde_json::json!({"tag":"geoip-cn","type":"remote","url":"https://x/geoip-cn.srs"}),
            ],
        );
        assert_eq!(merged.len(), 2, "{merged:?}");
        assert_eq!(merged[0]["type"], "local", "a stored set wins the tag");
        assert_eq!(merged[1]["tag"], "geoip-cn");
    }

    #[test]
    fn native_sidecars_preserve_unknown_fields_and_runtime_files_are_private() {
        let home = tempfile::tempdir().unwrap();
        crate::singbox::save_dns_spec(home.path(), &crate::singbox::DnsConfigSpec::default()).unwrap();
        let mut config = serde_json::json!({"dns":{"independent_cache":true,"servers":[]},"route":{"auto_detect_interface":true},"custom":{"future":1}});
        apply_native_sidecars(&mut config, home.path()).unwrap();
        assert_eq!(config["dns"]["independent_cache"], true);
        assert_eq!(config["route"]["auto_detect_interface"], true);
        assert_eq!(config["custom"]["future"], 1);
        let output = home.path().join("runtime.json");
        write_generated_json(&output, &config).unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&std::fs::read(&output).unwrap()).unwrap(),
            config
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(output).unwrap().permissions().mode() & 0o777, 0o600);
        }
    }

    #[tokio::test]
    async fn intentional_restart_suppresses_predecessor_exit_but_keeps_ready_ordered() {
        let inner = ManagerInner::new();
        let (tx, mut rx) = tokio::sync::mpsc::channel(2);
        *inner.action_tx.lock() = Some(tx);
        inner.generation.store(4, std::sync::atomic::Ordering::SeqCst);
        inner.restarting.store(true, std::sync::atomic::Ordering::SeqCst);
        inner.send_action(Action::CoreExited(0)).await;
        assert!(rx.try_recv().is_err());
        inner
            .send_action(Action::CoreStarted {
                version: None,
                binary_path: None,
                binary_source: None,
            })
            .await;
        assert!(
            matches!(rx.recv().await, Some(Action::CoreGeneration { generation: 4, action }) if matches!(*action, Action::CoreStarted {..}))
        );
        inner.restarting.store(false, std::sync::atomic::Ordering::SeqCst);
        inner.send_action(Action::CoreExited(0)).await;
        assert!(
            matches!(rx.recv().await, Some(Action::CoreGeneration { generation: 4, action }) if matches!(*action, Action::CoreExited(0)))
        );
    }

    /// Regression #48: a template/placeholder secret in config.yaml is
    /// rotated into the generated singbox.json, persisted, and reused on a
    /// second generation pass (no per-write rotation, no drift).
    #[tokio::test]
    async fn generated_singbox_carries_a_rotated_secret_and_never_the_template_one() {
        let home = tempfile::tempdir().unwrap();
        let _guard = crate::profile_store::store::tests::claim_test_app_home(home.path().to_path_buf()).await;
        std::fs::write(home.path().join("verge.yaml"), "verge_mixed_port: 35123\n").unwrap();
        std::fs::write(
            home.path().join("config.yaml"),
            "mixed-port: 35123\nexternal-controller: 127.0.0.1:49715\nsecret: set-your-secret\n",
        )
        .unwrap();

        let first = home.path().join("candidate-1.json");
        ManagerInner::write_singbox_assembled_to(home.path(), None, false, &first)
            .await
            .unwrap();
        let text = std::fs::read_to_string(&first).unwrap();
        assert!(!text.contains("set-your-secret"), "template secret leaked: {text}");
        assert!(!text.contains("\"*\""), "wildcard CORS origin emitted: {text}");

        let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
        let clash_api = &parsed["experimental"]["clash_api"];
        let secret = clash_api["secret"].as_str().expect("secret").to_string();
        assert!(!secret.is_empty() && secret != "set-your-secret");
        assert_eq!(
            clash_api["access_control_allow_private_network"],
            serde_json::Value::from(false)
        );
        assert!(
            clash_api["access_control_allow_origin"]
                .as_array()
                .expect("allow origins")
                .iter()
                .all(|origin| origin != "*")
        );

        // Persisted into config.yaml, mode 600, and shared with the mihomo side.
        let saved = std::fs::read_to_string(home.path().join("config.yaml")).unwrap();
        assert!(saved.contains(&secret), "config.yaml did not keep the rotated secret");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(home.path().join("config.yaml"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600, "controller config must stay private");
        }

        // The same secret survives a second generation pass.
        let second = home.path().join("candidate-2.json");
        ManagerInner::write_singbox_assembled_to(home.path(), None, false, &second)
            .await
            .unwrap();
        let again: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&second).unwrap()).unwrap();
        assert_eq!(
            again["experimental"]["clash_api"]["secret"],
            serde_json::Value::from(secret.as_str())
        );
    }

    /// Regression #49: `start` preparation against a completely empty config
    /// dir composes config.yaml (rather than failing with a bare os error 2)
    /// and resolves a usable controller secret, with no core spawned.
    #[tokio::test]
    async fn fresh_install_config_dir_gets_a_composed_config_and_secret() {
        let home = tempfile::tempdir().unwrap();
        let _guard = crate::profile_store::store::tests::claim_test_app_home(home.path().to_path_buf()).await;
        assert!(
            !home.path().join("config.yaml").exists(),
            "precondition: empty config dir"
        );

        // The read used by the spawn path must tolerate the missing file.
        assert_eq!(
            controller_secret_from_config(Some(&home.path().join("config.yaml"))).unwrap(),
            String::new()
        );

        // What `spawn_core_as` runs before touching the child process.
        let secret = crate::enhance::resolve_controller_secret().await.unwrap();
        assert!(!secret.is_empty());
        assert_ne!(secret, "set-your-secret");

        let path = home.path().join("config.yaml");
        assert!(path.exists(), "config.yaml must be composed on a fresh install");
        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(saved.contains(&secret));
        assert!(saved.contains("external-controller-unix"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }

        // The spawn path now reads a real secret instead of erroring.
        assert_eq!(controller_secret_from_config(Some(&path)).unwrap(), secret);
        // Idempotent: a second preparation does not rotate the secret again.
        assert_eq!(crate::enhance::resolve_controller_secret().await.unwrap(), secret);
    }

    /// Regression #49: an existing, user-set secret is preserved verbatim
    /// (we never silently re-key a configured controller).
    #[tokio::test]
    async fn a_configured_controller_secret_is_left_alone() {
        let home = tempfile::tempdir().unwrap();
        let _guard = crate::profile_store::store::tests::claim_test_app_home(home.path().to_path_buf()).await;
        std::fs::write(home.path().join("config.yaml"), "secret: user-picked\n").unwrap();
        assert_eq!(
            crate::enhance::resolve_controller_secret().await.unwrap(),
            "user-picked"
        );
        assert!(
            std::fs::read_to_string(home.path().join("config.yaml"))
                .unwrap()
                .contains("user-picked")
        );
    }
    /// P1-A: the secret resolved at spawn time must reach the live manager.
    /// A manager is built from `config.yaml` *before* the rotation, so it
    /// keeps the template's `set-your-secret`; sing-box enforces the bearer
    /// secret on its TCP clash_api, so every controller call answered 401 and
    /// a fresh-install start failed readiness.
    #[tokio::test]
    async fn a_rotated_secret_is_propagated_into_the_live_manager() {
        let home = tempfile::tempdir().unwrap();
        let _guard = crate::profile_store::store::tests::claim_test_app_home(home.path().to_path_buf()).await;
        std::fs::write(
            home.path().join("config.yaml"),
            "mixed-port: 35123\nexternal-controller: 127.0.0.1:49715\nsecret: set-your-secret\n",
        )
        .unwrap();

        let manager = MihomoManager::new(home.path().to_path_buf())
            .with_core_kind(CoreKind::SingBox)
            .with_socket(home.path().join("external-controller.sock"))
            .with_singbox_controller("127.0.0.1:49715".parse().unwrap())
            .with_secret("set-your-secret".into());
        // The manager's own snapshot is still the template secret; only the
        // spawn-path publication below fixes that (the process-wide fallback
        // may already hold another test's secret, so it is not asserted here).

        // What `spawn_core_as` does after resolving.
        let secret = crate::enhance::resolve_controller_secret().await.unwrap();
        manager.inner().set_secret_override(secret.clone());

        assert_eq!(
            manager.effective_secret(),
            secret,
            "api() must present the rotated secret"
        );
        assert_ne!(manager.effective_secret(), "set-your-secret");
    }

    /// P1-A, other producer: the sing-box config generation path resolves the
    /// same secret, and a manager built before it must pick it up too.
    #[tokio::test]
    async fn a_singbox_generation_secret_reaches_a_previously_built_manager() {
        let home = tempfile::tempdir().unwrap();
        let _guard = crate::profile_store::store::tests::claim_test_app_home(home.path().to_path_buf()).await;
        std::fs::write(
            home.path().join("config.yaml"),
            "mixed-port: 35123\nexternal-controller: 127.0.0.1:49715\nsecret: set-your-secret\n",
        )
        .unwrap();

        let manager = MihomoManager::new(home.path().to_path_buf())
            .with_core_kind(CoreKind::SingBox)
            .with_socket(home.path().join("external-controller.sock"))
            .with_singbox_controller("127.0.0.1:49715".parse().unwrap())
            .with_secret("set-your-secret".into());

        let generated = home.path().join("candidate.json");
        let (_path, _parts) = ManagerInner::write_singbox_assembled_to(home.path(), None, false, &generated)
            .await
            .unwrap();
        let generated_secret = crate::enhance::last_resolved_secret().expect("resolved secret published");
        let written: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&generated).unwrap()).unwrap();
        assert_eq!(
            written["experimental"]["clash_api"]["secret"],
            serde_json::Value::from(generated_secret.as_str())
        );
        assert_eq!(manager.effective_secret(), generated_secret);
    }

    /// Review regression: the TUI rules editor lets a logical (AND/OR) rule be
    /// moved above the profile `MATCH`. Persisting it must keep it above the
    /// catch-all in the generated config — appended after the whole profile
    /// rule list the logical rule was dead code, so a request matching it was
    /// never blocked.
    #[tokio::test]
    async fn a_logical_rule_ordered_before_the_profile_match_still_precedes_the_catch_all() {
        use crate::routing::{IRouteRule, LogicOp, MatchField, RuleTarget};
        use crate::singbox::{RuleOrder, RuleOrderEntry};

        let home = tempfile::tempdir().unwrap();
        let _guard = crate::profile_store::store::tests::claim_test_app_home(home.path().to_path_buf()).await;
        std::fs::write(home.path().join("verge.yaml"), "verge_mixed_port: 35123\n").unwrap();
        std::fs::write(
            home.path().join("config.yaml"),
            "mixed-port: 35123\nexternal-controller: 127.0.0.1:49715\nsecret: fixture\n",
        )
        .unwrap();

        // What the editor persists for the buffer
        // `OR(domain=blocked.example) -> block`, `MATCH -> DIRECT`.
        let block = IRouteRule::Logical {
            op: LogicOp::Or,
            rules: vec![IRouteRule::Simple {
                matches: vec![MatchField::Domain("blocked.example".into())],
                target: RuleTarget::Direct,
            }],
            target: RuleTarget::Block,
        };
        crate::singbox::save_rule_order(
            home.path(),
            &RuleOrder {
                logical: vec![block],
                entries: vec![RuleOrderEntry::Logical(0), RuleOrderEntry::Profile(0)],
                profile: None,
            },
        )
        .unwrap();

        let yaml = "proxies: []\nrules:\n  - MATCH,DIRECT\n";
        let generated = home.path().join("candidate.json");
        ManagerInner::write_singbox_assembled_to(home.path(), Some(yaml), false, &generated)
            .await
            .unwrap();
        let config: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&generated).unwrap()).unwrap();
        let rules = config["route"]["rules"].as_array().expect("route rules");
        let logical_at = rules
            .iter()
            .position(|rule| rule["outbound"] == "block")
            .expect("the ordered logical rule must be emitted");
        let catch_all_at = rules
            .iter()
            .position(|rule| {
                rule.as_object()
                    .is_some_and(|rule| rule.len() == 1 && rule.contains_key("outbound"))
            })
            .expect("the profile MATCH catch-all must be emitted");
        assert!(
            logical_at < catch_all_at,
            "logical rule landed after the catch-all: {rules:?}"
        );
        assert_eq!(rules[logical_at]["type"], serde_json::json!("logical"));
        assert_eq!(rules[logical_at]["mode"], serde_json::json!("or"));
        assert_eq!(
            rules[logical_at]["rules"][0]["domain"],
            serde_json::json!(["blocked.example"])
        );
    }
}

#[cfg(test)]
/// P1 (reviewer), conditional race: a supervisor that started but missed the
/// readiness window must be terminated AND reaped — otherwise it keeps
/// watching a core that competes with the recovery's replacement for the
/// controller socket.
mod supervisor_reaping {
    use super::*;

    #[tokio::test]
    async fn a_supervisor_that_missed_the_readiness_window_is_killed_and_reaped() {
        let _home = tempfile::tempdir().unwrap();
        #[allow(clippy::zombie_processes)]
        let mut supervisor = std::process::Command::new("/bin/sleep")
            .arg("300")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let pid = supervisor.id();
        assert!(pidfile::is_running(pid));

        crate::commands::start::reap_supervisor(&mut supervisor);

        assert!(!pidfile::is_running(pid), "the timed-out supervisor must be gone");
        // Reaped, not just signalled: `try_wait` collects it instead of
        // leaving a zombie for the rest of the process' life.
        let status = supervisor.try_wait().expect("reaped");
        assert!(status.is_some(), "the supervisor child must have been waited on");
    }
}

/// P1 (reviewer): the readiness probe after a supervisor launch must bind to
/// THIS replacement's identity, not to "something answers on the port".
mod replacement_identity {
    use super::*;
    use crate::mihomo_manager::pidfile::{self, CoreRecord};

    fn manager_with_record(home: &std::path::Path, record: Option<CoreRecord>) -> MihomoManager {
        let socket = home.join("controller.sock");
        match record {
            Some(record) => pidfile::write(&pidfile::path_for(&socket), record).unwrap(),
            None => {
                let _ = std::fs::remove_file(pidfile::path_for(&socket));
            }
        }
        MihomoManager::new(home.join("run"))
            .with_socket(socket)
            .with_singbox_controller("127.0.0.1:49715".parse().unwrap())
            .with_core_kind(CoreKind::SingBox)
    }

    fn record(pid: u32, kind: CoreKind, started: chrono::DateTime<chrono::Utc>) -> CoreRecord {
        CoreRecord::with_kind(pid, started, kind)
    }

    /// A live stand-in process: the identity check only asks whether the pid
    /// is alive, so any process will do.
    fn live_pid(live: &mut Vec<u32>) -> u32 {
        #[allow(clippy::zombie_processes)]
        let child = std::process::Command::new("/bin/sleep")
            .arg("30")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        live.push(child.id());
        child.id()
    }

    fn authorization(pid: u32) -> RestartAuthorization {
        RestartAuthorization {
            kind: CoreKind::SingBox,
            pid,
            exe: None,
        }
    }

    fn cleanup(live: &[u32]) {
        for pid in live {
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(*pid as i32),
                nix::sys::signal::Signal::SIGKILL,
            );
        }
    }

    #[test]
    fn a_live_record_for_this_kind_started_after_the_launch_is_the_replacement() {
        let home = tempfile::tempdir().unwrap();
        let mut live = Vec::new();
        let replaced = live_pid(&mut live);
        let replacement = live_pid(&mut live);
        let launched_at = std::time::SystemTime::now();
        let manager = manager_with_record(
            home.path(),
            Some(record(replacement, CoreKind::SingBox, chrono::Utc::now())),
        );
        manager
            .verify_replacement_core(&authorization(replaced), launched_at)
            .expect("a fresh live record of this kind is the replacement");
        cleanup(&live);
    }

    #[test]
    fn a_core_that_predates_the_launch_or_is_the_replaced_one_is_refused() {
        let home = tempfile::tempdir().unwrap();
        let mut live = Vec::new();
        let replaced = live_pid(&mut live);
        let other = live_pid(&mut live);
        let launched_at = std::time::SystemTime::now();

        // The very core the transaction was authorized to replace, still
        // answering: the replacement never started.
        let manager = manager_with_record(
            home.path(),
            Some(record(replaced, CoreKind::SingBox, chrono::Utc::now())),
        );
        assert!(
            manager
                .verify_replacement_core(&authorization(replaced), launched_at)
                .expect_err("the replaced core is not a replacement")
                .to_string()
                .contains("never started")
        );

        // A core that started well before this launch: something else is
        // answering; this transaction did not start it.
        let stale = chrono::Utc::now() - chrono::Duration::hours(1);
        let manager = manager_with_record(home.path(), Some(record(other, CoreKind::SingBox, stale)));
        assert!(
            manager
                .verify_replacement_core(&authorization(replaced), launched_at)
                .expect_err("a predecessor is not a replacement")
                .to_string()
                .contains("predates the replacement")
        );

        // No record at all: nothing can be attributed to this restart.
        let manager = manager_with_record(home.path(), None);
        assert!(
            manager
                .verify_replacement_core(&authorization(replaced), launched_at)
                .expect_err("no record, no attribution")
                .to_string()
                .contains("no live")
        );

        // A record of another core kind is not this replacement either.
        let manager = manager_with_record(home.path(), Some(record(other, CoreKind::Mihomo, chrono::Utc::now())));
        assert!(
            manager
                .verify_replacement_core(&authorization(replaced), launched_at)
                .expect_err("a foreign kind is not a replacement")
                .to_string()
                .contains("no live")
        );

        cleanup(&live);
    }
}

#[cfg(test)]
/// Helper: the profile-scoped fixtures every conversion test in this module
/// needs (mixed port, controller, secret) behind one temp app home.
/// The app-home lock is process-wide (the home dir is global state), so the
/// guard has to outlive the fixture: dropping it early would let a parallel
/// test repoint every path lookup at ITS home.
async fn conversion_fixture() -> (tempfile::TempDir, tokio::sync::MutexGuard<'static, ()>) {
    let home = tempfile::tempdir().unwrap();
    let guard = crate::profile_store::store::tests::claim_test_app_home(home.path().to_path_buf()).await;
    std::fs::write(home.path().join("verge.yaml"), "verge_mixed_port: 35123\n").unwrap();
    std::fs::write(
        home.path().join("config.yaml"),
        "mixed-port: 35123\nexternal-controller: 127.0.0.1:49715\nsecret: fixture\n",
    )
    .unwrap();
    (home, guard)
}

#[cfg(test)]
/// F2 (reviewer, minimal repro): the sidecar's `Profile(i)` indices are
/// ORIGINAL profile-rule indices, but generation used to interleave against
/// the COMPRESSED list. One dropped rule (here `DOMAIN-REGEX`, which the
/// conversion cannot express) shifted every later index by one, so the block
/// rule ordered between it and the `MATCH` was emitted AFTER the catch-all —
/// where the core never evaluates it — and the editor reopened showing the
/// two the other way round.
mod rule_order_slots {
    use super::*;
    use crate::singbox::{RuleOrder, RuleOrderEntry};

    const SCENE: &str = "proxies: []\nrules:\n  - DOMAIN-REGEX,ads\\..*,REJECT\n  - MATCH,DIRECT\n";

    fn block_rule() -> crate::routing::IRouteRule {
        use crate::routing::{IRouteRule, LogicOp, MatchField, RuleTarget};
        IRouteRule::Logical {
            op: LogicOp::Or,
            rules: vec![IRouteRule::Simple {
                matches: vec![MatchField::Domain("blocked.example".into())],
                target: RuleTarget::Direct,
            }],
            target: RuleTarget::Block,
        }
    }

    fn generated_route_rules(path: &std::path::Path) -> Vec<serde_json::Value> {
        let config: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        config["route"]["rules"]
            .as_array()
            .cloned()
            .unwrap_or_else(|| panic!("no route rules in {}", path.display()))
    }

    #[tokio::test]
    async fn a_dropped_profile_rule_does_not_shift_the_saved_rule_order() {
        let (home, _home_guard) = super::conversion_fixture().await;
        let fingerprint =
            crate::singbox::profile_rule_fingerprint(&crate::routing::load_profile_rules(SCENE).expect("parse scene"));
        // The editor's save for the buffer P0(dropable), L0(block), P1(MATCH).
        crate::singbox::save_rule_order(
            home.path(),
            &RuleOrder {
                logical: vec![block_rule()],
                entries: vec![
                    RuleOrderEntry::Profile(0),
                    RuleOrderEntry::Logical(0),
                    RuleOrderEntry::Profile(1),
                ],
                profile: Some(fingerprint),
            },
        )
        .unwrap();

        let generated = home.path().join("candidate.json");
        ManagerInner::write_singbox_assembled_to(home.path(), Some(SCENE), false, &generated)
            .await
            .unwrap();
        let rules = generated_route_rules(&generated);

        // Precondition of the bug: the conversion dropped P0, so only the
        // MATCH survived and the compressed list is one slot long.
        let converted =
            profile_route_rules(SCENE, &["direct".into()].into_iter().collect(), &Default::default()).unwrap();
        assert_eq!(converted.rules.len(), 1, "{:?}", converted.rules);
        assert_eq!(converted.slots, vec![None, Some(0)]);

        let block_at = rules
            .iter()
            .position(|rule| rule["type"] == serde_json::json!("logical"))
            .expect("the logical block rule must be emitted");
        let catch_all_at = rules
            .iter()
            .position(|rule| {
                rule.as_object()
                    .is_some_and(|rule| rule.len() == 1 && rule.contains_key("outbound"))
            })
            .expect("the profile MATCH catch-all must be emitted");
        assert!(
            block_at < catch_all_at,
            "the block rule landed after the catch-all and would never fire: {rules:?}"
        );
    }

    #[tokio::test]
    async fn a_subscription_refresh_that_replaced_the_rule_list_is_detected_as_drift() {
        let (home, _home_guard) = super::conversion_fixture().await;
        let original = "proxies: []\nrules:\n  - DOMAIN,a.example,DIRECT\n  - MATCH,DIRECT\n";
        let refreshed =
            "proxies: []\nrules:\n  - DOMAIN,new1.example,DIRECT\n  - DOMAIN,new2.example,DIRECT\n  - MATCH,DIRECT\n";
        // Stored against the ORIGINAL list.
        let order = RuleOrder {
            logical: vec![block_rule()],
            entries: vec![
                RuleOrderEntry::Profile(0),
                RuleOrderEntry::Logical(0),
                RuleOrderEntry::Profile(1),
            ],
            profile: Some(crate::singbox::profile_rule_fingerprint(
                &crate::routing::load_profile_rules(original).unwrap(),
            )),
        };
        crate::singbox::save_rule_order(home.path(), &order).unwrap();

        let stored = crate::singbox::load_rule_order(home.path()).expect("sidecar");
        assert_eq!(stored.profile, order.profile, "the fingerprint is persisted");

        let actual = crate::singbox::profile_rule_fingerprint(&crate::routing::load_profile_rules(refreshed).unwrap());
        assert!(
            !stored.matches_profile(actual),
            "a replaced rule list is an identity change"
        );
        let (effective, note) = stored.resolve_profile_drift(actual);
        assert!(effective.entries.is_empty(), "the stale indices must be dropped");
        assert!(effective.logical.len() == 1, "the logical rule itself is kept");
        let note = note.expect("the fallback is reported, not silent");
        assert!(note.contains("rule order reset"), "{note}");

        // Generation applies the same demotion and reports it in the
        // degradation notes.
        let generated = home.path().join("candidate.json");
        let (_path, parts) = ManagerInner::write_singbox_assembled_to(home.path(), Some(refreshed), false, &generated)
            .await
            .unwrap();
        assert!(
            parts
                .conversion
                .notes
                .iter()
                .any(|line| line.contains("rule order reset")),
            "{:?}",
            parts.conversion.notes
        );
        let rules = generated_route_rules(&generated);
        let block_at = rules
            .iter()
            .position(|rule| rule["type"] == serde_json::json!("logical"))
            .unwrap();
        let catch_all_at = rules
            .iter()
            .position(|rule| {
                rule.as_object()
                    .is_some_and(|rule| rule.len() == 1 && rule.contains_key("outbound"))
            })
            .unwrap();
        assert_eq!(
            block_at,
            rules.len() - 1,
            "append-after is the documented fallback: {rules:?}"
        );
        assert!(
            catch_all_at < block_at,
            "the fallback still emits every profile rule first: {rules:?}"
        );
    }
}

/// F1 (reviewer): a recovery start must serve the config that is already on
/// disk. The apply transaction restores the previous config (A) and then
/// asks a supervisor to bring the service back; a supervisor launched the
/// normal way regenerates the runtime config from the active profile — by
/// then the NEWER profile (B) the failed apply had just persisted — so the
/// "recovered" service ran B and overwrote A again.
mod recovery_config_selection {
    use super::*;

    const PROFILE_A: &str = "proxies: []\nrules:\n  - DOMAIN,a-recovery-marker.example,DIRECT\n  - MATCH,DIRECT\n";
    const PROFILE_B: &str = "proxies: []\nrules:\n  - DOMAIN,b-apply-marker.example,DIRECT\n  - MATCH,DIRECT\n";

    #[tokio::test]
    async fn a_recovery_start_serves_the_restored_config_instead_of_regenerating_it() {
        let (home, _home_guard) = super::conversion_fixture().await;
        std::fs::write(
            home.path().join("profiles.yaml"),
            "current: base\nitems:\n  - {uid: base, type: remote, name: fixture, file: base.yaml}\n",
        )
        .unwrap();
        std::fs::write(home.path().join("profiles/base.yaml"), PROFILE_B).unwrap();

        let formal = clash_verge_core::utils::dirs::singbox_config_path().unwrap();
        let restored = ManagerInner::write_singbox_assembled_to(home.path(), Some(PROFILE_A), false, &formal)
            .await
            .unwrap()
            .0;
        let restored_bytes = std::fs::read(&restored).unwrap();
        assert!(
            String::from_utf8_lossy(&restored_bytes).contains("a-recovery-marker.example"),
            "precondition: the restored config is A"
        );

        // A normal start regenerates from the active profile: that is what
        // the recovery must NOT do.
        let manager = MihomoManager::new(home.path().to_path_buf())
            .with_socket(home.path().join("controller.sock"))
            .with_singbox_controller("127.0.0.1:49715".parse().unwrap())
            .with_core_kind(CoreKind::SingBox);
        let regenerated = ManagerInner::runtime_config_for_start(
            &manager.inner(),
            home.path(),
            crate::commands::start::SupervisorLaunch::Regenerate,
        )
        .await
        .expect("a normal start generates");
        assert_eq!(regenerated, restored);
        let regenerated_bytes = std::fs::read(&restored).unwrap();
        assert!(
            String::from_utf8_lossy(&regenerated_bytes).contains("b-apply-marker.example"),
            "a normal start serves the active profile"
        );

        // Put the restored A back, then launch with the recovery mode,
        // exactly as `commands::daemon::run` does for a recovery supervisor.
        std::fs::write(&restored, &restored_bytes).unwrap();
        let recovery = ManagerInner::runtime_config_for_start(
            &manager.inner(),
            home.path(),
            crate::commands::start::SupervisorLaunch::UseExistingConfig,
        )
        .await
        .expect("a recovery start consumes the restored config");
        assert_eq!(recovery, restored);
        assert_eq!(
            std::fs::read(&restored).unwrap(),
            restored_bytes,
            "the recovery must not touch the restored config"
        );

        // Nothing to recover from is an error, never a silent regeneration.
        std::fs::remove_file(&restored).unwrap();
        let error = ManagerInner::runtime_config_for_start(
            &manager.inner(),
            home.path(),
            crate::commands::start::SupervisorLaunch::UseExistingConfig,
        )
        .await
        .expect_err("no restored config must not regenerate");
        assert!(error.to_string().contains("no restored"), "{error}");
    }
}
