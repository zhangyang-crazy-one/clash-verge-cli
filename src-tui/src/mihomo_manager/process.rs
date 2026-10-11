//! Linux process identities and per-launch receipts. Shared pidfiles are not
//! authority to clean up a failed launch, particularly after reparenting.

use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd};
use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::{Deserialize, Serialize};

use super::pidfile::CoreKind;

pub(crate) const RECEIPT_PATH_ENV: &str = "CLASH_VERGE_CLI_LAUNCH_RECEIPT";
pub(crate) const LAUNCH_ID_ENV: &str = "CLASH_VERGE_CLI_LAUNCH_ID";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ProcessIdentity {
    pub pid: u32,
    start_ticks: u64,
    exe: PathBuf,
    cmdline: Vec<u8>,
}

impl ProcessIdentity {
    pub(crate) fn capture(pid: u32) -> anyhow::Result<Self> {
        let root = PathBuf::from(format!("/proc/{pid}"));
        let start_ticks = start_ticks(&std::fs::read_to_string(root.join("stat"))?)
            .ok_or_else(|| anyhow::anyhow!("cannot read starttime of pid {pid}"))?;
        let identity = Self {
            pid,
            start_ticks,
            exe: std::fs::read_link(root.join("exe"))?,
            cmdline: std::fs::read(root.join("cmdline"))?,
        };
        // Detect an exit/reuse while reading the separate proc files.
        anyhow::ensure!(
            start_ticks == self::start_ticks(&std::fs::read_to_string(root.join("stat"))?).unwrap_or(0),
            "pid {pid} changed identity during capture"
        );
        Ok(identity)
    }

    pub(crate) fn matches_live(&self) -> bool {
        super::pidfile::is_running(self.pid) && Self::capture(self.pid).is_ok_and(|live| live == *self)
    }

    pub(crate) fn matches_process(&self) -> bool {
        super::pidfile::is_running(self.pid)
            && std::fs::read_to_string(format!("/proc/{}/stat", self.pid))
                .ok()
                .and_then(|stat| start_ticks(&stat))
                == Some(self.start_ticks)
    }

    pub(crate) fn bind(&self) -> anyhow::Result<OwnedProcess> {
        let process = OwnedProcess::capture(self.pid)?;
        anyhow::ensure!(
            process.start_ticks == self.start_ticks && self.matches_live(),
            "receipt process identity changed before binding"
        );
        Ok(process)
    }

    /// Open a pidfd before the final identity check. Both TERM and KILL use
    /// this handle, so PID reuse between checks cannot redirect a signal.
    pub(crate) async fn stop(&self) -> anyhow::Result<()> {
        if !self.matches_live() {
            return Ok(());
        }
        self.bind()?.stop_when(|| self.matches_live()).await
    }
}

/// Authority over one kernel process, not over a reusable numeric PID or
/// an executable name. Kept open from ancestry verification through cleanup.
/// An owned child may exec between those steps without becoming foreign.
pub(crate) struct OwnedProcess {
    pub pid: u32,
    start_ticks: u64,
    pidfd: OwnedFd,
}

impl OwnedProcess {
    pub(crate) fn capture(pid: u32) -> anyhow::Result<Self> {
        let ticks = start_ticks(&std::fs::read_to_string(format!("/proc/{pid}/stat"))?)
            .ok_or_else(|| anyhow::anyhow!("cannot read starttime of pid {pid}"))?;
        // SAFETY: pidfd_open has no pointer arguments and returns an owned fd.
        let raw = unsafe { nix::libc::syscall(nix::libc::SYS_pidfd_open, pid, 0) } as i32;
        if raw < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        // SAFETY: raw is a fresh successful pidfd_open result.
        let process = Self {
            pid,
            start_ticks: ticks,
            pidfd: unsafe { OwnedFd::from_raw_fd(raw) },
        };
        anyhow::ensure!(process.matches_process(), "pid {pid} changed identity before binding");
        Ok(process)
    }

