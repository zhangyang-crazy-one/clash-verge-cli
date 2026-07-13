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
}
