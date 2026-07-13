use crate::mihomo_manager::manager::MihomoManager;

pub async fn run(manager: MihomoManager) -> anyhow::Result<()> {
    manager.start().await?;
    println!("mihomo started (pid {})", manager.pid().map_or(0, |p| p));
    Ok(())
}
