mod app;
mod chain;
mod cli;
mod commands;
mod config_dir;
mod i18n;
mod mihomo_api;
mod mihomo_manager;
mod profile_store;
mod subscribe;
mod tui;
mod ui;

use clap::Parser as _;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    color_eyre::install().map_err(|e| anyhow::anyhow!("color-eyre install failed: {e}"))?;

    let cli = cli::Cli::parse();
    let config_dir = config_dir::resolve(cli.config_dir)?;
    clash_verge_core::utils::dirs::set_app_home_dir(config_dir.clone());

    match cli.command {
        None => tui::run(config_dir).await?,
        Some(cli::Command::Start) => {
            let manager = commands::build_manager(config_dir).await?;
            commands::start::run(manager).await?;
        }
        Some(cli::Command::Stop) => {
            let manager = commands::build_manager(config_dir).await?;
            commands::stop::run(manager).await?;
        }
        Some(cli::Command::Restart) => {
            let manager = commands::build_manager(config_dir).await?;
            commands::restart::run(manager).await?;
        }
        Some(cli::Command::Status { json }) => {
            let manager = commands::build_manager(config_dir).await?;
            let code = commands::status::run(manager, json).await?;
            std::process::exit(code);
        }
        Some(cli::Command::Profile { action }) => match action {
            cli::ProfileCommand::List => commands::profile::list().await?,
            cli::ProfileCommand::Import { url, name } => {
                commands::profile::import(&url, name.as_deref()).await?;
            }
            cli::ProfileCommand::Update { uid, all } => {
                commands::profile::update(uid.as_deref(), all).await?;
            }
        },
    }

    Ok(())
}
