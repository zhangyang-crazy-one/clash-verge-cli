use anyhow::Result;
use clash_verge_core::utils::dirs::APP_ID;
use std::path::PathBuf;

/// Resolve the config directory following priority order:
/// 1. CLI override (--config-dir)
/// 2. Portable mode (exe_dir/.config/PORTABLE)
/// 3. Default: $XDG_DATA_HOME/<APP_ID> (created on first run)
///
/// The CLI is fully standalone: it never probes or shares a GUI config
/// directory. GUI configuration is only ever consumed explicitly via
/// `profile migrate --from <gui-dir>`.
pub fn resolve(cli_override: Option<PathBuf>) -> Result<PathBuf> {
    // 1. CLI override
    if let Some(path) = cli_override {
        std::fs::create_dir_all(&path)?;
        return Ok(path);
    }

    // 2. Portable mode
    if let Ok(exe) = std::env::current_exe()
        && let Some(exe_dir) = exe.parent()
    {
        let portable_flag = exe_dir.join(".config").join("PORTABLE");
        if portable_flag.exists() {
            let config_dir = exe_dir.join(".config").join(APP_ID);
            std::fs::create_dir_all(&config_dir)?;
            return Ok(config_dir);
        }
    }

    // 3. Standalone default (first launch creates it)
    let default = dirs::data_dir()
        .ok_or_else(|| anyhow::anyhow!("could not determine data directory"))?
        .join(APP_ID);
    std::fs::create_dir_all(&default)?;
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
        assert!(path.ends_with(APP_ID));
    }
}
