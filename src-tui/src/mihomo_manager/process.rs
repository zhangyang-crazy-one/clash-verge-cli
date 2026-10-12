//! Linux process identities and per-launch receipts. Shared pidfiles are not
//! authority to clean up a failed launch, particularly after reparenting.

use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd};
use std::os::linux::net::SocketAddrExt as _;
use std::os::unix::net::{SocketAddr, UnixDatagram};
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
    pub(super) cmdline: Vec<u8>,
}

impl ProcessIdentity {
    pub(crate) fn exe_string(&self) -> Option<&str> {
        self.exe.to_str()
    }

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
    /// The actual unreaped Child, not a supplied PID, authorizes this bind.
    /// File capabilities may make /proc/PID/exe unreadable after exec.
    pub(crate) fn spawned(child: &tokio::process::Child) -> anyhow::Result<Self> {
        let pid = child.id().context("spawned child has already been reaped")?;
        let parent = Self::capture(std::process::id())?;
        Self::capture_child(pid, &parent)
    }

    fn try_clone(&self) -> anyhow::Result<Self> {
        Ok(Self {
            pid: self.pid,
            start_ticks: self.start_ticks,
            pidfd: self.pidfd.try_clone()?,
        })
    }

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

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct LaunchReceipt {
    launch_id: String,
    supervisor_start_ticks: u64,
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
    socket: UnixDatagram,
    supervisor: Option<(u32, u64)>,
    received: parking_lot::Mutex<Option<(LaunchReceipt, OwnedProcess)>>,
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
        let launch_id = uuid::Uuid::new_v4().to_string();
        // Abstract Linux sockets avoid the filesystem socket path limit;
        // the owner-only directory still bounds this launch's lifetime.
        let socket = UnixDatagram::bind_addr(&receipt_address(&launch_id)?)?;
        // SAFETY: the fd and the integer option buffer remain live.
        let enabled: nix::libc::c_int = 1;
        let result = unsafe {
            nix::libc::setsockopt(
                socket.as_raw_fd(),
                nix::libc::SOL_SOCKET,
                nix::libc::SO_PASSCRED,
                (&enabled as *const nix::libc::c_int).cast(),
                std::mem::size_of_val(&enabled) as _,
            )
        };
        if result < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(Self {
            path: directory.path().join("core.json"),
            _directory: directory,
            launch_id,
            kind,
            config,
            socket,
            supervisor: None,
            received: parking_lot::Mutex::new(None),
        })
    }

    pub(crate) fn bind_supervisor(&mut self, pid: u32) {
        if self.supervisor.is_some_and(|(bound, _)| bound == pid) {
            return;
        }
        // The caller still owns the unreaped supervisor Child, including
        // when it already exited: its numeric PID cannot yet be reused.
        self.supervisor = std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .and_then(|stat| start_ticks(&stat))
            .map(|ticks| (pid, ticks));
    }

    fn receive(&self) {
        // The abstract address is discoverable; unrelated senders must not
        // keep a readiness/cleanup check busy indefinitely.
        for _ in 0..64 {
            let (body, credentials, mut fds) = match receive_receipt(&self.socket) {
                Ok(Some(message)) => message,
                // recvmsg consumed this invalid packet and closed its fds;
                // an authenticated receipt may already be queued behind it.
                Err(error) if error.kind() == std::io::ErrorKind::InvalidData => continue,
                _ => break,
            };
            let sender_ticks = std::fs::read_to_string(format!("/proc/{}/stat", credentials.pid))
                .ok()
                .and_then(|stat| start_ticks(&stat));
            if self.supervisor.map(|(pid, _)| pid) != Some(credentials.pid as u32)
                || self.supervisor.map(|(_, ticks)| ticks) != sender_ticks
                // SAFETY: geteuid has no arguments.
                || credentials.uid != unsafe { nix::libc::geteuid() }
                || fds.len() != 1
            {
                continue;
            }
            let Ok(receipt) = serde_json::from_slice::<LaunchReceipt>(&body) else {
                continue;
            };
            let pidfd = fds.remove(0);
            // fdinfo is kernel evidence that SCM_RIGHTS carried the pidfd
            // for the receipt's PID, rather than an unrelated descriptor.
            let fdinfo =
                std::fs::read_to_string(format!("/proc/self/fdinfo/{}", pidfd.as_raw_fd())).unwrap_or_default();
            let fd_pid = fdinfo
                .lines()
                .find_map(|line| line.strip_prefix("Pid:").and_then(|pid| pid.trim().parse::<u32>().ok()));
            if fd_pid != Some(receipt.identity.pid)
                || self.supervisor != Some((credentials.pid as u32, receipt.supervisor_start_ticks))
                || receipt.launch_id != self.launch_id
                || receipt.kind != self.kind
                || receipt.config != self.config
            {
                continue;
            }
            let process = OwnedProcess {
                pid: receipt.identity.pid,
                start_ticks: receipt.identity.start_ticks,
                pidfd,
            };
            if process.matches_process() {
                *self.received.lock() = Some((receipt, process));
            }
        }
    }

