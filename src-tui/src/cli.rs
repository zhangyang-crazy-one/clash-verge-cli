use clap::Parser;
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(name = "clash-verge-cli", version, about = "Terminal-native proxy client for mihomo")]
pub struct Cli {
    /// Configuration directory (default: ~/.local/share/clash-verge-cli).
    /// Accepted before or after the subcommand; the systemd unit passes it
    /// after `start --foreground`.
    #[arg(long, value_name = "PATH", global = true)]
    pub config_dir: Option<PathBuf>,

    /// Increase log verbosity (-v info, -vv debug, -vvv trace). `RUST_LOG`
    /// overrides it. The TUI logs to <config-dir>/logs/, other commands to
    /// stderr.
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    pub verbose: u8,

    /// Machine-readable JSON output for list and status commands.
    #[arg(long, global = true)]
    pub json: bool,

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
    /// Show mihomo status (exit 0 running, 3 not running, 1 error)
    Status {
        /// Wait until the core is running (exit 3 on timeout)
        #[arg(long)]
        wait: bool,
        /// Seconds `--wait` waits
        #[arg(long, value_name = "SECS", default_value_t = 15, requires = "wait")]
        timeout: u64,
    },
    /// Manage subscription profiles
    Profile {
        #[command(subcommand)]
        action: ProfileCommand,
    },
    /// Show or select proxies and test their delay (core must be running)
    Proxy {
        #[command(subcommand)]
        action: ProxyCommand,
    },
    /// Show or set the routing mode
    Mode {
        /// New mode; omit to print the current one
        #[arg(value_enum)]
        mode: Option<ClashMode>,
    },
    /// List or close active connections (core must be running)
    Connections {
        #[command(subcommand)]
        action: Option<ConnectionsCommand>,
    },
    /// List or update rule providers (core must be running)
    Provider {
        #[command(subcommand)]
        action: ProviderCommand,
    },
    /// Manage systemd daemon service
    Service {
        #[command(subcommand)]
        action: ServiceCommand,
    },
    /// Turn the desktop system proxy on or off, or print shell exports
    Sysproxy {
        #[command(subcommand)]
        action: SysproxyCommand,
    },
    /// Turn TUN mode on or off, or manage its one-time privileges
    Tun {
        #[command(subcommand)]
        action: TunCommand,
    },
    /// Print a shell completion script: `clash-verge-cli completions bash`
    Completions {
        #[arg(value_enum)]
        shell: clap_complete::Shell,
    },
    /// Print the man page, or write pages for every command into DIR
    Man {
        #[arg(long, value_name = "DIR")]
        dir: Option<PathBuf>,
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
    /// Make a profile current and apply it
    Use {
        /// Profile uid or name
        profile: String,
    },
    /// Update one remote profile or all of them
    Update {
        /// Profile uid or name
        #[arg(required_unless_present = "all", conflicts_with = "all")]
        profile: Option<String>,
        /// Update every remote profile
        #[arg(long)]
        all: bool,
        /// Reload the running core when the current profile changed
        #[arg(long)]
        reload: bool,
    },
    /// Delete a profile (including its chain fragments)
    Delete {
        /// Profile uid or name
        profile: String,
    },
    /// Rename a profile
    Rename {
        /// Profile uid or name
        profile: String,
        new_name: String,
    },
    /// One-shot import of a Clash Verge GUI profile set (subscriptions,
    /// chain fragments, settings) into the standalone directory
    Migrate {
        /// Source GUI config directory (e.g. ~/.local/share/io.github.clash-verge-rev.clash-verge-rev)
        #[arg(long, value_name = "DIR")]
        from: std::path::PathBuf,
        /// Overwrite an existing standalone profile set
        #[arg(long)]
        force: bool,
    },
}

#[derive(clap::Subcommand, Debug)]
pub enum ProxyCommand {
    /// List proxy groups, or the members of one group
    List {
        /// Group to list members of
        group: Option<String>,
    },
    /// Select a member of a selector group
    Select { group: String, node: String },
    /// Test the delay of one proxy, or of every real proxy in a group
    Delay {
        /// Proxy or group name
        target: String,
        /// URL the test requests
        #[arg(long, default_value = crate::services::proxy::DELAY_TEST_URL)]
        url: String,
        /// Timeout in milliseconds
        #[arg(long, value_name = "MS", default_value_t = crate::services::proxy::DELAY_TEST_TIMEOUT_MS)]
        timeout: u64,
    },
}

#[derive(clap::ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClashMode {
    Rule,
    Global,
    Direct,
}

impl ClashMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Rule => "rule",
            Self::Global => "global",
            Self::Direct => "direct",
        }
    }
}

#[derive(clap::Subcommand, Debug)]
pub enum ConnectionsCommand {
    /// List active connections (the default)
    List,
    /// Close one connection by id
    Close { id: String },
    /// Close every connection
    CloseAll,
}

#[derive(clap::Subcommand, Debug)]
pub enum ProviderCommand {
    /// List rule providers
    List,
    /// Update one rule provider or all of them
    Update {
        #[arg(required_unless_present = "all", conflicts_with = "all")]
        name: Option<String>,
        /// Update every rule provider
        #[arg(long)]
        all: bool,
    },
}

