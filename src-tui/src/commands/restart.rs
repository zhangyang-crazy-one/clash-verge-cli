use crate::mihomo_manager::manager::MihomoManager;

/// `clash-verge-cli restart`: stop the core that is **running** (its kind
/// comes from the pid record, so a `proxy_core` edited mid-run no longer
/// hides it — #56), then start a fresh one of the core `verge.yaml` now
/// selects.
pub async fn run(manager: MihomoManager) -> anyhow::Result<()> {
    let running_kind = manager.core_kind();
    let config_dir = manager.config_dir().clone();
    if let Some(pid) = manager.pid() {
        manager.stop().await?;
        println!("{} stopped (pid {pid})", running_kind.as_str());
    }
    // The replacement follows the configured selection, which may differ
    // from the core that was just stopped: that is how a core switch takes
    // effect without the user having to stop the old core by hand.
    let target = super::configured_core_kind().await;
    let replacement = if target == running_kind {
        manager
    } else {
        println!(
            "{} selected; starting it in place of {}",
            target.as_str(),
            running_kind.as_str()
        );
        super::build_manager_for_kind(config_dir, target).await?
    };
    super::start::run(replacement).await
}