    pub(crate) fn identity(&self) -> Option<ProcessIdentity> {
        self.receive();
        let receipt: LaunchReceipt = serde_json::from_slice(&std::fs::read(&self.path).ok()?).ok()?;
        let received = self.received.lock();
        let (authenticated, process) = received.as_ref()?;
        (receipt.launch_id == self.launch_id
            && receipt.kind == self.kind
            && receipt.config == self.config
            && receipt
                .identity
                .cmdline
                .split(|b| *b == 0)
                .any(|arg| arg == self.config.as_os_str().as_encoded_bytes())
            && receipt == *authenticated
            && process.matches_process())
        .then_some(receipt.identity)
    }

    pub(crate) fn bind(&self) -> anyhow::Result<OwnedProcess> {
        let identity = self
            .identity()
            .context("core has no authenticated per-launch receipt")?;
        let received = self.received.lock();
        let (receipt, process) = received.as_ref().context("missing received core handle")?;
        anyhow::ensure!(
            receipt.identity == identity && process.matches_process(),
            "launch process changed before binding"
        );
        process.try_clone()
    }

    /// Readiness verifies the planned executable and argv independently of
    /// cleanup ownership. Only an authenticated child handle can tolerate
    /// EACCES on exe, and its live argv must still match the launch exactly.
    pub(crate) fn verified_identity(&self) -> Option<ProcessIdentity> {
        let identity = self.identity()?;
        match ProcessIdentity::capture(identity.pid) {
            Ok(live) if live == identity => Some(identity),
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|error| error.raw_os_error() == Some(nix::libc::EACCES))
                    && std::fs::read_link(format!("/proc/{}/exe", identity.pid))
                        .is_err_and(|error| error.raw_os_error() == Some(nix::libc::EACCES))
                    && std::fs::read(format!("/proc/{}/cmdline", identity.pid))
                        .is_ok_and(|cmdline| cmdline == identity.cmdline)
                    && identity.matches_process() =>
            {
                Some(identity)
            }
            _ => None,
        }
    }
}

/// Called synchronously immediately after spawning, before readiness. A
/// supervisor can subsequently exit; its child's immutable identity remains.
pub(crate) fn record_launch(
    process: &OwnedProcess,
    kind: CoreKind,
    config: &Path,
    executable: &Path,
    cmdline: Vec<u8>,
) -> anyhow::Result<()> {
    let (Some(path), Some(launch_id)) = (std::env::var_os(RECEIPT_PATH_ENV), std::env::var_os(LAUNCH_ID_ENV)) else {
        return Ok(());
    };
    let path = PathBuf::from(path);
    // Parent finished readiness and dropped its directory: subsequent crash
    // recovery belongs to the supervisor, and needs no caller receipt.
    if !path.parent().is_some_and(Path::exists) {
        return Ok(());
    }
    let supervisor = OwnedProcess::capture(std::process::id())?;
    let child = OwnedProcess::capture_child(process.pid, &supervisor)?;
    anyhow::ensure!(
        child.start_ticks == process.start_ticks,
        "receipt must name this supervisor's actual child"
    );
    let receipt = LaunchReceipt {
        launch_id: launch_id.to_string_lossy().into_owned(),
        supervisor_start_ticks: supervisor.start_ticks,
        identity: ProcessIdentity {
            pid: process.pid,
            start_ticks: process.start_ticks,
            exe: executable.to_path_buf(),
            cmdline,
        },
        kind,
        config: config.to_path_buf(),
    };
    let mut file = tempfile::NamedTempFile::new_in(path.parent().context("receipt has no parent")?)?;
    let body = serde_json::to_vec(&receipt)?;
    std::io::Write::write_all(&mut file, &body)?;
    file.persist(&path).map_err(|error| error.error)?;
    let socket = UnixDatagram::unbound()?;
    socket.connect_addr(&receipt_address(&receipt.launch_id)?)?;
    send_receipt(&socket, &body, &process.pidfd)?;
    Ok(())
}

