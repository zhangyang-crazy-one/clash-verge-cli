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
    Start,
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
}

#[derive(clap::Subcommand, Debug)]
pub enum ProfileCommand {
    /// List remote subscription profiles
    List,
    /// Import a subscription URL
    Import {
        /// Subscription URL (http/https)
        url: String,
        /// Optional display name
        #[arg(long)]
        name: Option<String>,
    },
    /// Update one remote profile or all of them
    Update {
        /// Profile UID to update
        uid: Option<String>,
        /// Update every remote profile
        #[arg(long)]
        all: bool,
    },
}
