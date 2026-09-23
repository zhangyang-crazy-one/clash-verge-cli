//! Diagnostic logging for the CLI's own `tracing` events.
//!
//! Where logs go depends on how the binary runs:
//! - interactive TUI: a daily file under `<config-dir>/logs/` (stderr would
//!   corrupt the alternate screen); `log_cleanup` prunes old files;
//! - `start --foreground` (systemd): stderr, collected by journald;
//! - one-shot commands: stderr, warnings and errors only by default.
//!
//! `-v`/`-vv`/`-vvv` raise the level to info/debug/trace, and `RUST_LOG`
//! (e.g. `info,mihomo=debug`) overrides both when it parses.

use std::path::{Path, PathBuf};

use tracing_subscriber::filter::{LevelFilter, Targets};
use tracing_subscriber::prelude::*;

/// Target used for lines the mihomo child process prints on stdout/stderr.
/// Kept separate from `mihomo` (download/resolve events) so the busy
/// per-connection output can be filtered on its own.
pub const MIHOMO_OUTPUT_TARGET: &str = "mihomo_core";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Tui,
    Daemon,
    OneShot,
}

impl Mode {
    pub fn for_command(command: Option<&crate::cli::Command>) -> Self {
        match command {
            None => Self::Tui,
            Some(crate::cli::Command::Start { foreground: true }) => Self::Daemon,
            Some(_) => Self::OneShot,
        }
    }

    const fn default_level(self) -> LevelFilter {
        match self {
            Self::Tui | Self::Daemon => LevelFilter::INFO,
            Self::OneShot => LevelFilter::WARN,
        }
    }
}

/// Install the global subscriber. Never fails the program: if the log file
/// cannot be opened the TUI simply runs without a log file.
pub fn init(mode: Mode, verbose: u8, config_dir: &Path) {
    let filter = build_filter(mode, verbose, std::env::var("RUST_LOG").ok().as_deref());
    let result = match mode {
        Mode::Tui => {
            let Some(file) = open_log_file(config_dir) else {
                return;
            };
            tracing_subscriber::registry()
                .with(
                    tracing_subscriber::fmt::layer()
                        .with_ansi(false)
                        .with_writer(std::sync::Mutex::new(file)),
                )
                .with(filter)
                .try_init()
        }
        Mode::Daemon | Mode::OneShot => tracing_subscriber::registry()
            .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
            .with(filter)
            .try_init(),
    };
    // Only fails when a subscriber is already installed (tests).
    let _ = result;
}

fn build_filter(mode: Mode, verbose: u8, rust_log: Option<&str>) -> Targets {
    if let Some(filter) = rust_log
        .map(str::trim)
        .filter(|spec| !spec.is_empty())
        .and_then(|spec| spec.parse::<Targets>().ok())
    {
        return filter;
    }
    let level = match verbose {
        0 => mode.default_level(),
        1 => LevelFilter::INFO.max(mode.default_level()),
        2 => LevelFilter::DEBUG,
        _ => LevelFilter::TRACE,
    };
    let mut filter = Targets::new().with_default(level);
    // The TUI already shows mihomo's own output in the Logs view; keep the
    // file to the CLI's events unless the user asked for more.
    if mode == Mode::Tui && verbose == 0 {
        filter = filter.with_target(MIHOMO_OUTPUT_TARGET, LevelFilter::WARN);
    }
    filter
}

fn log_file_path(config_dir: &Path) -> PathBuf {
    let date = chrono::Local::now().format("%Y-%m-%d");
    config_dir.join("logs").join(format!("clash-verge-cli-{date}.log"))
}

fn open_log_file(config_dir: &Path) -> Option<std::fs::File> {
    let path = log_file_path(config_dir);
    std::fs::create_dir_all(path.parent()?).ok()?;
    std::fs::OpenOptions::new().create(true).append(true).open(path).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing::Level;

    #[test]
    fn one_shot_commands_default_to_warnings_only() {
        let filter = build_filter(Mode::OneShot, 0, None);
        assert!(filter.would_enable("mihomo", &Level::WARN));
        assert!(!filter.would_enable("mihomo", &Level::INFO));
    }

    #[test]
    fn verbose_flags_raise_the_level() {
        assert!(build_filter(Mode::OneShot, 1, None).would_enable("x", &Level::INFO));
        assert!(!build_filter(Mode::OneShot, 1, None).would_enable("x", &Level::DEBUG));
        assert!(build_filter(Mode::OneShot, 2, None).would_enable("x", &Level::DEBUG));
        assert!(build_filter(Mode::OneShot, 3, None).would_enable("x", &Level::TRACE));
    }

    #[test]
    fn tui_file_keeps_cli_events_and_quiets_mihomo_output() {
        let filter = build_filter(Mode::Tui, 0, None);
        assert!(filter.would_enable("auto_update", &Level::INFO));
        assert!(filter.would_enable("mihomo", &Level::INFO));
        assert!(!filter.would_enable(MIHOMO_OUTPUT_TARGET, &Level::INFO));
        assert!(filter.would_enable(MIHOMO_OUTPUT_TARGET, &Level::WARN));
        // -v brings the core's output back.
        assert!(build_filter(Mode::Tui, 1, None).would_enable(MIHOMO_OUTPUT_TARGET, &Level::INFO));
    }

    #[test]
    fn daemon_logs_info_including_mihomo_output() {
        let filter = build_filter(Mode::Daemon, 0, None);
        assert!(filter.would_enable(MIHOMO_OUTPUT_TARGET, &Level::INFO));
        assert!(!filter.would_enable("x", &Level::DEBUG));
    }

    #[test]
    fn rust_log_overrides_mode_and_verbosity() {
        let filter = build_filter(Mode::OneShot, 0, Some("warn,auto_update=debug"));
        assert!(filter.would_enable("auto_update", &Level::DEBUG));
        assert!(!filter.would_enable("mihomo", &Level::INFO));
        // Blank values fall back to the defaults.
        assert!(!build_filter(Mode::OneShot, 0, Some("  ")).would_enable("x", &Level::INFO));
    }

    #[test]
    fn log_file_lives_under_config_logs_dir_with_a_daily_name() {
        let path = log_file_path(Path::new("/tmp/cv-home"));
        assert!(path.starts_with("/tmp/cv-home/logs"));
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default();
        assert!(
            name.starts_with("clash-verge-cli-") && name.ends_with(".log"),
            "got {name}"
        );
    }
}
