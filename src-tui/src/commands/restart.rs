use crate::mihomo_manager::manager::MihomoManager;

pub async fn run(manager: MihomoManager) -> anyhow::Result<()> {
    manager.restart().await?;
    super::wait_until_ready(&manager, super::start::READY_TIMEOUT).await?;
    println!("mihomo restarted (pid {})", manager.pid().unwrap_or(0));
    Ok(())
}
