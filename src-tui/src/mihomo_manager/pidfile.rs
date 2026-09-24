//! Record of the running core, shared between CLI invocations.
//!
//! The manager keeps the core's pid in memory, which a later
//! `clash-verge-cli stop` / `restart` / `status` process does not have. The
//! spawning process therefore writes `mihomo.pid` next to the controller
//! socket; other processes adopt the core from it. A pid is only trusted if
//! `/proc/<pid>/cmdline` still shows the recorded core serving this
//! controller, so a pid the kernel reused for another program is never
//! signalled.
//!
//! Both mihomo and sing-box write to the same on-disk record — the
//! [`CoreRecord::kind`] field disambiguates, and [`read_live_for_kind`]
//! refuses to adopt across kinds so the TUI cannot pick up a stale record
//! left by a sibling core that also defaulted to 7897/9090.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Which core wrote (and may consume) a [`CoreRecord`].
///
/// Serde lowercases each variant (`"mihomo"` / `"singbox"`) and the
/// `#[serde(default)]` attribute on the record's `kind` field means older
/// records written before this field existed parse as `Mihomo` — so an
/// upgrade does not strand already-running mihomo cores.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoreRecord {
    pub pid: u32,
    /// Unix seconds.
    started_at: i64,
    /// Which core wrote this record. Defaults to `Mihomo` on read for
    /// backward compat with pre-schema records.
    #[serde(default)]
    pub kind: CoreKind,
}

impl CoreRecord {
    pub const fn new(pid: u32, started_at: DateTime<Utc>) -> Self {
        Self {
            pid,
            started_at: started_at.timestamp(),
            kind: CoreKind::Mihomo,
        }
    }

    /// Record with an explicit core kind. The sing-box writer uses this;
    /// mihomo callers can keep using [`Self::new`].
    pub const fn with_kind(pid: u32, started_at: DateTime<Utc>, kind: CoreKind) -> Self {
        Self {
            pid,
            started_at: started_at.timestamp(),
            kind,
        }
    }

    pub fn started_at(&self) -> Option<DateTime<Utc>> {
        DateTime::from_timestamp(self.started_at, 0)
    }
}

/// `mihomo.pid` in the controller socket's directory.
///
/// The filename is historical (mihomo-only days) and is preserved for
/// backward compatibility: an old build writing here is still readable
/// as a mihomo record by the new code. The on-disk [`CoreRecord::kind`]
/// is what guards against cross-core adoption now.
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

/// `mihomo.stopping` in the controller socket's directory: written by a
/// process about to stop a core it did not spawn, so the process that did
/// (a TUI or `start --foreground`) treats the exit as intended instead of
/// auto-restarting the core.
pub fn stop_intent_path_for(socket_path: &Path) -> PathBuf {
    socket_path.with_file_name("mihomo.stopping")
}

pub fn mark_stop_intent(path: &Path, pid: u32) -> std::io::Result<()> {
    std::fs::write(path, pid.to_string())
}

/// Consume a stop intent recorded for `pid`. True when one was present.
pub fn take_stop_intent(path: &Path, pid: u32) -> bool {
    let matches = std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| raw.trim().parse::<u32>().ok())
        == Some(pid);
    if matches {
        let _ = std::fs::remove_file(path);
    }
    matches
}

/// Backward-compatible read: returns the live mihomo record if any.
///
/// New callers should use [`read_live_for_kind`] — this shim exists so the
/// out-of-scope `commands/start.rs` mihomo supervisor keeps compiling
/// without picking up a sing-box record (or vice versa).
pub fn read_live(path: &Path, socket_path: &Path) -> Option<CoreRecord> {
    read_live_for_kind(path, socket_path, CoreKind::Mihomo, None)
}

/// Read the raw on-disk record WITHOUT any kind or liveness filter.
///
/// Used by callers that need to inspect what is recorded regardless of
/// whether they would adopt it themselves — most importantly the
/// `start` path's cross-kind guard, which must refuse to overwrite a
/// running different-kind record (the same file name is shared by
/// both cores) without first checking the recorded kind.
pub fn read_record(path: &Path) -> Option<CoreRecord> {
    let body = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&body).ok()
}

