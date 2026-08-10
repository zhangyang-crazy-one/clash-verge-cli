use anyhow::Result;
use std::sync::OnceLock;
use std::{fs, path::PathBuf};

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
        PathBuf::from(runtime)
            .join("clash-verge-cli")
            .join("external-controller.sock")
    } else {
        PathBuf::from("/tmp/clash-verge-cli").join("external-controller.sock")
    }
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
