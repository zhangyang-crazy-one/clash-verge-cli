//! Applying the TUN flag to the runtime config.

use crate::mihomo_manager::manager::MihomoManager;
use crate::runtime_config::{RUNTIME_CONFIG_IO, reload_config_file, write_runtime_config_unlocked};

/// Write the TUN flag into the runtime config and apply it under one IO lock.
///
/// Re-reads `clash.yaml` inside the lock so a concurrent profile/mode commit
/// is not overwritten by a stale snapshot. A core this process owns restarts
/// (stop-by-pid works while the child sits in the watcher); an attached core
/// reloads the written file through the API.
///
/// The TUN capability must already be checked: the manager repeats the
/// read-only preflight before every TUN-enabled spawn, and nothing here ever
/// prompts for a password.
pub async fn apply_tun_runtime(manager: &MihomoManager, owns_core: bool, enable_tun: bool) -> Result<(), String> {
    let _guard = RUNTIME_CONFIG_IO.lock().await;
    let config = clash_verge_core::config::IClashTemp::new().await.0;
    let path = write_runtime_config_unlocked(config, enable_tun).await?;
    if owns_core {
        manager.restart().await.map(|_| ()).map_err(|error| error.to_string())
    } else {
        reload_config_file(&manager.api(), &path).await
    }
}

/// Persist the TUN flag into a freshly loaded runtime config (core stopped).
pub async fn write_tun_runtime(enable_tun: bool) -> Result<std::path::PathBuf, String> {
    let _guard = RUNTIME_CONFIG_IO.lock().await;
    let config = clash_verge_core::config::IClashTemp::new().await.0;
    write_runtime_config_unlocked(config, enable_tun).await
}

/// Read-only check before turning TUN on: the binary that would run must
/// carry the TUN capability. Returns `Ok` when no binary is known yet (the
/// spawn preflight will check the one that gets installed).
pub fn preflight_enable(manager: &MihomoManager) -> anyhow::Result<()> {
    let known = manager
        .binary_path()
        .or_else(crate::mihomo_manager::binary::candidate_without_install);
    match known {
        Some(binary) => crate::commands::privilege::require_tun_capability(&binary),
        None => Ok(()),
    }
}
