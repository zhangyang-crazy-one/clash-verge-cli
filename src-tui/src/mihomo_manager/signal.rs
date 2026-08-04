// Foundation module — `graceful_stop` is wired up by Plan 02-03.

use std::time::Duration;

use anyhow::Result;
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use tokio::process::Command;

const GRACEFUL_TIMEOUT: Duration = Duration::from_secs(5);

/// D-10: graceful shutdown — send SIGTERM, wait up to 5 seconds, then
/// SIGKILL if the child ignored SIGTERM.
///
/// On timeout we fall back to `Child::kill()` (SIGKILL) and reap the
/// zombie. The PID is sent SIGTERM via `nix` so the same code path
/// works whether the child is a foreground process or a reparented
/// daemon.
pub async fn graceful_stop(child_pid: u32, child: &mut tokio::process::Child) -> Result<()> {
    let pid = Pid::from_raw(child_pid as i32);
    kill(pid, Signal::SIGTERM)?;

    match tokio::time::timeout(GRACEFUL_TIMEOUT, child.wait()).await {
        Ok(Ok(_status)) => Ok(()),
        Ok(Err(e)) => Err(anyhow::anyhow!("error waiting for mihomo after SIGTERM: {e}")),
        Err(_) => {
            tracing::warn!("mihomo did not exit in 5s, sending SIGKILL");
            child.kill().await?;
            child.wait().await?;
            Ok(())
        }
    }
}

/// Stop a process we own by PID when the `Child` handle was moved into the watcher.
///
/// Polls `kill(pid, None)` for liveness after SIGTERM, then escalates to SIGKILL.
pub async fn graceful_stop_by_pid(child_pid: u32) -> Result<()> {
    let pid = Pid::from_raw(child_pid as i32);
    match kill(pid, Signal::SIGTERM) {
        Ok(()) => {}
        Err(nix::errno::Errno::ESRCH) => return Ok(()),
        Err(error) => return Err(error.into()),
    }

    let deadline = tokio::time::Instant::now() + GRACEFUL_TIMEOUT;
    while tokio::time::Instant::now() < deadline {
        match kill(pid, None) {
            Err(nix::errno::Errno::ESRCH) => return Ok(()),
            Err(error) => return Err(error.into()),
            Ok(()) => tokio::time::sleep(Duration::from_millis(100)).await,
        }
    }

    tracing::warn!("mihomo pid {child_pid} did not exit in 5s, sending SIGKILL");
    match kill(pid, Signal::SIGKILL) {
        Ok(()) | Err(nix::errno::Errno::ESRCH) => {}
        Err(error) => return Err(error.into()),
    }

    // Reap until gone (or give up after a short window).
    let kill_deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while tokio::time::Instant::now() < kill_deadline {
        match kill(pid, None) {
            Err(nix::errno::Errno::ESRCH) => return Ok(()),
            Err(error) => {
                return Err(anyhow::anyhow!("error checking pid {child_pid} after SIGKILL: {error}"));
            }
            Ok(()) => tokio::time::sleep(Duration::from_millis(100)).await,
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::process::Stdio;

    fn skip_if_missing(name: &str) -> bool {
        // `which` shim — try invoking the binary with --help and check
        // exit code. If spawn fails outright the binary is missing.
        match std::process::Command::new(name)
            .arg("--help")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
        {
            Ok(_) => false,
            Err(_) => {
                eprintln!("skipping — `{name}` not available on PATH");
                true
            }
        }
    }

    #[tokio::test]
    async fn test_graceful_stop_sigterm_works() {
        if skip_if_missing("sleep") {
            return;
        }
        let mut child = Command::new("sleep")
            .arg("0.1")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn sleep");
        let pid = child.id().expect("child pid");
        graceful_stop(pid, &mut child).await.expect("graceful_stop");
    }

    #[tokio::test]
    async fn test_graceful_stop_sigkill_fallback() {
        if skip_if_missing("sh") {
            return;
        }
        // Trap SIGTERM so the shell ignores it; only SIGKILL will end it.
        // We loop in the same shell so the trap stays in effect (running
        // `sleep 30` directly would exec a fresh process without the trap).
        let mut child = Command::new("sh")
            .arg("-c")
            .arg("trap '' TERM; while :; do sleep 1; done")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn sh");
        let pid = child.id().expect("child pid");
        eprintln!("DEBUG: spawned pid={pid}");

        // Give the shell time to actually set the trap.
        tokio::time::sleep(Duration::from_millis(200)).await;
        let start = std::time::Instant::now();
        graceful_stop(pid, &mut child).await.expect("graceful_stop");
        let elapsed = start.elapsed();

        // 5s timeout + a bit of slack for the SIGKILL reap.
        assert!(
            elapsed < Duration::from_secs(7),
            "graceful_stop took {elapsed:?}, expected < 7s"
        );
        assert!(
            elapsed >= GRACEFUL_TIMEOUT,
            "graceful_stop returned in {elapsed:?} without waiting the full SIGTERM window"
        );
    }
}
