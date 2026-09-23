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
    let api = manager.api();
    if super::core_running(&api).await {
        anyhow::bail!(
            "a mihomo core already answers on {} without a clash-verge-cli pid record; stop it where it was started",
            manager.socket_path().display()
        );
    }

    let log = supervisor_log_path(manager.config_dir());
    let mut supervisor = launch_supervisor(manager.config_dir(), &log)?;
    wait_until_ready(&manager, &mut supervisor, &log).await?;

    let version = api.version().await.map(|v| v.version).unwrap_or_else(|_| "?".into());
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

/// Where the detached supervisor writes its own and the core's output.
pub fn supervisor_log_path(config_dir: &Path) -> PathBuf {
    config_dir.join("logs").join("daemon.log")
}

/// Start `clash-verge-cli --config-dir <dir> start --foreground` detached
/// from this process: its own process group (a terminal Ctrl-C aimed at the
/// CLI does not reach it), stdin closed, output appended to `log` (the
/// previous run is kept as `daemon.log.old`).
fn launch_supervisor(config_dir: &Path, log: &Path) -> anyhow::Result<std::process::Child> {
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
    std::process::Command::new(exe)
        .arg("--config-dir")
        .arg(config_dir)
        .args(["start", "--foreground"])
        .stdin(std::process::Stdio::null())
        .stdout(out)
        .stderr(err)
        .process_group(0)
        .spawn()
        .context("failed to launch the core supervisor")
}

/// Wait until the supervised core answers. If the supervisor exits first
/// (bad config, missing TUN capability, crash loop), report its log tail.
async fn wait_until_ready(
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
