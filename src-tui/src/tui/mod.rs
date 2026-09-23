pub mod event_loop;
mod handlers;
pub mod input;
pub mod keymap;
pub mod terminal_guard;

pub use terminal_guard::TerminalGuard;

use std::path::PathBuf;

/// Run the interactive TUI. `main` has already set the app home directory.
pub async fn run(config_dir: PathBuf) -> anyhow::Result<()> {
    event_loop::run(config_dir).await
}
