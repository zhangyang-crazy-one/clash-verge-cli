//! GUI mutual exclusion (task 3.5, add-singbox-dual-core).
//!
//! The GUI only understands mihomo + `config.yaml`; letting it manage
//! while the TUI runs sing-box guarantees a fight over the core. Two
//! mechanisms:
//! 1. Process detection — switching to sing-box is blocked while a GUI
//!    instance is running.
//! 2. An ownership marker file in the app data dir recording who owns
//!    the core while sing-box mode is active.

use std::path::{Path, PathBuf};

/// Marker file name, placed in the app home dir.
pub const OWNERSHIP_MARKER: &str = "core-owner.json";

/// GUI binary names (comm is truncated to 15 bytes by the kernel, so
/// `clash-verge-gui` fits exactly). The TUI binary `clash-verge-cli`
/// must NOT match — it is us.
pub fn is_gui_comm(comm: &str) -> bool {
    matches!(comm.trim(), "clash-verge" | "clash-verge-gui")
}

/// Scan `/proc` for a running GUI instance, excluding our own pid.
pub fn gui_process_running() -> bool {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return false;
    };
    for entry in entries.flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        if pid == std::process::id() {
            continue;
        }
        let Ok(comm) = std::fs::read_to_string(entry.path().join("comm")) else {
            continue;
        };
        if is_gui_comm(&comm) {
            return true;
        }
    }
    false
}

/// Persisted ownership record.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct OwnershipMarker {
    pub owner: String,
    pub core: String,
    pub pid: u32,
}

// --- Pure path-parameterized core (unit-testable without touching the
// --- global app-home OnceCell).

fn marker_path_in(home: &Path) -> PathBuf {
    home.join(OWNERSHIP_MARKER)
}

/// Write the ownership marker (owner = "tui") under `home`.
pub fn write_ownership_marker_at(home: &Path, core: &str) -> std::io::Result<PathBuf> {
    let path = marker_path_in(home);
    let marker = OwnershipMarker {
        owner: "tui".into(),
        core: core.into(),
        pid: std::process::id(),
    };
    std::fs::write(&path, serde_json::to_string(&marker)?)?;
    Ok(path)
}

/// Remove the marker under `home`. Missing file is success (idempotent).
pub fn remove_ownership_marker_at(home: &Path) {
    let _ = std::fs::remove_file(marker_path_in(home));
}

/// Read the marker under `home` if present.
pub fn read_ownership_marker_at(home: &Path) -> Option<OwnershipMarker> {
    let body = std::fs::read_to_string(marker_path_in(home)).ok()?;
    serde_json::from_str(&body).ok()
}

// --- App-dir wrappers for production callers.

fn app_home() -> Option<PathBuf> {
    clash_verge_core::utils::dirs::app_home_dir().ok()
}

pub fn write_ownership_marker(core: &str) -> std::io::Result<Option<PathBuf>> {
    match app_home() {
        Some(home) => Ok(Some(write_ownership_marker_at(&home, core)?)),
        None => Ok(None),
    }
}

pub fn remove_ownership_marker() {
    if let Some(home) = app_home() {
        remove_ownership_marker_at(&home);
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn gui_comm_matches_only_gui_binaries() {
        assert!(is_gui_comm("clash-verge"));
        assert!(is_gui_comm("clash-verge-gui"));
        assert!(!is_gui_comm("clash-verge-cli"), "the TUI itself is not the GUI");
        assert!(!is_gui_comm("mihomo"));
        assert!(!is_gui_comm(""));
    }

    #[test]
    fn marker_roundtrip_in_explicit_home() {
        // Pure path-parameterized API: no global state touched.
        let home = std::env::temp_dir().join(format!("ownership-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&home).expect("mkdir");

        assert!(read_ownership_marker_at(&home).is_none(), "no marker initially");

        write_ownership_marker_at(&home, "singbox").expect("write marker");
        let marker = read_ownership_marker_at(&home).expect("marker present");
        assert_eq!(marker.owner, "tui");
        assert_eq!(marker.core, "singbox");
        assert_eq!(marker.pid, std::process::id());

        remove_ownership_marker_at(&home);
        assert!(read_ownership_marker_at(&home).is_none(), "marker removed");
        remove_ownership_marker_at(&home); // idempotent

        let _ = std::fs::remove_dir_all(&home);
    }
}
