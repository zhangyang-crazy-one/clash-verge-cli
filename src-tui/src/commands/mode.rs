//! `mode [rule|global|direct]`.

use crate::cli::ClashMode;
use crate::mihomo_manager::manager::MihomoManager;
use crate::services::mode::{apply_clash_mode, current_mode};

pub async fn run(manager: &MihomoManager, mode: Option<ClashMode>) -> anyhow::Result<()> {
    let api = manager.api();
    let running = super::core_running(&api).await;
    match mode {
        None => println!("{}", current_mode(&api, running).await),
        Some(mode) => {
            let applied = apply_clash_mode(&api, mode.as_str(), running)
                .await
                .map_err(|error| anyhow::anyhow!("failed to set mode: {error}"))?;
            if running {
                println!("mode: {applied}");
            } else {
                println!("mode: {applied} (core not running; used on next start)");
            }
        }
    }
    Ok(())
}
