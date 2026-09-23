mod app;
mod chain;
mod cli;
mod commands;
mod config_dir;
mod editor;
mod enhance;
mod exit;
mod i18n;
mod logging;
mod mihomo_api;
mod mihomo_manager;
mod profile_store;
mod runtime_config;
mod service_cmd;
mod services;
mod subscribe;
mod sys_proxy;
mod tui;
mod ui;

use clap::Parser as _;

#[tokio::main]
async fn main() {
    let code = match run().await {
        Ok(code) => code,
        Err(error) => {
            eprintln!("Error: {error:?}");
            exit::code_for(&error)
        }
    };
    std::process::exit(code);
}

/// Run the command line; `Ok` carries the exit code (see [`exit`]).
async fn run() -> anyhow::Result<i32> {
    color_eyre::install().map_err(|e| anyhow::anyhow!("color-eyre install failed: {e}"))?;

    // `sudo -A` runs the SUDO_ASKPASS program (us, during `tun setup`) with
    // the prompt text as its argument, which is not a valid subcommand: detect
    // it before clap parses anything, and before any config resolution (the
    // environment is sudo's, maybe without a config dir).
    if is_askpass_invocation() {
        commands::askpass::run()?;
        return Ok(exit::SUCCESS);
    }
    let cli = cli::Cli::parse();
    // Commands that need no configuration at all.
    match &cli.command {
        Some(cli::Command::Askpass) => {
            commands::askpass::run()?;
            return Ok(exit::SUCCESS);
        }
        Some(cli::Command::Completions { shell }) => {
            commands::docs::completions(*shell)?;
            return Ok(exit::SUCCESS);
        }
        Some(cli::Command::Man { dir }) => {
            commands::docs::man(dir.as_deref())?;
            return Ok(exit::SUCCESS);
        }
        _ => {}
    }
    let json = cli.json;
    let config_dir = config_dir::resolve(cli.config_dir)?;
    clash_verge_core::utils::dirs::set_app_home_dir(config_dir.clone());
    logging::init(
        logging::Mode::for_command(cli.command.as_ref()),
        cli.verbose,
        &config_dir,
    );

    // Prune old log files where logs are written: the TUI and the long-running
    // supervisor (`start` launches one). One-shot commands stay read-only.
    if matches!(
        logging::Mode::for_command(cli.command.as_ref()),
        logging::Mode::Tui | logging::Mode::Daemon
    ) {
        let verge = clash_verge_core::config::IVerge::new().await;
        commands::log_cleanup::run(&config_dir, verge.auto_log_clean).await;
    }

    match cli.command {
        None => tui::run(config_dir).await?,
        Some(cli::Command::Askpass | cli::Command::Completions { .. } | cli::Command::Man { .. }) => {
            unreachable!("handled before config resolution")
        }
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
        Some(cli::Command::Status { wait, timeout }) => {
            let manager = commands::build_manager(config_dir).await?;
            let wait = wait.then(|| std::time::Duration::from_secs(timeout));
            return commands::status::run(manager, json, wait).await;
        }
        Some(cli::Command::Profile { action }) => match action {
            cli::ProfileCommand::List => commands::profile::list(json).await?,
            cli::ProfileCommand::Import {
                url,
                name,
                update_interval,
                no_auto_update,
            } => {
                commands::profile::import(&url, name.as_deref(), update_interval, no_auto_update).await?;
            }
            cli::ProfileCommand::Use { profile } => {
                let manager = commands::build_manager(config_dir).await?;
                commands::profile::use_profile(&manager, &profile).await?;
            }
            cli::ProfileCommand::Update { profile, all, reload } => {
                let manager = commands::build_manager(config_dir).await?;
                commands::profile::update(&manager, profile.as_deref(), all, reload).await?;
            }
            cli::ProfileCommand::Delete { profile } => {
                commands::profile::delete(&profile).await?;
            }
            cli::ProfileCommand::Rename { profile, new_name } => {
                commands::profile::rename(&profile, &new_name).await?;
            }
            cli::ProfileCommand::Migrate { from, force } => {
                commands::profile::migrate(&from, force).await?;
            }
        },
        Some(cli::Command::Service { action }) => match action {
            cli::ServiceCommand::Install { now } => {
                // The unit must name the exact files: a lossy conversion
                // would write a different path into ExecStart.
                let bin = std::env::current_exe()?;
                let bin = bin
                    .to_str()
                    .ok_or_else(|| anyhow::anyhow!("executable path is not valid UTF-8: {}", bin.display()))?;
                let config = config_dir
                    .to_str()
                    .ok_or_else(|| anyhow::anyhow!("config dir is not valid UTF-8: {}", config_dir.display()))?;
                commands::service::install(bin, config, now)?;
            }
            cli::ServiceCommand::Uninstall => commands::service::uninstall()?,
            cli::ServiceCommand::Status => commands::service::status(json)?,
        },
        Some(cli::Command::Proxy { action }) => {
            let manager = commands::build_manager(config_dir).await?;
            match action {
                cli::ProxyCommand::List { group } => {
                    commands::proxy::list(&manager, group.as_deref(), json).await?;
                }
                cli::ProxyCommand::Select { group, node } => commands::proxy::select(&manager, &group, &node).await?,
                cli::ProxyCommand::Delay { target, url, timeout } => {
                    commands::proxy::delay(&manager, &target, &url, timeout).await?;
                }
            }
        }
        Some(cli::Command::Mode { mode }) => {
            let manager = commands::build_manager(config_dir).await?;
            commands::mode::run(&manager, mode, json).await?;
        }
        Some(cli::Command::Connections { action }) => {
            let manager = commands::build_manager(config_dir).await?;
            match action.unwrap_or(cli::ConnectionsCommand::List) {
                cli::ConnectionsCommand::List => commands::connections::list(&manager, json).await?,
                cli::ConnectionsCommand::Close { id } => commands::connections::close(&manager, &id).await?,
                cli::ConnectionsCommand::CloseAll => commands::connections::close_all(&manager).await?,
            }
        }
        Some(cli::Command::Provider { action }) => {
            let manager = commands::build_manager(config_dir).await?;
            match action {
                cli::ProviderCommand::List => commands::provider::list(&manager, json).await?,
                cli::ProviderCommand::Update { name, all } => {
                    commands::provider::update(&manager, name.as_deref(), all).await?;
                }
            }
        }
        Some(cli::Command::Sysproxy { action }) => match action {
            cli::SysproxyCommand::On => {
                let manager = commands::build_manager(config_dir).await?;
                commands::sysproxy::on(&manager).await?;
            }
            cli::SysproxyCommand::Off => commands::sysproxy::off().await?,
            cli::SysproxyCommand::Status => {
                let manager = commands::build_manager(config_dir).await?;
                commands::sysproxy::status(&manager, json).await?;
            }
            cli::SysproxyCommand::Env { unset } => commands::sysproxy::env(unset).await,
        },
        Some(cli::Command::Tun { action }) => match action {
            cli::TunCommand::On | cli::TunCommand::Off => {
                let manager = commands::build_manager(config_dir).await?;
                commands::tun::set_enabled(&manager, matches!(action, cli::TunCommand::On)).await?;
            }
            cli::TunCommand::Setup => commands::tun::setup().await?,
            cli::TunCommand::Status => commands::tun::status(json).await?,
        },
    }

    Ok(exit::SUCCESS)
}

/// Whether `sudo -A` started us as its askpass helper: sudo points
/// SUDO_ASKPASS at this executable for `tun setup`.
fn is_askpass_invocation() -> bool {
    askpass_target_is(
        std::env::var_os("SUDO_ASKPASS").as_deref(),
        std::env::current_exe().ok().as_deref(),
    )
}

fn askpass_target_is(askpass: Option<&std::ffi::OsStr>, exe: Option<&std::path::Path>) -> bool {
    matches!((askpass, exe), (Some(askpass), Some(exe)) if std::path::Path::new(askpass) == exe)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;
    use std::path::Path;

    #[test]
    fn askpass_needs_sudo_askpass_to_name_this_executable() {
        let exe = Path::new("/usr/bin/clash-verge-cli");
        assert!(askpass_target_is(
            Some(OsStr::new("/usr/bin/clash-verge-cli")),
            Some(exe)
        ));
        assert!(!askpass_target_is(Some(OsStr::new("/usr/bin/ssh-askpass")), Some(exe)));
        assert!(!askpass_target_is(None, Some(exe)));
        // An empty variable must not match an unknown executable path.
        assert!(!askpass_target_is(Some(OsStr::new("")), None));
    }
}
