pub mod askpass;
pub mod daemon;
pub mod log_cleanup;
pub mod privilege;
pub mod profile;
pub mod restart;
pub mod service;
pub mod start;
pub mod status;
pub mod stop;
pub mod tun;

use std::path::PathBuf;

use crate::mihomo_manager::manager::MihomoManager;
use clash_verge_core::config::IClashTemp;

/// Build a MihomoManager wired with config from the standalone config dir.
///
/// Honors `verge.yaml`'s `proxy_core` selection (`mihomo` or `singbox`) so
/// the CLI matches the GUI's core switch: a stored `singbox` value makes the
/// manager resolve and spawn the sing-box core instead of verge-mihomo.
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

    let mut manager = MihomoManager::new(config_dir)
        .with_socket(socket_path)
        .with_secret(secret);

    // proxy_core: "singbox" switches the managed core from verge-mihomo to
    // sing-box. IVerge validates the value (unknown degrades to mihomo).
    let verge = clash_verge_core::config::IVerge::new().await;
    if verge.get_valid_proxy_core() == "singbox" {
        manager = manager.with_core_kind(crate::mihomo_manager::CoreKind::SingBox);
    }

    Ok(manager)
}
