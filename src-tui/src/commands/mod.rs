pub mod restart;
pub mod start;
pub mod status;
pub mod stop;

use std::path::PathBuf;

use crate::mihomo_manager::manager::MihomoManager;
use clash_verge_core::config::IClashTemp;

/// Build a MihomoManager wired with config from the clash-verge config dir.
pub async fn build_manager(config_dir: PathBuf) -> anyhow::Result<MihomoManager> {
    let clash = IClashTemp::new().await;
    let _info = clash.get_client_info();
    // Read Unix socket path from clash config (external-controller-unix)
    let socket_path = clash
        .0
        .get("external-controller-unix")
        .and_then(|v| v.as_str())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR") {
                PathBuf::from(runtime)
                    .join("clash-verge")
                    .join("external-controller.sock")
            } else {
                PathBuf::from("/tmp/verge/verge-mihomo.sock")
            }
        });
    let secret = _info.secret.unwrap_or_default();

    Ok(MihomoManager::new(config_dir)
        .with_socket(socket_path)
        .with_secret(secret))
}
