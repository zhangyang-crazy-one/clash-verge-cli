use crate::mihomo_manager::manager::MihomoManager;

pub async fn run(manager: MihomoManager) -> anyhow::Result<()> {
    let Some(pid) = manager.pid() else {
        // Still release a desktop proxy left pointing at a dead core.
        manager.stop().await?;
        if super::core_running(&manager.api()).await {
            anyhow::bail!(
                "a core answers on {} but was not started by clash-verge-cli (no pid record); \
stop it where it was started",
                manager.socket_path().display()
            );
        }
        println!("mihomo is not running");
        return Ok(());
    };
    manager.stop().await?;
    println!("mihomo stopped (pid {pid})");
    Ok(())
}
