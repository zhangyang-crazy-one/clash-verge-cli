//! `clash-verge-cli start`: run the core in the background under a
//! supervisor.
//!
//! The core's output is piped to whichever process spawned it, so that
//! process must outlive the core: a pipe to an exited process kills mihomo
//! with SIGPIPE. `start` therefore launches a detached
//! `clash-verge-cli start --foreground` (the same supervisor systemd runs).
//! The supervisor spawns mihomo, restarts it after a crash, releases the
//! system proxy when it stops for good, and runs the subscription
//! auto-update. `start` itself only waits until the controller answers.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context;

use crate::mihomo_manager::manager::MihomoManager;
use crate::mihomo_manager::pidfile;

/// How long `start`/`restart` wait for the controller to answer.
pub const READY_TIMEOUT: Duration = Duration::from_secs(15);

pub async fn run(manager: MihomoManager) -> anyhow::Result<()> {
    if let Some(pid) = manager.pid() {
        anyhow::bail!("mihomo is already running (pid {pid}); use `restart` to replace it");
    }
    // P1 (reviewer): complete the credential rotation in the PARENT before
    // anything is launched. The supervisor child resolves the secret again at
    // spawn time (a no-op once it is strong), so doing it here is safe — but
    // without it the parent keeps the pre-rotation snapshot from
    // config.yaml and authenticates with `set-your-secret`. mihomo's unix
    // transport ignores the bearer, but sing-box's clash_api is TCP and
    // really enforces it: a fresh install would report a 15s start timeout
    // while the core was healthy.
    prepare_controller_credentials(&manager).await?;
    let api = manager.api();
    if super::core_running(&api).await {
        anyhow::bail!(
            "a mihomo core already answers on {} without a clash-verge-cli pid record; stop it where it was started",
            manager.socket_path().display()
        );
    }

    let log = supervisor_log_path(manager.config_dir());
    let mut supervisor = launch_supervisor_via(manager.config_dir(), &log, SupervisorLaunch::Regenerate)?;
    wait_until_ready(&manager, &mut supervisor, &log).await?;

    let version = manager
        .api()
        .version()
        .await
        .map(|v| v.version)
        .unwrap_or_else(|_| "?".into());
    let core = pidfile::read_live(&pidfile::path_for(manager.socket_path()), manager.socket_path());
    println!("mihomo started");
    println!("  version:    {version}");
    if let Some(core) = core {
        println!("  pid:        {}", core.pid);
    }
    println!("  supervisor: pid {}", supervisor.id());
    println!("  log:        {}", log.display());
    Ok(())
}

/// What a supervisor-launched core must do about the runtime config on disk.
///
/// P1 (reviewer): the rollback recovery of a failed apply has to bring the
/// OLD service back, not re-apply the newer profile. The restored config
/// (A) is already on disk, but the supervisor's normal start regenerates it
/// from whatever the active profile now holds (B) — so the recovered core
/// served the config the user was trying to get away from, and the restored
/// file was overwritten a moment later.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SupervisorLaunch {
    /// Normal cold start: generate the runtime config from the active profile.
    Regenerate,
    /// Recovery: bring the core up on the config already on disk, verbatim.
    UseExistingConfig,
}

/// Environment variable carrying [`SupervisorLaunch`] to the detached child.
///
/// The supervisor is launched as `start --foreground`, whose argument parser
/// lives outside this module; an environment variable is what lets the
/// recovery request travel to the child without a new CLI flag.
pub const START_CONFIG_MODE_ENV: &str = "CLASH_VERGE_CLI_START_CONFIG";

/// The launch mode this process was started with (default [`SupervisorLaunch::Regenerate`]).
pub fn requested_launch() -> SupervisorLaunch {
    match std::env::var(START_CONFIG_MODE_ENV).as_deref() {
        Ok("existing") => SupervisorLaunch::UseExistingConfig,
        _ => SupervisorLaunch::Regenerate,
    }
}

/// Where the detached supervisor writes its own and the core's output.
pub fn supervisor_log_path(config_dir: &Path) -> PathBuf {
    config_dir.join("logs").join("daemon.log")
}

/// Resolve (and, when needed, rotate + persist) the shared controller
/// secret, then publish it to `manager` so every controller call this
/// process makes — including the readiness probe below — authenticates with
/// the value the core was actually started with.
///
/// Fail-closed: a malformed `config.yaml`, or a rotation that cannot be
/// persisted, aborts the start here, before any process is launched.
pub async fn prepare_controller_credentials(manager: &MihomoManager) -> anyhow::Result<()> {
    let secret = crate::enhance::resolve_controller_secret()
        .await
        .context("failed to prepare the controller secret in the clash config; no core was started")?;
    manager.set_secret_override(secret);
    Ok(())
}

