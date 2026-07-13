use crate::mihomo_manager::manager::MihomoManager;

pub async fn run(manager: MihomoManager) -> anyhow::Result<()> {
    manager.restart().await?;
    println!("mihomo restarted");
    Ok(())
}
