use anyhow::Result;
use clash_verge_core::utils::dirs::APP_ID;
use std::path::PathBuf;

/// Resolve the config directory following priority order:
/// 1. CLI override (--config-dir)
/// 2. Portable mode (exe_dir/.config/PORTABLE)
/// 3. Existing GUI path ($XDG_DATA_HOME/<APP_ID>/verge.yaml)
/// 4. Existing XDG fallback (~$/.config/clash-verge/verge.yaml)
/// 5. Default: GUI path (first-launch will create it)
pub fn resolve(cli_override: Option<PathBuf>) -> Result<PathBuf> {
    // 1. CLI override
    if let Some(path) = cli_override {
        return Ok(path);
    }

    // 2. Portable mode
    if let Ok(exe) = std::env::current_exe()
        && let Some(exe_dir) = exe.parent()
    {
        let portable_flag = exe_dir.join(".config").join("PORTABLE");
        if portable_flag.exists() {
            let config_dir = exe_dir.join(".config").join(APP_ID);
            return Ok(config_dir);
        }
    }

    // 3. GUI path (XDG_DATA_HOME)
    if let Some(data_dir) = dirs::data_dir() {
        let gui_path = data_dir.join(APP_ID);
        if gui_path.join("verge.yaml").exists() {
            return Ok(gui_path);
        }
    }

    // 4. XDG fallback
    if let Some(config_dir) = dirs::config_dir() {
        let fallback = config_dir.join("clash-verge");
        if fallback.join("verge.yaml").exists() {
            return Ok(fallback);
        }
    }

    // 5. Default: GUI path
    let default = dirs::data_dir()
        .ok_or_else(|| anyhow::anyhow!("could not determine data directory"))?
        .join(APP_ID);
    Ok(default)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cli_override_wins() {
        let path = resolve(Some(PathBuf::from("/tmp/test"))).unwrap();
        assert_eq!(path, PathBuf::from("/tmp/test"));
    }

    #[test]
    fn test_returns_path() {
        let path = resolve(None).unwrap();
        assert!(path.is_absolute() || path.starts_with("/"));
    }
}
