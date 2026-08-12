use anyhow::Result;
use std::sync::OnceLock;
use std::{fs, path::Path, path::PathBuf};

pub static APP_ID: &str = "clash-verge-cli";
pub static BACKUP_DIR: &str = "clash-verge-rev-backup";

pub static APP_HOME_DIR: OnceLock<PathBuf> = OnceLock::new();
pub static PORTABLE_FLAG: OnceLock<bool> = OnceLock::new();

pub fn set_app_home_dir(path: PathBuf) {
    let _ = APP_HOME_DIR.set(path);
}

pub fn set_portable_flag(flag: bool) {
    let _ = PORTABLE_FLAG.set(flag);
}

pub static CLASH_CONFIG: &str = "config.yaml";
pub static GUI_CLASH_CONFIG: &str = "clash-verge.yaml";
pub static VERGE_CONFIG: &str = "verge.yaml";
pub static PROFILE_YAML: &str = "profiles.yaml";

/// get the verge app home dir
pub fn app_home_dir() -> Result<PathBuf> {
    APP_HOME_DIR
        .get()
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("app home dir not initialized"))
}

/// profiles dir
pub fn app_profiles_dir() -> Result<PathBuf> {
    Ok(app_home_dir()?.join("profiles"))
}

/// logs dir
pub fn app_logs_dir() -> Result<PathBuf> {
    Ok(app_home_dir()?.join("logs"))
}

// latest verge log
pub fn app_latest_log() -> Result<PathBuf> {
    Ok(app_logs_dir()?.join("latest.log"))
}

/// local backups dir
pub fn local_backup_dir() -> Result<PathBuf> {
    let dir = app_home_dir()?.join(BACKUP_DIR);
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

pub fn clash_path() -> Result<PathBuf> {
    let app_dir = app_home_dir()?;
    let gui_config = app_dir.join(GUI_CLASH_CONFIG);
    // Current Clash Verge GUI releases launch Mihomo with this full generated
    // configuration. Keep the older config.yaml path as a compatibility fallback.
    Ok(if gui_config.exists() {
        gui_config
    } else {
        app_dir.join(CLASH_CONFIG)
    })
}

pub fn verge_path() -> Result<PathBuf> {
    Ok(app_home_dir()?.join(VERGE_CONFIG))
}

pub fn profiles_path() -> Result<PathBuf> {
    Ok(app_home_dir()?.join(PROFILE_YAML))
}

/// The CLI's own mihomo controller socket. Standalone by design — never
/// resolves to a GUI path.
pub fn standalone_socket_path() -> PathBuf {
    if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR") {
        standalone_socket_path_under(Path::new(&runtime))
    } else {
        standalone_socket_path_under(Path::new("/tmp"))
    }
}

/// Standalone socket path under a given runtime base dir (`$XDG_RUNTIME_DIR`
/// normally, `/tmp` fallback). Injectable so tests need no env mutation.
fn standalone_socket_path_under(base: &Path) -> PathBuf {
    base.join("clash-verge-cli").join("external-controller.sock")
}

/// Ensure the standalone external-controller socket's parent directory
/// exists and return the socket path. On a fresh install neither
/// `$XDG_RUNTIME_DIR/clash-verge-cli` nor the `/tmp` fallback exists, and
/// mihomo's unix-socket bind fails with ENOENT when the parent is absent.
/// Best-effort 0700, mirroring the systemd user runtime dir the socket
/// normally lives under.
pub fn ensure_standalone_socket_dir() -> Result<PathBuf> {
    let socket = standalone_socket_path();
    ensure_socket_parent(&socket)?;
    Ok(socket)
}

/// Create `socket`'s parent directory (0700 on unix, best-effort) if it is
/// missing. Exposed only to tests; [`ensure_standalone_socket_dir`] is the
/// production entry point.
fn ensure_socket_parent(socket: &Path) -> Result<()> {
    let parent = socket
        .parent()
        .ok_or_else(|| anyhow::anyhow!("external-controller socket path {} has no parent", socket.display()))?;
    if parent.exists() {
        return Ok(());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)
            .map_err(|error| {
                anyhow::anyhow!(
                    "failed to create external-controller socket dir {}: {error}",
                    parent.display()
                )
            })?;
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(parent).map_err(|error| {
            anyhow::anyhow!(
                "failed to create external-controller socket dir {}: {error}",
                parent.display()
            )
        })?;
    }
    Ok(())
}
pub fn path_to_str(path: &PathBuf) -> Result<&str> {
    let path_str = path
        .as_os_str()
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("failed to get path from {:?}", path))?;
    Ok(path_str)
}

pub fn get_encryption_key() -> Result<Vec<u8>> {
    let app_dir = app_home_dir()?;
    let key_path = app_dir.join(".encryption_key");

    if key_path.exists() {
        // Read existing key
        fs::read(&key_path).map_err(|e| anyhow::anyhow!("Failed to read encryption key: {}", e))
    } else {
        // Generate and save new key
        let mut key = vec![0u8; 32];
        getrandom::fill(&mut key)?;

        // Ensure directory exists
        if let Some(parent) = key_path.parent() {
            fs::create_dir_all(parent).map_err(|e| anyhow::anyhow!("Failed to create key directory: {}", e))?;
        }
        // Save key
        fs::write(&key_path, &key).map_err(|e| anyhow::anyhow!("Failed to save encryption key: {}", e))?;
        Ok(key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Unwrap a test result with a panic carrying the error (avoids
    /// `.expect()` which pi-lens flags).
    fn must<T, E: std::fmt::Display>(result: Result<T, E>, what: &str) -> T {
        match result {
            Ok(value) => value,
            Err(error) => panic!("{what}: {error}"),
        }
    }

    fn unique_temp_base(label: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!("cv-core-{label}-{nanos}"))
    }

    #[test]
    fn ensure_standalone_socket_dir_creates_missing_parent() {
        // The runtime base does not exist yet — a fresh install has no
        // $XDG_RUNTIME_DIR/clash-verge-cli — so the helper must create it.
        let base = unique_temp_base("socket");
        let socket = standalone_socket_path_under(&base);
        must(ensure_socket_parent(&socket), "ensure socket parent");

        let parent = socket.parent().expect("socket path has a parent");
        assert!(parent.is_dir(), "helper must create the socket parent dir");
        assert!(
            base.is_dir(),
            "intermediate runtime base dir must exist after recursion"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = must(std::fs::metadata(parent), "metadata").permissions().mode();
            assert_eq!(mode & 0o777, 0o700, "socket dir must be 0700");
        }

        // Idempotent: re-running against an existing dir succeeds.
        must(ensure_socket_parent(&socket), "ensure again");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn standalone_socket_path_lives_under_runtime_dir_clash_verge_cli() {
        let base = unique_temp_base("socket-path");
        let socket = standalone_socket_path_under(&base);
        assert_eq!(
            socket.parent().expect("parent").file_name().expect("name"),
            "clash-verge-cli"
        );
        assert_eq!(socket.file_name().expect("name"), "external-controller.sock");
        let _ = std::fs::remove_dir_all(&base);
    }
}