/// Start `clash-verge-cli --config-dir <dir> start --foreground` detached
/// from this process: its own process group (a terminal Ctrl-C aimed at the
/// CLI does not reach it), stdin closed, output appended to `log` (the
/// previous run is kept as `daemon.log.old`).
///
/// Also used by [`crate::mihomo_manager::MihomoManager::restart_through_supervisor`]
/// to hand a replacement core to a supervisor after stopping an adopted one.
pub fn launch_supervisor(config_dir: &Path, log: &Path, mode: SupervisorLaunch) -> anyhow::Result<std::process::Child> {
    use std::os::unix::process::CommandExt as _;

    if let Some(dir) = log.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("failed to create {}", dir.display()))?;
    }
    if log.exists() {
        let _ = std::fs::rename(log, log.with_extension("log.old"));
    }
    let out = std::fs::File::create(log).with_context(|| format!("failed to create {}", log.display()))?;
    let err = out.try_clone().context("failed to duplicate the log handle")?;
    let exe = std::env::current_exe().context("cannot locate the clash-verge-cli executable")?;
    let mut command = std::process::Command::new(exe);
    command
        .arg("--config-dir")
        .arg(config_dir)
        .args(["start", "--foreground"])
        .stdin(std::process::Stdio::null())
        .stdout(out)
        .stderr(err)
        .process_group(0);
    if mode == SupervisorLaunch::UseExistingConfig {
        command.env(START_CONFIG_MODE_ENV, "existing");
    }
    command.spawn().context("failed to launch the core supervisor")
}

/// Launch the detached supervisor, or the test-installed stand-in.
///
/// Every detached start goes through here (the plain `start`, the adopted
/// core replacement and the apply transaction's rollback recovery), so a
/// test can inject a *failing* supervisor launch at the real process
/// boundary without spawning a core — and can observe the launch mode the
/// production path would have put on the child's command line.
pub(crate) fn launch_supervisor_via(
    config_dir: &Path,
    log: &Path,
    mode: SupervisorLaunch,
) -> anyhow::Result<std::process::Child> {
    #[cfg(test)]
    if let Some(launcher) = *SUPERVISOR_LAUNCHER
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
    {
        return launcher(config_dir, log, mode);
    }
    launch_supervisor(config_dir, log, mode)
}

/// Test seam: the launcher [`launch_supervisor_via`] prefers while armed.
#[cfg(test)]
pub(crate) type SupervisorLauncher = fn(&Path, &Path, SupervisorLaunch) -> anyhow::Result<std::process::Child>;

#[cfg(test)]
static SUPERVISOR_LAUNCHER: std::sync::Mutex<Option<SupervisorLauncher>> = std::sync::Mutex::new(None);

/// Arm the seam for one test and restore the real launcher afterwards.
#[cfg(test)]
pub(crate) struct SupervisorLauncherGuard;

#[cfg(test)]
impl Drop for SupervisorLauncherGuard {
    fn drop(&mut self) {
        *SUPERVISOR_LAUNCHER
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
    }
}

#[cfg(test)]
pub(crate) fn install_supervisor_launcher(launcher: SupervisorLauncher) -> anyhow::Result<SupervisorLauncherGuard> {
    *SUPERVISOR_LAUNCHER
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(launcher);
    Ok(SupervisorLauncherGuard)
}

/// Terminate a supervisor that never became ready and reap it.
///
/// P1 (reviewer): a readiness timeout can happen AFTER the supervisor has
/// spawned its core (a core that answers too slowly, a wedged one). Leaving
/// that supervisor alive would leave it watching a core that competes with
/// the recovery attempt's replacement for the same controller socket, so the
/// timeout path has to kill the supervisor and collect it instead of dropping
/// the [`std::process::Child`] (which never reaps anything) on the floor.
pub fn reap_supervisor(supervisor: &mut std::process::Child) {
    if matches!(supervisor.try_wait(), Ok(None)) {
        let _ = supervisor.kill();
    }
    // `wait` after `kill` collects the zombie; a supervisor that already
    // exited is reaped by it too.
    let _ = supervisor.wait();
}

