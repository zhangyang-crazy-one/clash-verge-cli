use clap::Parser;
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(name = "clash-verge-cli", version, about = "Terminal-native proxy client for mihomo")]
pub struct Cli {
    #[arg(long, value_name = "PATH")]
    pub config_dir: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(clap::Subcommand, Debug)]
pub enum Command {
    /// Start the mihomo core (non-interactive)
    Start {
        /// Run in foreground, blocking until SIGTERM (for systemd)
        #[arg(long)]
        foreground: bool,
    },
    /// Stop the mihomo core
    Stop,
    /// Restart the mihomo core
    Restart,
    /// Show mihomo status
    Status {
        /// Output machine-readable JSON
        #[arg(long)]
        json: bool,
    },
    /// Manage subscription profiles
    Profile {
        #[command(subcommand)]
        action: ProfileCommand,
    },
    /// Manage systemd daemon service
    Service {
        #[command(subcommand)]
        action: ServiceCommand,
    },
    /// Manage TUN privileges for the resolved mihomo binary
    Tun {
        #[command(subcommand)]
        action: TunCommand,
    },
    /// Internal: sudo askpass helper (SUDO_ASKPASS target).
    #[command(hide = true)]
    Askpass,
}

#[derive(clap::Subcommand, Debug)]
pub enum ProfileCommand {
    /// List subscription profiles
    List,
    /// Import a subscription URL
    Import {
        /// Subscription URL (http/https)
        url: String,
        /// Optional display name
        #[arg(long)]
        name: Option<String>,
        /// Auto-refresh interval in minutes (persisted as `option.update_interval`)
        #[arg(long, value_name = "MINUTES")]
        update_interval: Option<u64>,
        /// Disable automatic refresh for this profile
        #[arg(long)]
        no_auto_update: bool,
    },
    /// Update one remote profile or all of them
    Update {
        /// Profile UID to update
        uid: Option<String>,
        /// Update every remote profile
        #[arg(long)]
        all: bool,
    },
    /// Delete a profile by UID (including its chain fragments)
    Delete { uid: String },
    /// Rename a profile
    Rename { uid: String, new_name: String },
}

#[derive(clap::Subcommand, Debug)]
pub enum TunCommand {
    /// Grant TUN capabilities to the mihomo binary (one-time sudo; the only
    /// explicit privilege operation)
    Setup,
    /// Show the TUN capability state of the resolved mihomo binary
    Status,
}

#[derive(clap::Subcommand, Debug)]
pub enum ServiceCommand {
    /// Install the systemd service unit
    Install {
        /// Also start the service immediately
        #[arg(long)]
        now: bool,
    },
    /// Stop and remove the systemd service unit
    Uninstall,
    /// Show service active/enabled status
    Status {
        /// Output machine-readable JSON
        #[arg(long)]
        json: bool,
    },
}
