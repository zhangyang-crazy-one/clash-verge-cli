//! Login autostart: a systemd `--user` unit that launches the core in
//! foreground mode at login.
//!
//! The unit lives at `$XDG_CONFIG_HOME/systemd/user/clash-verge-cli.service`
//! (default `~/.config/systemd/user`). Enable writes the unit, runs
//! `systemctl --user daemon-reload`, then `systemctl --user enable` —
//! deliberately NO `--now`: the running TUI core must never be shadowed by
//! a second foreground core, so the unit only takes effect at the next login.
//! Disable runs `systemctl --user disable` and removes the unit.
//!
//! Headless / no-user-session environments fail loudly: `systemctl --user`
//! exits non-zero, and that error text is surfaced verbatim in the TUI
//! status bar.

use std::path::PathBuf;
use std::process::Command;

use crate::service_cmd::systemd_escape;

const UNIT_NAME: &str = "clash-verge-cli.service";

/// systemd --user unit directory for a given `XDG_CONFIG_HOME` (injectable
/// for tests); `None` falls back to the OS config dir, then `~/.config`.
fn unit_dir_from(config_home: Option<std::ffi::OsString>) -> PathBuf {
    let base = config_home
        .map(PathBuf::from)
        .or_else(dirs::config_dir)
        .unwrap_or_else(|| PathBuf::from("~/.config"));
    base.join("systemd").join("user")
}

/// systemd --user unit directory (`$XDG_CONFIG_HOME/systemd/user`).
pub fn unit_dir() -> PathBuf {
    unit_dir_from(std::env::var_os("XDG_CONFIG_HOME"))
}

/// Full path of the autostart unit file.
pub fn unit_path() -> PathBuf {
    unit_dir().join(UNIT_NAME)
}

/// User-unit content. The binary runs in foreground mode so systemd tracks
/// the PID properly; `WantedBy=default.target` starts the unit at login.
/// Both paths are systemd-escaped exactly like the system unit.
pub fn user_unit_content(binary_path: &str, config_dir: &str) -> String {
    format!(
        r#"[Unit]
Description=Clash Verge CLI - mihomo proxy daemon
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStart={} start --foreground --config-dir {}
ExecStop=/bin/kill -TERM $MAINPID
Restart=on-failure
RestartSec=5
LimitNOFILE=65536

[Install]
WantedBy=default.target
"#,
        systemd_escape(binary_path),
        systemd_escape(config_dir)
    )
}

/// Run a `systemctl --user` command; fail with its combined output on error
/// so headless/no-user-session failures read clearly.
fn run_systemctl(args: &[&str]) -> anyhow::Result<()> {
    let output = Command::new("systemctl")
        .arg("--user")
        .args(args)
        .output()
        .map_err(|error| anyhow::anyhow!("cannot run systemctl (is a systemd user session available?): {error}"))?;
    if !output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "systemctl --user {} failed: {} {}",
            args.join(" "),
            stdout.trim(),
            stderr.trim()
        );
    }
    Ok(())
}

/// Enable login autostart: write the unit, `daemon-reload`, then `enable`.
/// Deliberately no `--now`: the running TUI core keeps running; the unit
/// starts the core at the next login.
pub fn enable(binary_path: &str, config_dir: &str) -> anyhow::Result<()> {
    let dir = unit_dir();
    std::fs::create_dir_all(&dir).map_err(|error| anyhow::anyhow!("cannot create {}: {error}", dir.display()))?;
    std::fs::write(unit_path(), user_unit_content(binary_path, config_dir))
        .map_err(|error| anyhow::anyhow!("cannot write {}: {error}", unit_path().display()))?;
    run_systemctl(&["daemon-reload"])?;
    run_systemctl(&["enable", UNIT_NAME])?;
    Ok(())
}

/// Disable login autostart: `systemctl --user disable`, then remove the unit
/// file (already gone → success).
pub fn disable() -> anyhow::Result<()> {
    run_systemctl(&["disable", UNIT_NAME])?;
    match std::fs::remove_file(unit_path()) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(anyhow::anyhow!("cannot remove {}: {error}", unit_path().display()));
        }
    }
    Ok(())
}

/// Read-only status: whether the user unit is enabled
/// (`systemctl --user is-enabled`, exit 0). Headless sessions without a
/// systemd user bus report `false` (the Settings row renders "off").
pub fn is_enabled() -> bool {
    Command::new("systemctl")
        .args(["--user", "is-enabled", UNIT_NAME])
        .output()
        .is_ok_and(|output| output.status.success())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_unit_content_contains_exec_start_with_both_paths() {
        let unit = user_unit_content("/usr/bin/clash-verge-cli", "/home/u/.config/clash-verge-cli");
        assert!(unit.contains(
            "ExecStart=/usr/bin/clash-verge-cli start --foreground --config-dir /home/u/.config/clash-verge-cli"
        ));
    }

    #[test]
    fn user_unit_content_targets_default_for_login_start() {
        // WantedBy=default.target starts the unit at login; the system unit
        // uses multi-user.target (boot). The two must not be conflated.
        let unit = user_unit_content("/usr/bin/clash-verge-cli", "/home/u/.config/clash-verge-cli");
        assert!(unit.contains("WantedBy=default.target"));
        assert!(!unit.contains("multi-user.target"));
        assert!(unit.contains("Restart=on-failure"));
        assert!(unit.contains("$MAINPID"));
    }

    #[test]
    fn user_unit_content_escapes_special_path_characters() {
        // Same escaping contract as the system unit: whitespace `\s`,
        // literal `%` as `%%`, dollar doubled, quotes backslash-escaped.
        let unit = user_unit_content("/opt/my apps/clash-verge-cli", "/home/u/.config/100% $cfg");
        assert!(unit.contains(
            "ExecStart=/opt/my\\sapps/clash-verge-cli start --foreground --config-dir /home/u/.config/100%%\\s$$cfg"
        ));
        assert!(
            !unit.contains("/home/u/.config/100% $cfg"),
            "raw path must not survive escaping"
        );
        assert!(unit.contains("$MAINPID"), "the template's own variable stays intact");
    }

    #[test]
    fn unit_dir_follows_xdg_config_home_and_defaults() {
        assert_eq!(
            unit_dir_from(Some("/custom/config".into())),
            PathBuf::from("/custom/config/systemd/user")
        );
        // No XDG_CONFIG_HOME → the OS config dir (always Some on Linux).
        let fallback = unit_dir_from(None);
        assert_eq!(fallback.file_name().expect("dir name"), "user");
        assert_eq!(fallback.parent().expect("parent").file_name().expect("name"), "systemd");
    }

    #[test]
    fn unit_path_lives_under_the_user_systemd_dir() {
        let path = unit_dir_from(Some("/x/cfg".into())).join(UNIT_NAME);
        assert_eq!(path.file_name().expect("name"), "clash-verge-cli.service");
        assert_eq!(path.parent().expect("parent").file_name().expect("name"), "user");
    }
}
