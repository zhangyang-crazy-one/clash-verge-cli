use crate::mihomo_manager::binary::MihomoBinarySource;
use crate::mihomo_manager::manager::{MihomoManager, core_log_path};

use super::wait_until_ready;

/// How long `start`/`restart` wait for the controller to answer.
pub const READY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

pub async fn run(manager: MihomoManager) -> anyhow::Result<()> {
    // TUN capability preflight happens inside the manager, after binary
    // resolution and before spawn (see `ManagerInner::spawn_and_watch`).
    // Daily start NEVER runs sudo/setcap/askpass: a missing capability
    // fails with an actionable error naming `tun setup` and the binary.
    let resolved = manager.start().await?;
    wait_until_ready(&manager, READY_TIMEOUT).await?;
    let pid = manager.pid().unwrap_or(0);
    let source = match resolved.source {
        MihomoBinarySource::System => "system verge-mihomo",
        MihomoBinarySource::ManagedCached => "managed (already installed)",
        MihomoBinarySource::Downloaded => "managed (downloaded just now)",
    };
    println!("mihomo started");
    println!("  version: {}", resolved.version);
    println!("  source:  {source}");
    println!("  path:    {}", resolved.path.display());
    println!("  pid:     {pid}");
    println!("  log:     {}", core_log_path(manager.config_dir()).display());
    Ok(())
}