/// The recorded core if it is still alive, the recorded kind matches
/// `kind`, and the kernel still has it serving the controller this
/// manager would talk to.
///
/// For mihomo the controller is identified by `-ext-ctl-unix <socket_path>`
/// on the cmdline (cheap and exact). For sing-box the listener lives
/// inside the JSON config passed via `-c <config>`; `singbox_endpoint`
/// must carry the TCP address we expect this manager to be talking to.
pub fn read_live_for_kind(
    path: &Path,
    socket_path: &Path,
    kind: CoreKind,
    singbox_endpoint: Option<SocketAddr>,
) -> Option<CoreRecord> {
    let record: CoreRecord = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    // Cross-core rejection: a sing-box manager must never adopt a record
    // a mihomo supervisor wrote (and vice versa), even when both cores
    // happen to share the same on-disk pid file name.
    if record.kind != kind {
        return None;
    }
    let cmdline = std::fs::read(format!("/proc/{}/cmdline", record.pid)).ok()?;
    let serves = match kind {
        CoreKind::Mihomo => cmdline_serves_mihomo(&cmdline, socket_path),
        CoreKind::SingBox => {
            // No `singbox_endpoint` means the caller cannot validate the
            // JSON config → refuse to adopt rather than guess.
            let endpoint = singbox_endpoint?;
            cmdline_serves_singbox(&cmdline, endpoint)
        }
    };
    (is_running(record.pid) && serves).then_some(record)
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
fn cmdline_serves_mihomo(cmdline: &[u8], socket_path: &Path) -> bool {
    let socket = socket_path.as_os_str().as_encoded_bytes();
    let args: Vec<&[u8]> = cmdline.split(|b| *b == 0).collect();
    args.windows(2)
        .any(|pair| pair[0] == b"-ext-ctl-unix" && pair[1] == socket)
}

/// Whether a sing-box process is still serving `expected_endpoint`.
///
/// sing-box is invoked as `sing-box run -c <config_path>` and the
/// `clash_api.external_controller` listener lives inside that JSON
/// document (sing-box has no unix-socket support for the controller).
/// The verification has to read the JSON to know what the binary is
/// actually bound to; the cmdline only points at the config.
///
/// Three guards so a reused pid that has been swapped to something
/// unrelated never gets adopted:
/// 1. binary name starts with `sing-box`;
/// 2. cmdline carries `-c <path>`;
/// 3. the config parses and its clash_api listener matches.
fn cmdline_serves_singbox(cmdline: &[u8], expected_endpoint: SocketAddr) -> bool {
    let args: Vec<&[u8]> = cmdline.split(|b| *b == 0).collect();
    let binary_name = args
        .first()
        .copied()
        .unwrap_or(&[])
        .split(|b| *b == b'/')
        .next_back()
        .and_then(|name| std::str::from_utf8(name).ok())
        .unwrap_or("");
    if !binary_name.starts_with("sing-box") {
        return false;
    }
    let config_path = match args.windows(2).find(|pair| pair[0] == b"-c") {
        Some(pair) => match std::str::from_utf8(pair[1]) {
            Ok(s) => Path::new(s),
            Err(_) => return false,
        },
        None => return false,
    };
    let body = match std::fs::read_to_string(config_path) {
        Ok(body) => body,
        Err(_) => return false,
    };
    let config: serde_json::Value = match serde_json::from_str(&body) {
        Ok(value) => value,
        Err(_) => return false,
    };
    config
        .pointer("/experimental/clash_api/external_controller")
        .and_then(|value| value.as_str())
        .and_then(|listen| listen.parse::<SocketAddr>().ok())
        == Some(expected_endpoint)
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn cmdline_must_serve_this_socket() {
        let socket = Path::new("/run/user/1000/clash-verge-cli/external-controller.sock");
        let ours = b"/home/u/.local/share/clash-verge-cli/mihomo\0-d\0/cfg\0-ext-ctl-unix\0/run/user/1000/clash-verge-cli/external-controller.sock\0";
        assert!(cmdline_serves_mihomo(ours, socket));

        let other_socket = b"mihomo\0-ext-ctl-unix\0/tmp/other.sock\0";
        assert!(!cmdline_serves_mihomo(other_socket, socket));
        let reused_pid = b"/usr/bin/vim\0notes.txt\0";
        assert!(!cmdline_serves_mihomo(reused_pid, socket));
    }

    #[test]
    fn singbox_cmdline_recognized_with_matching_controller() {
        // Stage a sing-box config that listens on the expected endpoint.
        let dir = std::env::temp_dir().join(format!("cv-sb-match-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let config_path = dir.join("sb.json");
        std::fs::write(
            &config_path,
            br#"{"experimental":{"clash_api":{"external_controller":"127.0.0.1:9090","secret":"s"}}}"#,
        )
        .unwrap();

        let mut cmdline = Vec::new();
        cmdline.extend_from_slice(b"/usr/local/bin/sing-box\0");
        cmdline.extend_from_slice(b"run\0");
        cmdline.extend_from_slice(b"-c\0");
        cmdline.extend_from_slice(config_path.as_os_str().as_encoded_bytes());
        cmdline.push(0);

        let endpoint: SocketAddr = "127.0.0.1:9090".parse().unwrap();
        assert!(cmdline_serves_singbox(&cmdline, endpoint));
    }

    #[test]
    fn singbox_cmdline_rejects_wrong_controller_port() {
        let dir = std::env::temp_dir().join(format!("cv-sb-wrong-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let config_path = dir.join("sb.json");
        std::fs::write(
            &config_path,
            br#"{"experimental":{"clash_api":{"external_controller":"127.0.0.1:9090"}}}"#,
        )
        .unwrap();
        let mut cmdline = Vec::new();
        cmdline.extend_from_slice(b"sing-box\0");
        cmdline.extend_from_slice(b"run\0");
        cmdline.extend_from_slice(b"-c\0");
        cmdline.extend_from_slice(config_path.as_os_str().as_encoded_bytes());
        cmdline.push(0);

        let wrong: SocketAddr = "127.0.0.1:9091".parse().unwrap();
        assert!(!cmdline_serves_singbox(&cmdline, wrong));
    }

    #[test]
    fn singbox_cmdline_rejects_non_singbox_binary() {
        // A reused pid whose binary has been swapped to something else
        // must not be adopted, even if the rest of the cmdline looks
        // sing-box-ish.
        let cmdline = b"mihomo\0run\0-c\0/fake.json\0";
        let endpoint: SocketAddr = "127.0.0.1:9090".parse().unwrap();
        assert!(!cmdline_serves_singbox(cmdline, endpoint));
    }

    #[test]
    fn singbox_cmdline_rejects_missing_or_unreadable_config() {
        // Missing -c flag → no config to read.
        let cmdline = b"sing-box\0run\0";
        let endpoint: SocketAddr = "127.0.0.1:9090".parse().unwrap();
        assert!(!cmdline_serves_singbox(cmdline, endpoint));

        // -c pointing at a path that does not exist.
        let cmdline = b"sing-box\0run\0-c\0/does/not/exist.json\0";
        assert!(!cmdline_serves_singbox(cmdline, endpoint));

        // -c pointing at non-JSON content.
        let dir = std::env::temp_dir().join(format!("cv-sb-bad-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let bad = dir.join("bad.json");
        std::fs::write(&bad, b"this is not json").unwrap();
        let mut cmdline = Vec::new();
        cmdline.extend_from_slice(b"sing-box\0-c\0");
        cmdline.extend_from_slice(bad.as_os_str().as_encoded_bytes());
        cmdline.push(0);
        assert!(!cmdline_serves_singbox(&cmdline, endpoint));
        let _ = std::fs::remove_dir_all(&dir);
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
    fn core_record_kind_defaults_to_mihomo_on_missing_field() {
        // A record written before this schema existed must still parse
        // (so an upgrade does not strand already-running mihomo cores).
        let legacy = r#"{"pid":42,"started_at":1700000000}"#;
        let parsed: CoreRecord = serde_json::from_str(legacy).unwrap();
        assert_eq!(parsed.kind, CoreKind::Mihomo);
        assert_eq!(parsed.pid, 42);
        assert_eq!(parsed.started_at().map(|t| t.timestamp()), Some(1700000000));
    }

    #[test]
    fn core_record_kind_round_trips_with_explicit_value() {
        let record = CoreRecord::with_kind(42, Utc::now(), CoreKind::SingBox);
        let body = serde_json::to_string(&record).unwrap();
        assert!(body.contains("\"kind\":\"singbox\""), "{body}");
        let parsed: CoreRecord = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed.kind, CoreKind::SingBox);
        assert_eq!(parsed.pid, 42);
    }

    #[test]
    fn read_live_refuses_cross_kind_adoption() {
        // A sing-box record on disk must not be adopted by a mihomo
        // reader (and vice versa), even when both files share the same
        // `mihomo.pid` filename.
        let dir = std::env::temp_dir().join(format!("cv-pid-kind-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = path_for(&dir.join("external-controller.sock"));
        let now = Utc::now();
        let singbox_record = CoreRecord::with_kind(std::process::id(), now, CoreKind::SingBox);
        write(&path, singbox_record).unwrap();

        // A mihomo manager asks read_live_for_kind(Mihomo) — must refuse.
        let mihomo_socket = dir.join("external-controller.sock");
        assert!(
            read_live_for_kind(&path, &mihomo_socket, CoreKind::Mihomo, None).is_none(),
            "mihomo must not adopt a singbox record"
        );

        // A sing-box manager asks read_live_for_kind(SingBox) without an
        // endpoint — must refuse (cannot verify the controller).
        assert!(
            read_live_for_kind(&path, &mihomo_socket, CoreKind::SingBox, None).is_none(),
            "singbox reader needs an endpoint to adopt"
        );

        // Sanity: this process serves neither socket, so even with the
        // right kind the adoption still fails on the cmdline check.
        let endpoint: SocketAddr = "127.0.0.1:9090".parse().unwrap();
        assert!(
            read_live_for_kind(&path, &mihomo_socket, CoreKind::SingBox, Some(endpoint)).is_none(),
            "this test process is not a sing-box; adoption must fail"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_live_legacy_shim_accepts_only_mihomo() {
        // A pre-schema record (no kind field) must be picked up by the
        // backward-compat shim used by `commands/start.rs`.
        let dir = std::env::temp_dir().join(format!("cv-pid-legacy-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("external-controller.sock");
        let path = path_for(&socket);
        let legacy = r#"{"pid":42,"started_at":1700000000}"#;
        std::fs::write(&path, legacy).unwrap();

        // Pid 42 is unlikely to be alive in the test runner, so the
        // shim must still refuse — but the kind defaulting to mihomo
        // is what allows the legacy record to even pass the kind check.
        assert!(read_live(&path, &socket).is_none());

        // Now make sure a kind=SingBox legacy path is not adopted via
        // the shim (it would adopt a singbox as a mihomo, which is the
        // cross-core hazard the new schema was added to prevent).
        let path = path_for(&socket);
        std::fs::write(
            &path,
            format!(r#"{{"pid":42,"started_at":1700000000,"kind":"singbox"}}"#),
        )
        .unwrap();
        assert!(read_live(&path, &socket).is_none(), "legacy shim must stay mihomo-only");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stop_intent_is_consumed_only_for_its_pid() {
        let dir = std::env::temp_dir().join(format!("cv-stop-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = stop_intent_path_for(&dir.join("external-controller.sock"));
        assert!(path.ends_with("mihomo.stopping"));
        assert!(!take_stop_intent(&path, 42), "no marker, no intent");

        mark_stop_intent(&path, 42).unwrap();
        assert!(!take_stop_intent(&path, 43), "another core's exit is not intended");
        assert!(path.exists());
        assert!(take_stop_intent(&path, 42));
        assert!(!path.exists(), "the intent is consumed");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn this_process_is_running() {
        assert!(is_running(std::process::id()));
        assert!(!is_running(u32::MAX));
    }
}