    pub(crate) fn capture_child(pid: u32, parent: &Self) -> anyhow::Result<Self> {
        let process = Self::capture(pid)?;
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"))?;
        let ppid = stat
            .rsplit_once(')')
            .and_then(|(_, rest)| rest.split_whitespace().nth(1))
            .and_then(|token| token.parse::<u32>().ok());
        anyhow::ensure!(
            ppid == Some(parent.pid) && parent.matches_process() && process.matches_process(),
            "pid {pid} is no longer a child of the bound parent"
        );
        Ok(process)
    }

    pub(crate) fn matches_process(&self) -> bool {
        super::pidfile::is_running(self.pid)
            && std::fs::read_to_string(format!("/proc/{}/stat", self.pid))
                .ok()
                .and_then(|stat| start_ticks(&stat))
                == Some(self.start_ticks)
    }

    fn signal(&self, signal: i32) -> anyhow::Result<()> {
        // SAFETY: the fd remains owned/live, siginfo is null, and flags is 0.
        let result = unsafe {
            nix::libc::syscall(
                nix::libc::SYS_pidfd_send_signal,
                self.pidfd.as_raw_fd(),
                signal,
                std::ptr::null::<nix::libc::siginfo_t>(),
                0,
            )
        };
        if result < 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(nix::libc::ESRCH) {
                return Err(error.into());
            }
        }
        Ok(())
    }

    /// Stop a bound parent before enumerating its children, including each
    /// nested parent, so none can fork outside the cleanup snapshot.
    pub(crate) async fn freeze(&self) -> anyhow::Result<()> {
        if !self.matches_process() {
            return Ok(());
        }
        self.signal(nix::libc::SIGSTOP)?;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(1);
        while self.matches_process() {
            let stopped = std::fs::read_to_string(format!("/proc/{}/stat", self.pid))
                .ok()
                .and_then(|stat| {
                    stat.rsplit_once(')')
                        .map(|(_, rest)| rest.trim_start().starts_with(['T', 't']))
                })
                .unwrap_or(false);
            if stopped {
                return Ok(());
            }
            anyhow::ensure!(
                tokio::time::Instant::now() < deadline,
                "process {} did not freeze for cleanup",
                self.pid
            );
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        Ok(())
    }

    pub(crate) async fn stop(&self) -> anyhow::Result<()> {
        self.stop_when(|| true).await
    }

    async fn stop_when(&self, allowed: impl Fn() -> bool) -> anyhow::Result<()> {
        for signal in [nix::libc::SIGTERM, nix::libc::SIGKILL] {
            if !self.matches_process() || !allowed() {
                return Ok(());
            }
            self.signal(signal)?;
            if signal == nix::libc::SIGTERM {
                // TERM is pending on a frozen process until it resumes.
                self.signal(nix::libc::SIGCONT)?;
            }
            let timeout = if signal == nix::libc::SIGTERM { 5 } else { 2 };
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(timeout);
            while self.matches_process() && allowed() && tokio::time::Instant::now() < deadline {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        }
        anyhow::ensure!(
            !self.matches_process() || !allowed(),
            "pid {} did not exit after SIGKILL",
            self.pid
        );
        Ok(())
    }
}

fn start_ticks(stat: &str) -> Option<u64> {
    // comm can contain spaces and ')'; field 22 is index 19 after comm.
    stat.rsplit_once(')')?.1.split_whitespace().nth(19)?.parse().ok()
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct LaunchReceipt {
    launch_id: String,
    pub identity: ProcessIdentity,
    kind: CoreKind,
    config: PathBuf,
}

pub(crate) struct ReceiptTarget {
    // Owner-only directory, removed after success/cleanup. No shared path
    // can overwrite this launch's record or survive into a later launch.
    _directory: tempfile::TempDir,
    pub path: PathBuf,
    pub launch_id: String,
    kind: CoreKind,
    config: PathBuf,
}

impl ReceiptTarget {
    pub(crate) fn new(config_dir: &Path, kind: CoreKind) -> anyhow::Result<Self> {
        std::fs::create_dir_all(config_dir)?;
        let directory = tempfile::Builder::new()
            .prefix(".core-launch-")
            .tempdir_in(config_dir)?;
        let config = match kind {
            CoreKind::Mihomo => clash_verge_core::utils::dirs::clash_path()?,
            CoreKind::SingBox => clash_verge_core::utils::dirs::singbox_config_path()?,
        };
        Ok(Self {
            path: directory.path().join("core.json"),
            _directory: directory,
            launch_id: uuid::Uuid::new_v4().to_string(),
            kind,
            config,
        })
    }

    pub(crate) fn identity(&self) -> Option<ProcessIdentity> {
        let receipt: LaunchReceipt = serde_json::from_slice(&std::fs::read(&self.path).ok()?).ok()?;
        (receipt.launch_id == self.launch_id
            && receipt.kind == self.kind
            && receipt.config == self.config
            && receipt
                .identity
                .cmdline
                .split(|b| *b == 0)
                .any(|arg| arg == self.config.as_os_str().as_encoded_bytes())
            && receipt.identity.matches_live())
        .then_some(receipt.identity)
    }
}

/// Called synchronously immediately after spawning, before readiness. A
/// supervisor can subsequently exit; its child's immutable identity remains.
pub(crate) fn record_launch(pid: u32, kind: CoreKind, config: &Path) -> anyhow::Result<()> {
    let (Some(path), Some(launch_id)) = (std::env::var_os(RECEIPT_PATH_ENV), std::env::var_os(LAUNCH_ID_ENV)) else {
        return Ok(());
    };
    let path = PathBuf::from(path);
    // Parent finished readiness and dropped its directory: subsequent crash
    // recovery belongs to the supervisor, and needs no caller receipt.
    if !path.parent().is_some_and(Path::exists) {
        return Ok(());
    }
    let receipt = LaunchReceipt {
        launch_id: launch_id.to_string_lossy().into_owned(),
        identity: ProcessIdentity::capture(pid)?,
        kind,
        config: config.to_path_buf(),
    };
    let mut file = tempfile::NamedTempFile::new_in(path.parent().context("receipt has no parent")?)?;
    std::io::Write::write_all(&mut file, &serde_json::to_vec(&receipt)?)?;
    file.persist(&path).map_err(|error| error.error)?;
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct SystemdUnit {
    pub name: String,
    pub user: bool,
}

impl SystemdUnit {
    pub(crate) fn restart_command(&self) -> String {
        format!(
            "systemctl {}restart {}",
            if self.user { "--user " } else { "" },
            self.name
        )
    }
}

pub(crate) fn systemd_unit(cgroup: &str) -> Option<SystemdUnit> {
    cgroup.lines().find_map(|line| {
        let path = line.splitn(3, ':').nth(2)?;
        // user@UID.service is the user manager, not the managed application.
        let name = path
            .split('/')
            .rev()
            .find(|component| component.ends_with(".service") && !component.starts_with("user@"))?;
        // A session/scope underneath a service is not that service's daemon.
        let tail = path.rsplit_once(name)?.1;
        if tail.split('/').any(|component| component.ends_with(".scope")) {
            return None;
        }
        Some(SystemdUnit {
            name: name.to_string(),
            user: path.contains("/user.slice/") && path.contains("/user@"),
        })
    })
}

pub(crate) fn refuse_external_systemd_apply(owns_child: bool, cgroup: &str) -> anyhow::Result<()> {
    if !owns_child && let Some(unit) = systemd_unit(cgroup) {
        anyhow::bail!(
            "core is managed by systemd unit {}; external replacement would detach it from the service. Use `{}` to restart within its unit",
            unit.name,
            unit.restart_command()
        );
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn ancestry_bound_pidfd_survives_exec_without_weakening_receipt_checks() {
        use std::io::{BufRead as _, Write as _};
        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "printf 'ready\\n'; read proceed; exec sleep 60"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let mut ready = String::new();
        std::io::BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut ready)
            .unwrap();
        assert_eq!(ready, "ready\n");
        let before_exec = ProcessIdentity::capture(child.id()).unwrap();
        let parent = OwnedProcess::capture(std::process::id()).unwrap();
        let bound = OwnedProcess::capture_child(child.id(), &parent).unwrap();
        assert!(
            OwnedProcess::capture_child(child.id(), &bound).is_err(),
            "a pidfd alone cannot invent ancestry"
        );
        writeln!(child.stdin.take().unwrap(), "continue").unwrap();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        while !std::fs::read_link(format!("/proc/{}/exe", child.id())).is_ok_and(|exe| exe != before_exec.exe) {
            assert!(tokio::time::Instant::now() < deadline, "controlled exec never happened");
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(
            bound.matches_process(),
            "exec retains PID/starttime and the bound kernel process"
        );
        let after_exec = ProcessIdentity::capture(child.id()).unwrap();
        assert_eq!(before_exec.start_ticks, after_exec.start_ticks);
        assert_ne!(
            before_exec.exe, after_exec.exe,
            "counterexample requires a real executable transition"
        );
        assert!(
            before_exec.bind().is_err(),
            "a receipt for the old executable remains strict"
        );
        // Deterministic counterexample for the previous cleanup: shape-based
        // stop silently skips this still-owned process after a legal exec.
        before_exec.stop().await.unwrap();
        assert!(child.try_wait().unwrap().is_none());
        let mut wrong_generation = OwnedProcess::capture(child.id()).unwrap();
        wrong_generation.start_ticks += 1;
        wrong_generation.stop().await.unwrap();
        assert!(
            child.try_wait().unwrap().is_none(),
            "starttime mismatch must never be followed"
        );
        bound.stop().await.unwrap();
        child.wait().unwrap();
    }

    #[test]
    fn system_and_user_units_refuse_external_apply_but_allow_daemon_refresh() {
        for (group, command) in [
            (
                "0::/system.slice/clash-verge-cli.service\n",
                "systemctl restart clash-verge-cli.service",
            ),
            (
                "0::/user.slice/user-1000.slice/user@1000.service/app.slice/clash-verge-cli.service\n",
                "systemctl --user restart clash-verge-cli.service",
            ),
        ] {
            let error = refuse_external_systemd_apply(false, group).unwrap_err().to_string();
            assert!(error.contains(command), "{error}");
            refuse_external_systemd_apply(true, group).expect("owned daemon refresh remains inside its unit");
        }
        assert!(systemd_unit("0::/user.slice/user-1000.slice/user@1000.service/app.slice/terminal.scope").is_none());
        assert!(systemd_unit("0::/system.slice/ssh.service/session-1.scope").is_none());
    }

    #[tokio::test]
    async fn mismatched_process_starttime_never_signals_a_live_pid() {
        let mut child = std::process::Command::new("sleep").arg("60").spawn().unwrap();
        let mut identity = ProcessIdentity::capture(child.id()).unwrap();
        identity.start_ticks += 1;
        identity.stop().await.unwrap();
        assert!(
            child.try_wait().unwrap().is_none(),
            "a recycled/mismatched pid must not be stopped"
        );
        child.kill().unwrap();
        child.wait().unwrap();
    }

    // Subprocess fixtures return immediately in the ordinary suite. They
    // exercise the real receipt writer and reparenting without a real core.
    #[test]
    fn receipt_core_fixture() {
        let Some(config) = std::env::var_os("CLI_TEST_RECEIPT_CONFIG") else {
            return;
        };
        record_launch(std::process::id(), CoreKind::SingBox, Path::new(&config)).unwrap();
        std::thread::sleep(std::time::Duration::from_secs(60));
    }

    #[test]
    #[allow(clippy::zombie_processes)]
    fn exiting_supervisor_fixture() {
        let Some(config) = std::env::var_os("CLI_TEST_RECEIPT_CONFIG") else {
            return;
        };
        std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "mihomo_manager::process::tests::receipt_core_fixture",
                "--skip",
            ])
            .arg(config)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        // Exits immediately; the core can write its receipt after reparenting.
    }

    struct FixtureCoreGuard(ProcessIdentity);
    impl Drop for FixtureCoreGuard {
        fn drop(&mut self) {
            if self.0.matches_live() {
                let _ = nix::sys::signal::kill(
                    nix::unistd::Pid::from_raw(self.0.pid as i32),
                    nix::sys::signal::Signal::SIGKILL,
                );
            }
        }
    }

    #[tokio::test]
    async fn reparented_launch_core_is_cleaned_without_touching_another_instances_record() {
        use crate::mihomo_manager::manager::{LaunchOwnership, MihomoManager};
        let home = tempfile::tempdir().unwrap();
        let _home = crate::profile_store::store::tests::claim_test_app_home(home.path().to_path_buf()).await;
        let receipt = ReceiptTarget::new(home.path(), CoreKind::SingBox).unwrap();
        let mut supervisor = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "mihomo_manager::process::tests::exiting_supervisor_fixture"])
            .env(RECEIPT_PATH_ENV, &receipt.path)
            .env(LAUNCH_ID_ENV, &receipt.launch_id)
            .env("CLI_TEST_RECEIPT_CONFIG", &receipt.config)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        assert!(supervisor.wait().unwrap().success());
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        let core = loop {
            if let Some(core) = receipt.identity() {
                break core;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "reparented core did not write its launch receipt"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        };
        let _guard = FixtureCoreGuard(core.clone());
        let bytes = std::fs::read(&receipt.path).unwrap();
        let original: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        for (field, value) in [
            ("launch_id", serde_json::json!("other-launch")),
            ("kind", serde_json::json!("mihomo")),
            ("config", serde_json::json!("/another/config.json")),
        ] {
            let mut altered = original.clone();
            altered[field] = value;
            std::fs::write(&receipt.path, serde_json::to_vec(&altered).unwrap()).unwrap();
            assert!(receipt.identity().is_none(), "{field} must bind this launch");
        }
        for (field, value) in [
            ("start_ticks", serde_json::json!(core.start_ticks + 1)),
            ("exe", serde_json::json!("/another/executable")),
            ("cmdline", serde_json::json!([])),
        ] {
            let mut altered = original.clone();
            altered["identity"][field] = value;
            std::fs::write(&receipt.path, serde_json::to_vec(&altered).unwrap()).unwrap();
            assert!(receipt.identity().is_none(), "{field} must bind the kernel process");
        }
        std::fs::write(&receipt.path, &bytes).unwrap();
        let mut other = std::process::Command::new("sleep").arg("60").spawn().unwrap();
        let socket = home.path().join("controller.sock");
        let record_path = super::super::pidfile::path_for(&socket);
        let record = super::super::pidfile::CoreRecord::with_kind(other.id(), chrono::Utc::now(), CoreKind::SingBox);
        super::super::pidfile::write(&record_path, record.clone()).unwrap();
        let manager = MihomoManager::new(home.path().to_path_buf()).with_socket(socket);
        let ownership = LaunchOwnership::with_receipt(supervisor.id(), receipt);
        manager.cleanup_failed_launch(&ownership, &mut supervisor).await;
        assert!(
            !core.matches_live(),
            "receipt must find the core after the supervisor exits"
        );
        assert!(
            other.try_wait().unwrap().is_none(),
            "a shared record is not launch ownership"
        );
        assert_eq!(super::super::pidfile::read_record(&record_path), Some(record));
        other.kill().unwrap();
        other.wait().unwrap();
    }
}