/// Wait until the supervised core answers. If the supervisor exits first
/// (bad config, missing TUN capability, crash loop), report its log tail.
pub async fn wait_until_ready(
    manager: &MihomoManager,
    supervisor: &mut std::process::Child,
    log: &Path,
) -> anyhow::Result<()> {
    let api = manager.api();
    let deadline = tokio::time::Instant::now() + READY_TIMEOUT;
    loop {
        if super::core_running(&api).await {
            return Ok(());
        }
        if let Some(status) = supervisor.try_wait()? {
            anyhow::bail!("mihomo failed to start ({status})\n{}", super::log_tail(log, 10));
        }
        if tokio::time::Instant::now() >= deadline {
            anyhow::bail!(
                "mihomo did not answer on {} within {}s\n{}",
                manager.socket_path().display(),
                READY_TIMEOUT.as_secs(),
                super::log_tail(log, 10)
            );
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    /// Armed by the stand-in supervisor: a fake core must not answer before
    /// one was launched, or `run`'s "a core already answers" guard would
    /// trip.
    static CORE_ARMED: AtomicBool = AtomicBool::new(false);
    /// Requests the fake controller accepted with the persisted secret.
    static AUTHORIZED: AtomicUsize = AtomicUsize::new(0);
    /// Requests rejected with 401 — must stay 0 for the parent to start.
    static REJECTED: AtomicUsize = AtomicUsize::new(0);
    /// The `config.yaml` the stand-in supervisor child re-resolves, and the
    /// secret it writes when it finds a placeholder (i.e. what the real
    /// supervisor's `spawn_core` rotation does — in ITS OWN process).
    static CHILD_VIEW: std::sync::OnceLock<(PathBuf, String)> = std::sync::OnceLock::new();
    /// Pids of the stand-in supervisor processes, reaped at test end.
    static STANDIN_PIDS: std::sync::Mutex<Vec<u32>> = std::sync::Mutex::new(Vec::new());

    /// A fake sing-box clash_api: a real TCP listener on loopback that
    /// enforces the bearer secret exactly like the real controller, so the
    /// parent's credentials are checked over the wire, not by inspection.
    async fn fake_controller(addr: std::net::SocketAddr, config: PathBuf) {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let listener = tokio::net::TcpListener::bind(addr).await.expect("bind fake controller");
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let config = config.clone();
            tokio::spawn(async move {
                let mut buffer = [0u8; 4096];
                let Ok(count) = stream.read(&mut buffer).await else {
                    return;
                };
                let request = String::from_utf8_lossy(&buffer[..count]).to_string();
                let presented = request
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("authorization:")
                            .map(|value| value.trim().trim_start_matches("bearer ").trim().to_string())
                    })
                    .unwrap_or_default();
                let persisted = std::fs::read_to_string(&config)
                    .ok()
                    .and_then(|body| {
                        body.lines().find_map(|line| {
                            line.strip_prefix("secret:")
                                .map(|value| value.trim().trim_matches('"').to_string())
                        })
                    })
                    .unwrap_or_default();
                let (status, body) =
                    if CORE_ARMED.load(Ordering::SeqCst) && presented == persisted && !persisted.is_empty() {
                        AUTHORIZED.fetch_add(1, Ordering::SeqCst);
                        ("200 OK", r#"{"version":"1.19.0"}"#)
                    } else {
                        REJECTED.fetch_add(1, Ordering::SeqCst);
                        ("401 Unauthorized", r#"{"message":"Unauthorized"}"#)
                    };
                let _ = stream
                    .write_all(
                        format!(
                            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        )
                        .as_bytes(),
                    )
                    .await;
            });
        }
    }

    /// Stands in for the detached `start --foreground` supervisor: a real
    /// child process that first performs the credential preparation the
    /// supervisor does at spawn time — in its own process, so the parent's
    /// process-global caches stay untouched, exactly as in production.
    fn standin_supervisor(
        config_dir: &Path,
        log: &Path,
        mode: SupervisorLaunch,
    ) -> anyhow::Result<std::process::Child> {
        let (config, child_secret) = CHILD_VIEW.get_or_init(|| {
            let config = clash_verge_core::utils::dirs::clash_path().expect("clash path");
            let config = config.clone();
            (config, "child-rotated-secret".to_string())
        });
        if let Some(dir) = log.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(log, b"stand-in supervisor\n")?;
        let status = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(format!(
                r#"if grep -q '^secret:.*set-your-secret' "$1"; then sed -i 's|^secret:.*|secret: {secret}|' "$1"; fi"#,
                secret = child_secret
            ))
            .arg("supervisor")
            .arg(config)
            .status()?;
        if !status.success() {
            anyhow::bail!("the supervisor child failed to prepare its credentials: {status}");
        }
        // The core is "up" from here on; the parent must find it with the
        // secret now persisted on disk.
        CORE_ARMED.store(true, Ordering::SeqCst);
        let child = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg("sleep 120")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()?;
        STANDIN_PIDS.lock().unwrap().push(child.id());
        let _ = config_dir;
        let _ = mode;
        Ok(child)
    }

    fn reap_standins() {
        for pid in STANDIN_PIDS.lock().unwrap().drain(..) {
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(pid as i32),
                nix::sys::signal::Signal::SIGKILL,
            );
        }
    }

    /// P1 (reviewer): a detached start must not inherit the PRE-rotation
    /// secret. The supervisor child rotates `config.yaml` at spawn, so a
    /// parent that never resolved the secret itself polls an authenticated
    /// sing-box controller with `set-your-secret` and reports a 15s start
    /// timeout while the core is healthy.
    #[tokio::test]
    async fn a_detached_start_authenticates_with_the_secret_the_child_persisted() {
        let home = tempfile::tempdir().expect("tempdir");
        let _home_guard = crate::profile_store::store::tests::claim_test_app_home(home.path().to_path_buf()).await;
        let config = home.path().join("config.yaml");
        std::fs::write(
            &config,
            "mixed-port: 39181\nport: 0\nsocks-port: 0\nredir-port: 0\ntproxy-port: 0\nsecret: set-your-secret\nexternal-controller: 127.0.0.1:19097\n",
        )
        .expect("seed config");
        CORE_ARMED.store(false, Ordering::SeqCst);
        AUTHORIZED.store(0, Ordering::SeqCst);
        REJECTED.store(0, Ordering::SeqCst);

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("reserve port");
        let addr = listener.local_addr().expect("addr");
        drop(listener);
        tokio::spawn(fake_controller(addr, config.clone()));

        let config_dir = home.path().join("run");
        std::fs::create_dir_all(&config_dir).expect("config dir");
        // Exactly what `build_manager_for_kind` produces from the
        // pre-rotation file: the placeholder snapshot.
        let manager = MihomoManager::new(config_dir)
            .with_socket(home.path().join("controller.sock"))
            .with_singbox_controller(addr)
            .with_core_kind(crate::mihomo_manager::CoreKind::SingBox)
            .with_secret(crate::enhance::PLACEHOLDER_CONTROLLER_SECRET.to_string());
        let _launcher = install_supervisor_launcher(standin_supervisor).expect("install seam");

        let started = tokio::time::timeout(Duration::from_secs(10), run(manager))
            .await
            .expect("a healthy core must not hit the 15s readiness timeout");
        assert!(started.is_ok(), "{started:?}");
        reap_standins();

        assert!(
            AUTHORIZED.load(Ordering::SeqCst) >= 2,
            "the parent must reach the controller with the persisted secret"
        );
        let persisted = std::fs::read_to_string(&config).expect("reread config");
        let secret = persisted
            .lines()
            .find_map(|line| line.strip_prefix("secret:").map(|value| value.trim().to_string()))
            .expect("secret line");
        assert_ne!(secret, crate::enhance::PLACEHOLDER_CONTROLLER_SECRET);
        assert!(
            persisted.contains("port: 0") && persisted.contains("socks-port: 0"),
            "the parent-side rotation must not re-enable disabled listeners: {persisted}"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&config).expect("stat").permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "the rotated secret must stay owner-only: {mode:o}");
        }
    }

    /// The child's own resolve is an idempotent no-op once the parent has
    /// rotated: the value on disk (and the one the controller accepts) does
    /// not change between two resolutions.
    #[tokio::test]
    async fn resolving_twice_keeps_the_first_persisted_secret() {
        let home = tempfile::tempdir().expect("tempdir");
        let _home_guard = crate::profile_store::store::tests::claim_test_app_home(home.path().to_path_buf()).await;
        let config = home.path().join("config.yaml");
        std::fs::write(&config, "mixed-port: 39182\nsecret: set-your-secret\n").expect("seed");

        let first = prepare_controller_credentials_for_test().await;
        assert!(!crate::enhance::is_placeholder_secret(&first));
        let body = std::fs::read_to_string(&config).expect("reread");
        assert_eq!(
            crate::enhance::resolve_controller_secret()
                .await
                .expect("child resolve"),
            first,
            "the supervisor child must adopt the parent's secret, not mint another one"
        );
        assert_eq!(std::fs::read_to_string(&config).expect("reread"), body);
    }

    async fn prepare_controller_credentials_for_test() -> String {
        crate::enhance::resolve_controller_secret().await.expect("resolve")
    }
}
