//! Record of the running core, shared between CLI invocations.
//!
//! The manager keeps the core's pid in memory, which a later
//! `clash-verge-cli stop` / `restart` / `status` process does not have. The
//! spawning process therefore writes `mihomo.pid` next to the controller
//! socket; other processes adopt the core from it. A pid is only trusted if
//! `/proc/<pid>/cmdline` still shows mihomo serving this socket, so a pid
//! the kernel reused for another program is never signalled.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoreRecord {
    pub pid: u32,
    /// Unix seconds.
    started_at: i64,
}

impl CoreRecord {
    pub const fn new(pid: u32, started_at: DateTime<Utc>) -> Self {
        Self {
            pid,
            started_at: started_at.timestamp(),
        }
    }

    pub fn started_at(&self) -> Option<DateTime<Utc>> {
        DateTime::from_timestamp(self.started_at, 0)
    }
}

/// `mihomo.pid` in the controller socket's directory.
pub fn path_for(socket_path: &Path) -> PathBuf {
    socket_path.with_file_name("mihomo.pid")
}

pub fn write(path: &Path, record: CoreRecord) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string(&record).map_err(std::io::Error::other)?;
    std::fs::write(path, json)
}

/// The recorded core if it is still alive and still serves `socket_path`.
pub fn read_live(path: &Path, socket_path: &Path) -> Option<CoreRecord> {
    let record: CoreRecord = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    let cmdline = std::fs::read(format!("/proc/{}/cmdline", record.pid)).ok()?;
    (is_running(record.pid) && cmdline_serves(&cmdline, socket_path)).then_some(record)
}

/// Remove the record, but only if it still names `pid` (a newer core may
/// have replaced it).
pub fn remove_if(path: &Path, pid: u32) {
    let current = std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str::<CoreRecord>(&raw).ok());
    if current.is_some_and(|record| record.pid == pid) {
        let _ = std::fs::remove_file(path);
    }
}

/// Alive and not a zombie waiting to be reaped.
pub fn is_running(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|stat| process_state(&stat))
        .is_some_and(|state| state != 'Z' && state != 'X')
}

/// State letter from `/proc/<pid>/stat` (the field after `(comm)`).
fn process_state(stat: &str) -> Option<char> {
    stat.rsplit_once(')')?.1.trim_start().chars().next()
}

/// Whether a NUL-separated command line passes `-ext-ctl-unix <socket_path>`.
fn cmdline_serves(cmdline: &[u8], socket_path: &Path) -> bool {
    let socket = socket_path.as_os_str().as_encoded_bytes();
    let args: Vec<&[u8]> = cmdline.split(|b| *b == 0).collect();
    args.windows(2)
        .any(|pair| pair[0] == b"-ext-ctl-unix" && pair[1] == socket)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cmdline_must_serve_this_socket() {
        let socket = Path::new("/run/user/1000/clash-verge-cli/external-controller.sock");
        let ours = b"/home/u/.local/share/clash-verge-cli/mihomo\0-d\0/cfg\0-ext-ctl-unix\0/run/user/1000/clash-verge-cli/external-controller.sock\0";
        assert!(cmdline_serves(ours, socket));

        let other_socket = b"mihomo\0-ext-ctl-unix\0/tmp/other.sock\0";
        assert!(!cmdline_serves(other_socket, socket));
        let reused_pid = b"/usr/bin/vim\0notes.txt\0";
        assert!(!cmdline_serves(reused_pid, socket));
    }

    #[test]
    fn process_state_skips_parentheses_in_the_command_name() {
        assert_eq!(process_state("42 (mihomo) S 1 42 42"), Some('S'));
        assert_eq!(process_state("42 (odd) name)) Z 1"), Some('Z'));
        assert_eq!(process_state("garbage"), None);
    }

    #[test]
    fn records_round_trip_and_only_the_owner_removes_them() {
        let dir = std::env::temp_dir().join(format!("cv-pid-{}", uuid::Uuid::new_v4()));
        let path = path_for(&dir.join("external-controller.sock"));
        assert!(path.ends_with("mihomo.pid"));
        assert!(CoreRecord::new(1, Utc::now()).started_at().is_some());
        let record = CoreRecord::new(std::process::id(), Utc::now());
        write(&path, record).unwrap();

        // This test process does not serve the socket, so it is not adopted.
        assert!(read_live(&path, &dir.join("external-controller.sock")).is_none());

        remove_if(&path, record.pid + 1);
        assert!(path.exists(), "another pid must not remove the record");
        remove_if(&path, record.pid);
        assert!(!path.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn this_process_is_running() {
        assert!(is_running(std::process::id()));
        assert!(!is_running(u32::MAX));
    }
}
