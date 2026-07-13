use crate::mihomo_manager::manager::MihomoManager;

pub async fn run(manager: MihomoManager) -> anyhow::Result<()> {
    manager.stop().await?;
    println!("mihomo stopped");
    Ok(())
}