fn receipt_address(launch_id: &str) -> std::io::Result<SocketAddr> {
    SocketAddr::from_abstract_name(format!("clash-verge-cli-launch-{launch_id}"))
}

fn send_receipt(socket: &UnixDatagram, body: &[u8], pidfd: &OwnedFd) -> std::io::Result<()> {
    let mut control = [0usize; 8];
    let mut iov = nix::libc::iovec {
        iov_base: body.as_ptr().cast_mut().cast(),
        iov_len: body.len(),
    };
    // SAFETY: zero is the empty msghdr; its pointer fields are set below.
    let mut message: nix::libc::msghdr = unsafe { std::mem::zeroed() };
    message.msg_iov = &mut iov;
    message.msg_iovlen = 1;
    message.msg_control = control.as_mut_ptr().cast();
    // SAFETY: control is aligned and large enough for one integer fd.
    unsafe {
        message.msg_controllen = nix::libc::CMSG_SPACE(std::mem::size_of::<i32>() as _) as _;
        let header = nix::libc::CMSG_FIRSTHDR(&message);
        (*header).cmsg_level = nix::libc::SOL_SOCKET;
        (*header).cmsg_type = nix::libc::SCM_RIGHTS;
        (*header).cmsg_len = nix::libc::CMSG_LEN(std::mem::size_of::<i32>() as _) as _;
        std::ptr::write_unaligned(nix::libc::CMSG_DATA(header).cast::<i32>(), pidfd.as_raw_fd());
        if nix::libc::sendmsg(socket.as_raw_fd(), &message, nix::libc::MSG_DONTWAIT) < 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

type ReceivedReceipt = (Vec<u8>, nix::libc::ucred, Vec<OwnedFd>);

// GNU uses usize for these lengths; musl uses socklen_t. The bounded
// control buffer fits both ABIs, and parsing indexes must use usize.
#[allow(clippy::unnecessary_cast)]
fn receive_receipt(socket: &UnixDatagram) -> std::io::Result<Option<ReceivedReceipt>> {
    let mut body = vec![0u8; 65536];
    let mut control = [0usize; 32];
    let mut iov = nix::libc::iovec {
        iov_base: body.as_mut_ptr().cast(),
        iov_len: body.len(),
    };
    // SAFETY: zero is the empty msghdr; all storage outlives recvmsg.
    let mut message: nix::libc::msghdr = unsafe { std::mem::zeroed() };
    message.msg_iov = &mut iov;
    message.msg_iovlen = 1;
    message.msg_control = control.as_mut_ptr().cast();
    message.msg_controllen = std::mem::size_of_val(&control) as _;
    // SAFETY: message contains valid writable buffers; received fds are
    // marked CLOEXEC before another thread can exec.
    let count = unsafe {
        nix::libc::recvmsg(
            socket.as_raw_fd(),
            &mut message,
            nix::libc::MSG_DONTWAIT | nix::libc::MSG_CMSG_CLOEXEC,
        )
    };
    if count < 0 {
        let error = std::io::Error::last_os_error();
        return if error.kind() == std::io::ErrorKind::WouldBlock {
            Ok(None)
        } else {
            Err(error)
        };
    }
    let mut fds = Vec::new();
    let mut credentials = None;
    // SAFETY: CMSG macros walk only the returned control buffer. Every
    // transferred fd is immediately owned, including rejected messages.
    unsafe {
        let mut header = nix::libc::CMSG_FIRSTHDR(&message);
        while !header.is_null() {
            if (*header).cmsg_level == nix::libc::SOL_SOCKET {
                let size = ((*header).cmsg_len as usize).saturating_sub(nix::libc::CMSG_LEN(0) as usize);
                if (*header).cmsg_type == nix::libc::SCM_RIGHTS {
                    for index in 0..size / std::mem::size_of::<i32>() {
                        let fd = std::ptr::read_unaligned(nix::libc::CMSG_DATA(header).cast::<i32>().add(index));
                        fds.push(OwnedFd::from_raw_fd(fd));
                    }
                } else if (*header).cmsg_type == nix::libc::SCM_CREDENTIALS
                    && size == std::mem::size_of::<nix::libc::ucred>()
                {
                    credentials = Some(std::ptr::read_unaligned(
                        nix::libc::CMSG_DATA(header).cast::<nix::libc::ucred>(),
                    ));
                }
            }
            header = nix::libc::CMSG_NXTHDR(&message, header);
        }
    }
    let Some(credentials) =
        credentials.filter(|_| message.msg_flags & (nix::libc::MSG_TRUNC | nix::libc::MSG_CTRUNC) == 0)
    else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "incomplete launch receipt",
        ));
    };
    body.truncate(count as usize);
    Ok(Some((body, credentials, fds)))
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

    fn command_cmdline(command: &tokio::process::Command) -> Vec<u8> {
        let mut bytes = Vec::new();
        for arg in std::iter::once(command.as_std().get_program()).chain(command.as_std().get_args()) {
            bytes.extend_from_slice(arg.as_encoded_bytes());
            bytes.push(0);
        }
        bytes
    }

    async fn wait_for_fixture(config: &Path) {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        while !config.with_extension("ready").exists() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "fixture did not become nondumpable"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    fn nondumpable_command(config: &Path) -> tokio::process::Command {
        let mut command = tokio::process::Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "mihomo_manager::process::tests::receipt_core_fixture",
                "--skip",
            ])
            .arg(config)
            .env("CLI_TEST_RECEIPT_CONFIG", config)
            .env("CLI_TEST_NONDUMPABLE", "1")
            .env_remove(RECEIPT_PATH_ENV)
            .env_remove(LAUNCH_ID_ENV)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        command
    }

    #[tokio::test]
    async fn nondumpable_direct_spawn_binds_and_failure_cleanup_stops_only_that_child() {
        let home = tempfile::tempdir().unwrap();
        let config = home.path().join("owned.json");
        let other_config = home.path().join("foreign.json");
        let mut command = nondumpable_command(&config);
        let cmdline = command_cmdline(&command);
        let mut child = command.spawn().unwrap();
        let mut other = nondumpable_command(&other_config).spawn().unwrap();
        wait_for_fixture(&config).await;
        wait_for_fixture(&other_config).await;
        for pid in [child.id().unwrap(), other.id().unwrap()] {
            let error = ProcessIdentity::capture(pid).unwrap_err();
            assert_eq!(
                error.downcast_ref::<std::io::Error>().unwrap().kind(),
                std::io::ErrorKind::PermissionDenied,
                "must exercise the real unreadable exe, not an exited fixture"
            );
        }
        let owned = crate::mihomo_manager::manager::bind_spawned_core(
            &mut child,
            CoreKind::SingBox,
            Some(&config),
            &std::env::current_exe().unwrap(),
            cmdline,
        )
        .await
        .unwrap();
        assert!(
            child.try_wait().unwrap().is_none(),
            "legal nondumpable spawn must not be killed during binding"
        );
        let mut wrong_generation = owned.try_clone().unwrap();
        wrong_generation.start_ticks += 1;
        wrong_generation.stop().await.unwrap();
        assert!(
            child.try_wait().unwrap().is_none(),
            "a mismatched generation has no authority"
        );
        owned.stop().await.unwrap();
        child.wait().await.unwrap();
        assert!(
            other.try_wait().unwrap().is_none(),
            "same executable and uid are not ownership"
        );
        other.kill().await.unwrap();
        other.wait().await.unwrap();
    }

    #[tokio::test]
    async fn forged_receipt_and_foreign_sender_cannot_authorize_nondumpable_process() {
        let home = tempfile::tempdir().unwrap();
        let _home = crate::profile_store::store::tests::claim_test_app_home(home.path().to_path_buf()).await;
        let mut target = ReceiptTarget::new(home.path(), CoreKind::SingBox).unwrap();
        let mut supervisor = std::process::Command::new("sleep").arg("60").spawn().unwrap();
        target.bind_supervisor(supervisor.id());
        let mut foreign = nondumpable_command(&target.config).spawn().unwrap();
        wait_for_fixture(&target.config).await;
        let owned = OwnedProcess::spawned(&foreign).unwrap();
        assert!(ProcessIdentity::capture(owned.pid).is_err());
        let forged = LaunchReceipt {
            launch_id: target.launch_id.clone(),
            supervisor_start_ticks: target.supervisor.unwrap().1,
            identity: ProcessIdentity {
                pid: owned.pid,
                start_ticks: owned.start_ticks,
                exe: std::env::current_exe().unwrap(),
                cmdline: command_cmdline(&nondumpable_command(&target.config)),
            },
            kind: CoreKind::SingBox,
            config: target.config.clone(),
        };
        let body = serde_json::to_vec(&forged).unwrap();
        std::fs::write(&target.path, &body).unwrap();
        assert!(
            target.identity().is_none(),
            "a disk record is not a kernel ownership handle"
        );
        let sender = UnixDatagram::unbound().unwrap();
        sender
            .connect_addr(&receipt_address(&target.launch_id).unwrap())
            .unwrap();
        send_receipt(&sender, &body, &owned.pidfd).unwrap();
        assert!(
            target.bind().is_err(),
            "matching uid, launch metadata and pidfd cannot impersonate the known supervisor"
        );
        assert!(
            forged.identity.bind().is_err(),
            "an external unreadable-exe identity remains strict"
        );
        forged.identity.stop().await.unwrap();
        assert!(foreign.try_wait().unwrap().is_none());
        foreign.kill().await.unwrap();
        foreign.wait().await.unwrap();
        supervisor.kill().unwrap();
        supervisor.wait().unwrap();
    }

    #[tokio::test]
    async fn transferred_handle_must_match_receipt_pid_generation_and_fd_kind() {
        let home = tempfile::tempdir().unwrap();
        let _home = crate::profile_store::store::tests::claim_test_app_home(home.path().to_path_buf()).await;
        let mut target = ReceiptTarget::new(home.path(), CoreKind::SingBox).unwrap();
        target.bind_supervisor(std::process::id());
        let mut child = nondumpable_command(&target.config).spawn().unwrap();
        wait_for_fixture(&target.config).await;
        let owned = OwnedProcess::spawned(&child).unwrap();
        let sender = UnixDatagram::unbound().unwrap();
        sender
            .connect_addr(&receipt_address(&target.launch_id).unwrap())
            .unwrap();
        let mut receipt = LaunchReceipt {
            launch_id: target.launch_id.clone(),
            supervisor_start_ticks: target.supervisor.unwrap().1,
            identity: ProcessIdentity {
                pid: owned.pid,
                start_ticks: owned.start_ticks,
                exe: std::env::current_exe().unwrap(),
                cmdline: command_cmdline(&nondumpable_command(&target.config)),
            },
            kind: CoreKind::SingBox,
            config: target.config.clone(),
        };
        for invalid in [
            "pid",
            "generation",
            "fd-kind",
            "supervisor-generation",
            "kernel-supervisor-generation",
        ] {
            let original_pid = receipt.identity.pid;
            let original_ticks = receipt.identity.start_ticks;
            let original_supervisor = receipt.supervisor_start_ticks;
            let original_binding = target.supervisor;
            match invalid {
                "pid" => receipt.identity.pid = std::process::id(),
                "generation" => receipt.identity.start_ticks += 1,
                "supervisor-generation" => receipt.supervisor_start_ticks += 1,
                "kernel-supervisor-generation" => {
                    receipt.supervisor_start_ticks += 1;
                    target.supervisor = Some((std::process::id(), receipt.supervisor_start_ticks));
                }
                _ => {}
            }
            let body = serde_json::to_vec(&receipt).unwrap();
            std::fs::write(&target.path, &body).unwrap();
            let wrong_fd: OwnedFd = std::fs::File::open("/dev/null").unwrap().into();
            send_receipt(
                &sender,
                &body,
                if invalid == "fd-kind" { &wrong_fd } else { &owned.pidfd },
            )
            .unwrap();
            assert!(target.bind().is_err(), "{invalid} must be rejected");
            receipt.identity.pid = original_pid;
            receipt.identity.start_ticks = original_ticks;
            receipt.supervisor_start_ticks = original_supervisor;
            target.supervisor = original_binding;
        }
        assert!(child.try_wait().unwrap().is_none());
        child.kill().await.unwrap();
        child.wait().await.unwrap();
    }

    #[tokio::test]
    async fn receipt_write_failure_reaps_the_actual_nondumpable_spawn() {
        let home = tempfile::tempdir().unwrap();
        let _home = crate::profile_store::store::tests::claim_test_app_home(home.path().to_path_buf()).await;
        let target = ReceiptTarget::new(home.path(), CoreKind::SingBox).unwrap();
        std::fs::create_dir(&target.path).unwrap(); // deterministic persist error
        let mut supervisor = tokio::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "mihomo_manager::process::tests::exiting_supervisor_fixture"])
            .env(RECEIPT_PATH_ENV, &target.path)
            .env(LAUNCH_ID_ENV, &target.launch_id)
            .env("CLI_TEST_RECEIPT_CONFIG", &target.config)
            .env("CLI_TEST_EXPECT_RECEIPT_FAILURE", "1")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let status = tokio::time::timeout(std::time::Duration::from_secs(10), supervisor.wait())
            .await
            .unwrap()
            .unwrap();
        assert!(status.success(), "fixture must observe the receipt-failure cleanup");
        let child_pid: u32 = std::fs::read_to_string(target.config.with_extension("ready"))
            .unwrap()
            .parse()
            .unwrap();
        assert!(!super::super::pidfile::is_running(child_pid));
    }

    #[tokio::test]
    async fn readable_exec_drift_fails_readiness_but_retains_owned_cleanup_handle() {
        let home = tempfile::tempdir().unwrap();
        let _home = crate::profile_store::store::tests::claim_test_app_home(home.path().to_path_buf()).await;
        let mut target = ReceiptTarget::new(home.path(), CoreKind::SingBox).unwrap();
        target.bind_supervisor(std::process::id());
        let mut child = tokio::process::Command::new("/bin/sh")
            .args(["-c", "read proceed; exec sleep 60"])
            .arg(&target.config)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let owned = OwnedProcess::spawned(&child).unwrap();
        let before = ProcessIdentity::capture(owned.pid).unwrap();
        let receipt = LaunchReceipt {
            launch_id: target.launch_id.clone(),
            supervisor_start_ticks: target.supervisor.unwrap().1,
            identity: before.clone(),
            kind: CoreKind::SingBox,
            config: target.config.clone(),
        };
        let body = serde_json::to_vec(&receipt).unwrap();
        std::fs::write(&target.path, &body).unwrap();
        let sender = UnixDatagram::unbound().unwrap();
        sender
            .connect_addr(&receipt_address(&target.launch_id).unwrap())
            .unwrap();
        send_receipt(&sender, &body, &owned.pidfd).unwrap();
        assert!(target.verified_identity().is_some());
        let stdin = child.stdin.take().unwrap();
        use tokio::io::AsyncWriteExt as _;
        let mut stdin = stdin;
        stdin.write_all(b"continue\n").await.unwrap();
        drop(stdin);
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        while std::fs::read_link(format!("/proc/{}/exe", owned.pid)).unwrap() == before.exe {
            assert!(tokio::time::Instant::now() < deadline);
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(
            target.verified_identity().is_none(),
            "authenticated ownership must not conceal readable exe/argv drift"
        );
        target.bind().unwrap().stop().await.unwrap();
        child.wait().await.unwrap();
    }

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

    // Subprocess fixtures return immediately in the ordinary suite. Only
    // these test-owned children change their own dumpability.
    #[test]
    fn receipt_core_fixture() {
        let Some(config) = std::env::var_os("CLI_TEST_RECEIPT_CONFIG") else {
            return;
        };
        if std::env::var_os("CLI_TEST_NONDUMPABLE").is_some() {
            // SAFETY: changes only the calling fixture process; no pointers.
            assert_eq!(unsafe { nix::libc::prctl(nix::libc::PR_SET_DUMPABLE, 0) }, 0);
        }
        std::fs::write(
            PathBuf::from(&config).with_extension("ready"),
            std::process::id().to_string(),
        )
        .unwrap();
        std::thread::sleep(std::time::Duration::from_secs(60));
    }

    #[test]
    #[allow(clippy::zombie_processes)]
    fn exiting_supervisor_fixture() {
        let Some(config) = std::env::var_os("CLI_TEST_RECEIPT_CONFIG") else {
            return;
        };
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let executable = std::env::current_exe().unwrap();
            let mut command = tokio::process::Command::new(&executable);
            command
                .args([
                    "--exact",
                    "mihomo_manager::process::tests::receipt_core_fixture",
                    "--skip",
                ])
                .arg(&config)
                .env("CLI_TEST_NONDUMPABLE", "1")
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .kill_on_drop(false);
            let cmdline = command_cmdline(&command);
            let mut child = command.spawn().unwrap();
            wait_for_fixture(Path::new(&config)).await;
            let result = crate::mihomo_manager::manager::bind_spawned_core(
                &mut child,
                CoreKind::SingBox,
                Some(Path::new(&config)),
                &executable,
                cmdline,
            )
            .await;
            if std::env::var_os("CLI_TEST_EXPECT_RECEIPT_FAILURE").is_some() {
                assert!(result.is_err());
                assert!(
                    child.try_wait().unwrap().is_some(),
                    "receipt failure must stop and reap the same nondumpable child"
                );
            } else {
                assert!(result.unwrap().matches_process());
            }
        });
        // The actual supervisor writes and transfers the child handle, then
        // exits. The datagram retains that handle even after reparenting.
    }

    struct FixtureCoreGuard(OwnedProcess);
    impl Drop for FixtureCoreGuard {
        fn drop(&mut self) {
            if self.0.matches_process() {
                let _ = self.0.signal(nix::libc::SIGKILL);
            }
        }
    }

    #[tokio::test]
    async fn reparented_launch_core_is_cleaned_without_touching_another_instances_record() {
        use crate::mihomo_manager::manager::{LaunchOwnership, MihomoManager};
        let home = tempfile::tempdir().unwrap();
        let _home = crate::profile_store::store::tests::claim_test_app_home(home.path().to_path_buf()).await;
        let mut receipt = ReceiptTarget::new(home.path(), CoreKind::SingBox).unwrap();
        // An oversized foreign datagram is queued first. Cleanup gets one
        // bounded drain after exit, so it must not lose the valid fd behind
        // a packet consumed with MSG_TRUNC.
        let foreign_sender = UnixDatagram::unbound().unwrap();
        foreign_sender
            .connect_addr(&receipt_address(&receipt.launch_id).unwrap())
            .unwrap();
        let foreign_handle = OwnedProcess::capture(std::process::id()).unwrap();
        send_receipt(&foreign_sender, &vec![0u8; 65537], &foreign_handle.pidfd).unwrap();
        let mut supervisor = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "mihomo_manager::process::tests::exiting_supervisor_fixture"])
            .env(RECEIPT_PATH_ENV, &receipt.path)
            .env(LAUNCH_ID_ENV, &receipt.launch_id)
            .env("CLI_TEST_RECEIPT_CONFIG", &receipt.config)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        receipt.bind_supervisor(supervisor.id());
        let exit_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        while crate::commands::start::supervisor_exit_status(&supervisor)
            .unwrap()
            .is_none()
        {
            assert!(tokio::time::Instant::now() < exit_deadline);
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let core = receipt.identity().expect(
            "one drain must reject the foreign oversized packet and retain the exited supervisor's legitimate receipt",
        );
        let _guard = FixtureCoreGuard(receipt.bind().unwrap());
        assert!(supervisor.wait().unwrap().success());
        assert!(
            receipt.identity().is_some(),
            "cached kernel ownership survives reaping after authentication"
        );
        assert!(
            ProcessIdentity::capture(core.pid).is_err(),
            "fixture must hide its exe from external capture"
        );
        assert!(
            receipt.verified_identity().is_some(),
            "an authenticated nondumpable child retains exact launch argv"
        );
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
        // Binding was performed while the Child was still unreaped, just
        // as launch_owned_supervisor does in production.
        let ownership = LaunchOwnership::with_receipt(supervisor.id(), receipt);
        manager.cleanup_failed_launch(&ownership, &mut supervisor).await;
        assert!(
            !core.matches_process(),
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
