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
use tokio::sync::mpsc::UnboundedSender;

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
    pub action_tx: Mutex<Option<UnboundedSender<Action>>>,
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
            expected_exit: AtomicBool::new(false),
            owns_child: AtomicBool::new(false),
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
        let core_kind = inner.core_kind();
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
            pidfile::CoreRecord::with_kind(pid, started_at, core_kind),
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
        let probe_api = crate::mihomo_api::MihomoApi::with_transport(
            match core_kind {
                CoreKind::Mihomo => crate::mihomo_api::Transport::UnixSocket(socket_path.to_path_buf()),
                CoreKind::SingBox => crate::mihomo_api::Transport::Tcp(inner.singbox_controller()),
            },
            String::new(),
        )
        .map_err(|e| anyhow::anyhow!("readiness probe client build failed: {e}"))?;
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

        if let Some(tx) = inner.action_tx.lock().as_ref() {
            let _ = tx.send(Action::CoreStarted {
                version: Some(probed_version),
                binary_path: Some(resolved_path.display().to_string()),
                binary_source: Some(source.to_string()),
            });
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
    pub(crate) async fn active_profile_yaml() -> Option<String> {
        let store = crate::profile_store::store::ProfileStore::snapshot().await.ok()?;
        let uid = store.current_uid();
        let item = store.items().into_iter().find(|i| i.uid == uid)?;
        let file = item.file.as_deref()?;
        let path = clash_verge_core::utils::dirs::app_profiles_dir().ok()?.join(file);
        std::fs::read_to_string(path).ok()
    }

    /// Generate and persist the sing-box runtime config from whatever the
    /// active profile currently holds (task 7.5): converted outbounds/groups,
    /// route rules, stored logical rules, rule-sets and structured DNS.
    pub(crate) async fn write_singbox_full(config_dir: &Path) -> anyhow::Result<PathBuf> {
        let yaml = Self::active_profile_yaml().await;
        let enable_tun = runtime_tun_enabled().await.unwrap_or(false);
        Ok(Self::write_singbox_assembled(config_dir, yaml.as_deref(), enable_tun)
            .await?
            .0)
    }

    /// Persist a sing-box runtime config assembled from the given profile
    /// YAML (None → bare skeleton). Returns the written path plus the parts
    /// so callers can build a degradation report without re-converting.
    pub(crate) async fn write_singbox_assembled(
        config_dir: &Path,
        yaml: Option<&str>,
        enable_tun: bool,
    ) -> anyhow::Result<(PathBuf, SingboxParts)> {
        let _ = config_dir;
        // Honour the CLI's own `verge_mixed_port` override so sing-box binds the
        // same non-conflicting port as the mihomo runtime config.
        let mixed_port = crate::enhance::effective_mixed_port().await;
        let tun = crate::singbox::TunSettings {
            stack: "gvisor".into(),
            mtu: 9000,
        };
        let clash_api = crate::singbox::ClashApiSettings {
            listen: "127.0.0.1:9090".parse().expect("static addr"),
            secret: String::new(),
        };

        // Native sing-box JSON profile passthrough: preserve the provider's own
        // outbounds, route, and dns, and only enforce the CLI-owned control plane
        // (inbounds + clash_api + log). Avoids lossy conversion through the Clash model.
        if let Some(text) = yaml
            && crate::subscribe::from_url::is_singbox_json_profile(text)
        {
            let mut config: serde_json::Value =
                serde_json::from_str(text).context("failed to parse native sing-box JSON profile")?;
            crate::singbox::config_gen::apply_control_plane(&mut config, mixed_port, enable_tun, &tun, &clash_api);
            let path = clash_verge_core::utils::dirs::singbox_config_path()?;
            if let Some(parent) = path.parent() {
                tokio::fs::create_dir_all(parent).await.ok();
            }
            let body = serde_json::to_string_pretty(&config)?;
            let tmp = path.with_extension("json.download");
            tokio::fs::write(&tmp, body).await?;
            tokio::fs::rename(&tmp, &path).await?;

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
        let path = clash_verge_core::utils::dirs::singbox_config_path()?;
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await.ok();
        }
        let body = serde_json::to_string_pretty(&config)?;
        // Write-then-rename so a crash mid-write never leaves a truncated config.
        let tmp = path.with_extension("json.download");
        tokio::fs::write(&tmp, body).await?;
        tokio::fs::rename(&tmp, &path).await?;
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
        // Clash-expressible rules convert through IRouteRule; raw clash
        // fragments have no sing-box form and stay profile-only. Stored
        // logical rules are appended after them (see LOGICAL_RULES_FILE).
        let mut route_rules: Vec<serde_json::Value> = match yaml {
            Some(y) => crate::routing::load_profile_rules(y)
                .map_err(anyhow::Error::msg)?
                .iter()
                .filter_map(crate::routing::to_singbox_json)
                .collect(),
            None => Vec::new(),
        };
        let home = clash_verge_core::utils::dirs::app_home_dir().ok();
        if let Some(logical) = home.as_ref().map(|home| crate::singbox::load_logical_rules(home)) {
            route_rules.extend(logical.iter().filter_map(crate::routing::to_singbox_json));
        }
        let rule_sets = home
            .as_ref()
            .map(|home| crate::singbox::load_rule_sets(home))
            .unwrap_or_default();
        let dns_spec = home.as_deref().and_then(crate::singbox::load_dns_spec);
        let default_domain_resolver = dns_spec.as_ref().and_then(crate::singbox::dns::default_domain_resolver);
        let empty_dns = crate::singbox::dns::DnsConfigSpec::default();
        let dns_section = crate::singbox::dns::build_dns_section(dns_spec.as_ref().unwrap_or(&empty_dns))
            .map_err(anyhow::Error::msg)?;
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
            return self.start_singbox().await;
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
    async fn start_singbox(&self) -> anyhow::Result<binary::ResolvedMihomo> {
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
            || Self::resolve_and_preflight_singbox(),
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
                    let config_path = ManagerInner::write_singbox_full(&config_dir).await?;
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

    async fn restart_singbox(&self) -> anyhow::Result<binary::ResolvedMihomo> {
        use super::singbox_binary::SingboxBinarySource;
        self.reset_restart_history();
        let resolved = super::singbox_binary::resolve_or_install()
            .await
            .context("failed to resolve or auto-install sing-box core")?;
        let tun_enabled = runtime_tun_enabled().await.unwrap_or(false);
        preflight_tun_capability(&resolved.path, tun_enabled)?;
        self.stop().await.context("failed to stop running sing-box")?;
        crate::enhance::ensure_mixed_port_available()
            .await
            .map_err(anyhow::Error::msg)?;
        let config_path = ManagerInner::write_singbox_full(&self.config_dir).await?;
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
