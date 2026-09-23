pub mod askpass;
pub mod connections;
pub mod daemon;
pub mod log_cleanup;
pub mod mode;
pub mod privilege;
pub mod profile;
pub mod provider;
pub mod proxy;
pub mod restart;
pub mod service;
pub mod start;
pub mod status;
pub mod stop;
pub mod sysproxy;
pub mod tun;

use std::path::PathBuf;

use crate::mihomo_api::MihomoApi;
use crate::mihomo_manager::manager::MihomoManager;
use clash_verge_core::config::IClashTemp;

/// Build a MihomoManager wired with config from the standalone config dir.
pub async fn build_manager(config_dir: PathBuf) -> anyhow::Result<MihomoManager> {
    let clash = IClashTemp::new().await;
    let _info = clash.get_client_info();
    // Read Unix socket path from the CLI's own clash config; fall back to the
    // standalone socket (never a GUI path).
    let socket_path = clash
        .0
        .get("external-controller-unix")
        .and_then(|v| v.as_str())
        .map(PathBuf::from)
        .unwrap_or_else(clash_verge_core::utils::dirs::standalone_socket_path);
    let secret = _info.secret.unwrap_or_default();

    Ok(MihomoManager::new(config_dir)
        .with_socket(socket_path)
        .with_secret(secret))
}

/// Whether the core answers on its controller socket.
pub async fn core_running(api: &MihomoApi) -> bool {
    api.version().await.is_ok()
}

/// The controller API of a running core, or an error pointing at `start`.
pub async fn running_api(manager: &MihomoManager) -> anyhow::Result<MihomoApi> {
    let api = manager.api();
    if core_running(&api).await {
        Ok(api)
    } else {
        anyhow::bail!(
            "mihomo is not running (controller {} not reachable); start it with `clash-verge-cli start`",
            manager.socket_path().display()
        )
    }
}

/// `1.2 KiB`-style size for transfer counters.
pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_bytes_scales_units() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(1023), "1023 B");
        assert_eq!(format_bytes(1536), "1.5 KiB");
        assert_eq!(format_bytes(5 * 1024 * 1024 * 1024), "5.0 GiB");
    }
}
