// Foundation module — `mihomo_binary_path`, `system_mihomo`, and
// `ensure_executable` are wired up by Plan 02-03.
#![allow(dead_code, unused_imports)]

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

/// D-01: bundled binary path. Resolves to
/// `$XDG_DATA_HOME/clash-verge-cli/mihomo` with a fallback to
/// `~/.local/share/clash-verge-cli/mihomo` for systems without XDG.
///
/// Actual extraction happens in a later phase — this function only
/// resolves the path the binary should live at.
pub fn mihomo_binary_path() -> PathBuf {
    if let Some(data_dir) = std::env::var_os("XDG_DATA_HOME") {
        return PathBuf::from(data_dir).join("clash-verge-cli").join("mihomo");
    }
    let home = std::env::var_os("HOME").unwrap_or_default();
    PathBuf::from(home)
        .join(".local")
        .join("share")
        .join("clash-verge-cli")
        .join("mihomo")
}

/// Best-effort system mihomo fallback. Checks standard XDG `bin` first,
/// then common system paths. Returns `None` if nothing is present.
pub fn system_mihomo() -> Option<PathBuf> {
    let candidates = [
        dirs::executable_dir(),
        Some(PathBuf::from("/usr/bin")),
        Some(PathBuf::from("/usr/local/bin")),
    ];
    let mut seen = std::collections::HashSet::new();
    for dir in candidates.into_iter().flatten() {
        let path = dir.join("verge-mihomo");
        if seen.insert(path.clone()) && path.exists() {
            return Some(path);
        }
    }
    None
}

/// Set the executable bit on the binary. Idempotent — if the bits are
/// already 0o755 we return success without touching the inode.
pub async fn ensure_executable(path: &Path) -> std::io::Result<()> {
    let target = std::fs::Permissions::from_mode(0o755);
    let current = tokio::fs::metadata(path).await?.permissions();
    // Mask file-type bits — mode() includes e.g., 0o100755 (regular file)
    if (current.mode() & 0o777) == target.mode() {
        return Ok(());
    }
    tokio::fs::set_permissions(path, target).await
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn test_mihomo_binary_path_uses_xdg_data_home() {
        let _guard = ENV_LOCK.lock().unwrap();
        let prev = std::env::var_os("XDG_DATA_HOME");
        // SAFETY: this is a single-threaded test runner for these tests.
        unsafe {
            std::env::set_var("XDG_DATA_HOME", "/tmp/test-xdg");
        }

        let path = mihomo_binary_path();
        assert!(path.ends_with("clash-verge-cli/mihomo"), "got {path:?}");

        match prev {
            Some(v) => unsafe { std::env::set_var("XDG_DATA_HOME", v) },
            None => unsafe { std::env::remove_var("XDG_DATA_HOME") },
        }
    }

    #[test]
    fn test_mihomo_binary_path_falls_back_to_home() {
        let _guard = ENV_LOCK.lock().unwrap();
        let prev_xdg = std::env::var_os("XDG_DATA_HOME");
        let prev_home = std::env::var_os("HOME");
        unsafe {
            std::env::remove_var("XDG_DATA_HOME");
            std::env::set_var("HOME", "/tmp/fake-home");
        }

        let path = mihomo_binary_path();
        assert!(
            path.starts_with("/tmp/fake-home/.local/share/clash-verge-cli/mihomo"),
            "got {path:?}"
        );

        match prev_xdg {
            Some(v) => unsafe { std::env::set_var("XDG_DATA_HOME", v) },
            None => unsafe { std::env::remove_var("XDG_DATA_HOME") },
        }
        match prev_home {
            Some(v) => unsafe { std::env::set_var("HOME", v) },
            None => unsafe { std::env::remove_var("HOME") },
        }
    }
}
