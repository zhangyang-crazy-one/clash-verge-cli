mod app;
mod chain;
mod cli;
mod commands;
mod config_dir;
mod editor;
mod enhance;
mod i18n;
mod mihomo_api;
mod mihomo_manager;
mod profile_store;
mod runtime_config;
mod service_cmd;
mod subscribe;
mod sys_proxy;
mod tui;
mod ui;

use clap::Parser as _;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    color_eyre::install().map_err(|e| anyhow::anyhow!("color-eyre install failed: {e}"))?;

    let cli = cli::Cli::parse();
    let config_dir = config_dir::resolve(cli.config_dir)?;
    clash_verge_core::utils::dirs::set_app_home_dir(config_dir.clone());

    // Non-fatal: clean old log files on startup.
    let gui = clash_verge_core::config::IVerge::new().await;
    commands::log_cleanup::run(&config_dir, gui.auto_log_clean).await;

    match cli.command {
        None => tui::run(config_dir).await?,
        Some(cli::Command::Start { foreground }) => {
            if foreground {
                commands::daemon::run(config_dir).await?;
            } else {
                let manager = commands::build_manager(config_dir).await?;
                commands::start::run(manager).await?;
            }
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
            cli::ProfileCommand::Import {
                url,
                name,
                update_interval,
                no_auto_update,
            } => {
                commands::profile::import(&url, name.as_deref(), update_interval, no_auto_update).await?;
            }
            cli::ProfileCommand::Update { uid, all } => {
                commands::profile::update(uid.as_deref(), all).await?;
            }
            cli::ProfileCommand::Delete { uid } => {
                commands::profile::delete(&uid).await?;
            }
            cli::ProfileCommand::Rename { uid, new_name } => {
                commands::profile::rename(&uid, &new_name).await?;
            }
        },
        Some(cli::Command::Service { action }) => {
            let bin = std::env::current_exe()?;
            let bin_str = bin.to_string_lossy();
            let config_str = config_dir.to_string_lossy();
            match action {
                cli::ServiceCommand::Install { now } => {
                    commands::service::install(&bin_str, &config_str, now)?;
                }
                cli::ServiceCommand::Uninstall => {
                    commands::service::uninstall()?;
                }
                cli::ServiceCommand::Status { json } => {
                    commands::service::status(json)?;
                }
            }
        }
    }

    Ok(())
}
