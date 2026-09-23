use crate::mihomo_manager::manager::MihomoManager;

/// `clash-verge-cli restart`: stop the recorded core (its supervisor exits
/// with it), then start a fresh supervised one.
pub async fn run(manager: MihomoManager) -> anyhow::Result<()> {
    if let Some(pid) = manager.pid() {
        manager.stop().await?;
        println!("mihomo stopped (pid {pid})");
    }
    super::start::run(manager).await
}