#[derive(clap::Subcommand, Debug)]
pub enum SysproxyCommand {
    /// Enable the system proxy (applied now if the core runs, else on start)
    On,
    /// Disable the system proxy and restore the previous desktop settings
    Off,
    /// Show the system proxy setting and whether the desktop uses it
    Status,
    /// Print shell exports for the proxy: eval "$(clash-verge-cli sysproxy env)"
    Env {
        /// Print `unset` commands instead
        #[arg(long)]
        unset: bool,
    },
}

#[derive(clap::Subcommand, Debug)]
pub enum TunCommand {
    /// Enable TUN mode (reloads the running core)
    On,
    /// Disable TUN mode (reloads the running core)
    Off,
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
    Status,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(std::iter::once("clash-verge-cli").chain(args.iter().copied()))
    }

    #[test]
    fn cli_definition_is_consistent() {
        use clap::CommandFactory as _;
        Cli::command().debug_assert();
    }

    #[test]
    fn parses_the_new_commands() {
        assert!(matches!(
            parse(&["profile", "use", "Home"]).unwrap().command,
            Some(Command::Profile { action: ProfileCommand::Use { profile } }) if profile == "Home"
        ));
        assert!(matches!(
            parse(&["proxy", "select", "Proxy", "Tokyo"]).unwrap().command,
            Some(Command::Proxy { action: ProxyCommand::Select { group, node } }) if group == "Proxy" && node == "Tokyo"
        ));
        assert!(matches!(
            parse(&["proxy", "delay", "Proxy", "--timeout", "800"]).unwrap().command,
            Some(Command::Proxy {
                action: ProxyCommand::Delay { timeout: 800, .. }
            })
        ));
        assert!(matches!(
            parse(&["mode", "global"]).unwrap().command,
            Some(Command::Mode {
                mode: Some(ClashMode::Global)
            })
        ));
        assert!(matches!(
            parse(&["mode"]).unwrap().command,
            Some(Command::Mode { mode: None })
        ));
        assert!(matches!(
            parse(&["connections"]).unwrap().command,
            Some(Command::Connections { action: None })
        ));
        assert!(matches!(
            parse(&["connections", "close-all"]).unwrap().command,
            Some(Command::Connections {
                action: Some(ConnectionsCommand::CloseAll)
            })
        ));
        assert!(matches!(
            parse(&["provider", "update", "--all"]).unwrap().command,
            Some(Command::Provider {
                action: ProviderCommand::Update { name: None, all: true }
            })
        ));
        assert!(matches!(
            parse(&["sysproxy", "on"]).unwrap().command,
            Some(Command::Sysproxy {
                action: SysproxyCommand::On
            })
        ));
        assert!(matches!(
            parse(&["tun", "off"]).unwrap().command,
            Some(Command::Tun {
                action: TunCommand::Off
            })
        ));
    }

    #[test]
    fn config_dir_is_accepted_before_and_after_the_subcommand() {
        for args in [
            &["--config-dir", "/cfg", "status"][..],
            &["status", "--config-dir", "/cfg"][..],
            &["start", "--foreground", "--config-dir", "/cfg"][..],
        ] {
            let cli = parse(args).unwrap_or_else(|error| panic!("{args:?}: {error}"));
            assert_eq!(
                cli.config_dir.as_deref(),
                Some(std::path::Path::new("/cfg")),
                "{args:?}"
            );
        }
    }

    #[test]
    fn the_generated_systemd_unit_command_parses() {
        // `service install` writes this ExecStart; it must be a valid command
        // line for this CLI or the service can never start.
        let unit = crate::service_cmd::unit_content("/usr/bin/clash-verge-cli", "/home/u/cfg");
        let exec = unit
            .lines()
            .find_map(|line| line.strip_prefix("ExecStart="))
            .expect("unit has an ExecStart line");
        let cli = parse(&exec.split_whitespace().skip(1).collect::<Vec<_>>()).expect("ExecStart parses");
        assert!(matches!(cli.command, Some(Command::Start { foreground: true })));
        assert_eq!(cli.config_dir.as_deref(), Some(std::path::Path::new("/home/u/cfg")));
    }

    #[test]
    fn json_is_global_and_status_can_wait() {
        let cli = parse(&["proxy", "list", "Proxy", "--json"]).unwrap();
        assert!(cli.json);
        assert!(parse(&["--json", "connections"]).unwrap().json);
        assert!(matches!(
            parse(&["status", "--wait", "--timeout", "5"]).unwrap().command,
            Some(Command::Status { wait: true, timeout: 5 })
        ));
        assert!(parse(&["status", "--timeout", "5"]).is_err(), "--timeout needs --wait");
        assert!(matches!(
            parse(&["completions", "zsh"]).unwrap().command,
            Some(Command::Completions {
                shell: clap_complete::Shell::Zsh
            })
        ));
    }

    #[test]
    fn rejects_invalid_combinations() {
        assert!(parse(&["mode", "script"]).is_err());
        assert!(parse(&["profile", "update"]).is_err(), "needs a profile or --all");
        assert!(parse(&["profile", "update", "R1", "--all"]).is_err());
        assert!(parse(&["provider", "update"]).is_err());
    }
}
