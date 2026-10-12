use crate::mihomo_manager::manager::MihomoManager;

/// `clash-verge-cli restart`: stop the core that is **running** (its kind
/// comes from the pid record, so a `proxy_core` edited mid-run no longer
/// hides it — #56), then start a fresh one of the core `verge.yaml` now
/// selects.
pub async fn run(manager: MihomoManager) -> anyhow::Result<()> {
    let running_kind = manager.core_kind();
    let config_dir = manager.config_dir().clone();
    let target = super::configured_core_kind().await;
    let replacement = if target == running_kind {
        manager.clone()
    } else {
        super::build_manager_for_kind(config_dir, target).await?
    };
    // #75: reject the new Mihomo candidate while the existing core is alive,
    // then bind the replacement supervisor to the exact checked artifact.
    let mode = replacement
        .prepare_mihomo_launch(super::start::SupervisorLaunch::Regenerate)
        .await?;
    if let Some(pid) = manager.pid() {
        manager.stop().await?;
        println!("{} stopped (pid {pid})", running_kind.as_str());
    }
    // The replacement follows the configured selection, which may differ
    // from the core that was just stopped: that is how a core switch takes
    // effect without the user having to stop the old core by hand.
    let replacement = if target == running_kind {
        manager
    } else {
        println!(
            "{} selected; starting it in place of {}",
            target.as_str(),
            running_kind.as_str()
        );
        replacement
    };
    super::start::run_with_mode(replacement, mode).await
}
