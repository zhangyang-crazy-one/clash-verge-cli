pub mod event_loop;
pub mod input;
pub mod terminal_guard;

pub use terminal_guard::TerminalGuard;

use std::path::PathBuf;

pub async fn run(config_dir: PathBuf) -> anyhow::Result<()> {
    clash_verge_core::utils::dirs::set_app_home_dir(config_dir.clone());
    event_loop::run(config_dir).await
}
