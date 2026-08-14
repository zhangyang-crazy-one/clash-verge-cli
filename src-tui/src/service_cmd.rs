//! systemd service management: install/uninstall/status for clash-verge-cli daemon.
//!
//! Each install/uninstall runs the whole transaction under exactly ONE
//! `sudo -A` boundary (the CLI's own self-rendered askpass), regardless of
//! sudo timestamp caching. Commands inside the transaction are fixed argv
//! built from quoted constants — no shell interpolation of user input.

use std::process::Command;

const SERVICE_NAME: &str = "clash-verge-cli";
const SERVICE_PATH: &str = "/etc/systemd/system/clash-verge-cli.service";

/// Escape a path for a systemd unit `ExecStart=` value. systemd splits the
/// command line on unescaped whitespace, expands `%` specifiers and `$VAR`
/// environment references, and interprets a small set of escape sequences.
/// To refer to the exact file:
///
/// - whitespace becomes the documented `\s` (space) / `\t` (tab) / `\n`
///   (newline) escapes — a bare backslash-space is NOT a valid systemd
///   escape and makes ExecStart fail to parse
/// - quotes are backslash-escaped (`\"`, `\'`); a literal backslash is `\\`
/// - a literal `%` is `%%` (suppresses specifier expansion such as `%i`)
/// - a literal `$` is `$$` (suppresses `$VAR` / `${VAR}` expansion)
///
/// (systemd does not run commands through a shell, so no shell
/// metacharacters need handling beyond these.)
///
/// `pub(crate)` so the login-autostart user unit ([`crate::autostart`])
/// reuses the exact same escaping — the two unit renderers must agree.
pub(crate) fn systemd_escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 8);
    for c in value.chars() {
        match c {
            ' ' => out.push_str("\\s"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '"' => out.push_str("\\\""),
            '\'' => out.push_str("\\'"),
            '\\' => out.push_str("\\\\"),
            '%' => out.push_str("%%"),
            '$' => out.push_str("$$"),
            _ => out.push(c),
        }
    }
    out
}

/// Generate systemd unit file content.
/// The binary runs in foreground mode so systemd can track the PID properly.
/// `binary_path` and `config_dir` are systemd-escaped so units keep working
/// for paths containing spaces, `%`, quotes or backslashes.
pub fn unit_content(binary_path: &str, config_dir: &str) -> String {
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
WantedBy=multi-user.target
"#,
        systemd_escape(binary_path),
        systemd_escape(config_dir)
    )
}

/// Single-quote a value for `sh -c` (safe argv even with spaces/quotes).
fn sh_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// Build the one-shot install transaction script. `set -eu` aborts on the
/// first failing step; `echo` markers identify the failing step in stderr.
pub fn build_install_script(tmp_unit: &str, service_path: &str, service_name: &str, start_now: bool) -> String {
    let mut script = String::new();
    script.push_str("set -eu\n");
    script.push_str("echo 'clash-verge-cli: copying unit'\n");
    script.push_str(&format!("cp -- {} {}\n", sh_quote(tmp_unit), sh_quote(service_path)));
    script.push_str("echo 'clash-verge-cli: reloading systemd'\n");
    script.push_str("systemctl daemon-reload\n");
    script.push_str("echo 'clash-verge-cli: enabling service'\n");
    script.push_str(&format!("systemctl enable {}\n", sh_quote(service_name)));
    if start_now {
        script.push_str("echo 'clash-verge-cli: starting service'\n");
        script.push_str(&format!("systemctl start {}\n", sh_quote(service_name)));
    }
    script
}

/// Build the one-shot uninstall transaction script. `stop`/`disable` may
/// fail when the service is not running/registered, so they are tolerant;
/// removing the unit and reloading systemd complete the transaction.
pub fn build_uninstall_script(service_path: &str, service_name: &str) -> String {
    let mut script = String::new();
    script.push_str("set -u\n");
    script.push_str("echo 'clash-verge-cli: stopping service'\n");
    script.push_str(&format!("systemctl stop {} || true\n", sh_quote(service_name)));
    script.push_str("echo 'clash-verge-cli: disabling service'\n");
    script.push_str(&format!("systemctl disable {} || true\n", sh_quote(service_name)));
    script.push_str("echo 'clash-verge-cli: removing unit'\n");
    script.push_str(&format!("rm -f {}\n", sh_quote(service_path)));
    script.push_str("echo 'clash-verge-cli: reloading systemd'\n");
    script.push_str("systemctl daemon-reload || true\n");
    script
}

/// Build the `sudo -S` command for a password-authenticated transaction.
/// `-S` makes sudo read the password from stdin; `SUDO_ASKPASS` is never
/// touched, so the TUI popup's captured password goes straight to sudo and
/// no askpass subprocess fights the raw-mode terminal.
fn sudo_password_transaction_command(script: &str) -> Command {
    use std::process::Stdio;
    let mut command = Command::new("sudo");
    command.arg("-S").args(["sh", "-c", script]);
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

/// Run one privileged transaction under `sudo -S`: the password captured by
/// the TUI popup is written to sudo's stdin, so no askpass subprocess is
/// needed (the popup stays rendered by the TUI) and nothing is logged. The
/// whole transaction stays inside a single sudo authentication boundary.
fn run_sudo_transaction_with_password(script: &str, password: &str) -> std::io::Result<()> {
    use std::io::Write as _;
    let mut child = sudo_password_transaction_command(script)
        .spawn()
        .map_err(|error| std::io::Error::other(format!("cannot run sudo (is sudo installed?): {error}")))?;
    if let Some(mut stdin) = child.stdin.take() {
        writeln!(stdin, "{password}")?;
    }
    let output = child.wait_with_output()?;
    if !output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(std::io::Error::other(format!(
            "sudo transaction failed: {} {}",
            stdout.trim(),
            stderr.trim()
        )));
    }
    Ok(())
}

/// Run one privileged transaction under a single sudo authentication
/// boundary: `sudo -A sh -c <script>` with `SUDO_ASKPASS` pointing at this
/// binary's hidden `askpass` subcommand (works over SSH without DISPLAY).
/// CLI-only: the TUI uses [`run_sudo_transaction_with_password`] instead.
fn run_sudo_transaction(script: &str) -> std::io::Result<()> {
    let askpass = std::env::current_exe().unwrap_or_default();
    let output = Command::new("sudo")
        .arg("-A")
        .env("SUDO_ASKPASS", &askpass)
        .args(["sh", "-c", script])
        .output()
        .map_err(|error| std::io::Error::other(format!("cannot run sudo (is sudo installed?): {error}")))?;
    if !output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(std::io::Error::other(format!(
            "sudo transaction failed: {} {}",
            stdout.trim(),
            stderr.trim()
        )));
    }
    Ok(())
}

/// Install systemd service (requires root). The whole transaction (copy
/// unit, daemon-reload, enable, optional start) runs under one sudo call.
pub fn install_service(binary_path: &str, config_dir: &str, start_now: bool) -> std::io::Result<()> {
    install_service_with(binary_path, config_dir, start_now, &mut run_sudo_transaction)
}

/// Install with an injectable transaction executor (test boundary).
fn install_service_with(
    binary_path: &str,
    config_dir: &str,
    start_now: bool,
    executor: &mut dyn FnMut(&str) -> std::io::Result<()>,
) -> std::io::Result<()> {
    let unit = unit_content(binary_path, config_dir);
    // Write to a temp dir under /tmp with a unique name — avoids the
    // well-known /tmp/clash-verge-cli.service symlink race.
    let dir = tempfile::tempdir().map_err(std::io::Error::other)?;
    let tmp_path = dir.path().join("clash-verge-cli.service");
    std::fs::write(&tmp_path, unit)?;

    let script = build_install_script(&tmp_path.to_string_lossy(), SERVICE_PATH, SERVICE_NAME, start_now);
    // Exactly one sudo authentication boundary for the whole transaction.
    executor(&script)?;
    // Temp dir is cleaned up when `dir` is dropped.

    if start_now {
        println!("service installed and started");
    } else {
        println!("service installed (not started)");
    }
    Ok(())
}

/// Install systemd service using a password captured by the TUI password
/// popup: `sudo -S` reads the password from stdin (no askpass — the popup
/// must stay rendered by the raw-mode TUI). The whole transaction (copy
/// unit, daemon-reload, enable, optional start) runs under one sudo call.
pub fn install_service_with_password(
    binary_path: &str,
    config_dir: &str,
    start_now: bool,
    password: &str,
) -> std::io::Result<()> {
    install_service_with_password_with(
        binary_path,
        config_dir,
        start_now,
        password,
        &mut run_sudo_transaction_with_password,
    )
}

/// Install with an injectable password-transaction executor (test boundary).
/// The executor receives `(script, password)` so tests can record both the
/// transaction text and that the captured password reaches the boundary.
fn install_service_with_password_with(
    binary_path: &str,
    config_dir: &str,
    start_now: bool,
    password: &str,
    executor: &mut dyn FnMut(&str, &str) -> std::io::Result<()>,
) -> std::io::Result<()> {
    let unit = unit_content(binary_path, config_dir);
    // Write to a temp dir under /tmp with a unique name — avoids the
    // well-known /tmp/clash-verge-cli.service symlink race.
    let dir = tempfile::tempdir().map_err(std::io::Error::other)?;
    let tmp_path = dir.path().join("clash-verge-cli.service");
    std::fs::write(&tmp_path, unit)?;

    let script = build_install_script(&tmp_path.to_string_lossy(), SERVICE_PATH, SERVICE_NAME, start_now);
    // Exactly one sudo -S authentication boundary for the whole transaction.
    // No stdout output here: the TUI password path must never write to the
    // terminal while the alternate screen is active (status bar reports it).
    executor(&script, password)?;

    Ok(())
}

/// Uninstall systemd service. One sudo call for the whole transaction.
pub fn uninstall_service() -> std::io::Result<()> {
    uninstall_service_with(&mut run_sudo_transaction)
}

/// Uninstall systemd service using a password captured by the TUI popup
/// (`sudo -S`, no askpass). One sudo call for the whole transaction.
pub fn uninstall_service_with_password(password: &str) -> std::io::Result<()> {
    uninstall_service_with_password_with(password, &mut run_sudo_transaction_with_password)
}

/// Uninstall with an injectable password-transaction executor (test boundary).
fn uninstall_service_with_password_with(
    password: &str,
    executor: &mut dyn FnMut(&str, &str) -> std::io::Result<()>,
) -> std::io::Result<()> {
    let script = build_uninstall_script(SERVICE_PATH, SERVICE_NAME);
    // No stdout output: the TUI password path must never write to the
    // terminal while the alternate screen is active.
    executor(&script, password)?;
    Ok(())
}

/// Uninstall with an injectable transaction executor (test boundary).
fn uninstall_service_with(executor: &mut dyn FnMut(&str) -> std::io::Result<()>) -> std::io::Result<()> {
    let script = build_uninstall_script(SERVICE_PATH, SERVICE_NAME);
    // Exactly one sudo authentication boundary for the whole transaction.
    executor(&script)?;
    println!("service uninstalled");
    Ok(())
}

/// Check service active state.
pub fn service_active_state() -> String {
    let output = Command::new("systemctl").args(["is-active", SERVICE_NAME]).output();
    match output {
        Ok(o) => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        Err(_) => "unknown".to_string(),
    }
}

/// Check service enabled state.
pub fn service_enabled_state() -> String {
    let output = Command::new("systemctl").args(["is-enabled", SERVICE_NAME]).output();
    match output {
        Ok(o) => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        Err(_) => "unknown".to_string(),
    }
}

/// Whether the system service unit is installed (unit file present).
///
/// `systemctl is-enabled` alone misclassifies an installed-but-disabled unit
/// as `disabled` — the exact state the Settings uninstall offer must still
/// cover — so installation is probed by unit-file presence instead:
/// `systemctl list-unit-files` prints the unit's own STATE row
/// (enabled/disabled/static) when the file exists, and "0 unit files
/// listed." when it does not.
pub fn service_installed_state() -> bool {
    service_installed_state_with(&|name| {
        Command::new("systemctl")
            .args(["list-unit-files", name])
            .output()
            .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
            .unwrap_or_default()
    })
}

/// Injectable installation probe (test boundary): the probe returns the
/// `systemctl list-unit-files` stdout for the service name.
fn service_installed_state_with(probe: &dyn Fn(&str) -> String) -> bool {
    // The unit's own row (e.g. "clash-verge-cli.service disabled") appears
    // after the header only when the unit file exists; a missing unit prints
    // "0 unit files listed." instead.
    probe(SERVICE_NAME)
        .lines()
        .any(|line| line.trim_start().starts_with(SERVICE_NAME))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_script_runs_all_steps_in_one_transaction() {
        let script = build_install_script("/tmp/tmpX/clash-verge-cli.service", SERVICE_PATH, SERVICE_NAME, true);
        let cp_idx = script.find("cp -- ").expect("cp step");
        let reload_idx = script.find("systemctl daemon-reload").expect("daemon-reload step");
        let enable_idx = script.find("systemctl enable 'clash-verge-cli'").expect("enable step");
        let start_idx = script.find("systemctl start 'clash-verge-cli'").expect("start step");
        assert!(cp_idx < reload_idx && reload_idx < enable_idx && enable_idx < start_idx);
        // Exactly one `sh -c`-style transaction body: no second sudo.
        assert!(!script.contains("sudo"), "script must not self-elevate: {script}");
        // Unit path is single-quoted (safe argv).
        assert!(script.contains(&format!("cp -- {}", sh_quote("/tmp/tmpX/clash-verge-cli.service"))));
    }

    #[test]
    fn install_script_omits_start_when_not_requested() {
        let script = build_install_script("/tmp/u.service", SERVICE_PATH, SERVICE_NAME, false);
        assert!(script.contains("systemctl enable 'clash-verge-cli'"));
        assert!(!script.contains("systemctl start"));
    }

    #[test]
    fn uninstall_script_covers_stop_disable_remove_reload_in_order() {
        let script = build_uninstall_script(SERVICE_PATH, SERVICE_NAME);
        let stop_idx = script.find("systemctl stop 'clash-verge-cli'").expect("stop step");
        let disable_idx = script
            .find("systemctl disable 'clash-verge-cli'")
            .expect("disable step");
        let rm_idx = script
            .find("rm -f '/etc/systemd/system/clash-verge-cli.service'")
            .expect("rm step");
        let reload_idx = script.find("systemctl daemon-reload").expect("reload step");
        assert!(stop_idx < disable_idx && disable_idx < rm_idx && rm_idx < reload_idx);
        assert!(!script.contains("sudo"), "script must not self-elevate: {script}");
    }

    #[test]
    fn sh_quote_handles_spaces_quotes_and_single_quotes() {
        assert_eq!(sh_quote("/tmp/a b"), "'/tmp/a b'");
        assert_eq!(sh_quote("it's"), "'it'\\''s'");
    }

    #[test]
    fn install_transaction_invokes_executor_exactly_once() {
        let mut calls: Vec<String> = Vec::new();
        let result = install_service_with(
            "/usr/bin/clash-verge-cli",
            "/home/u/.config/clash-verge-cli",
            true,
            &mut |script: &str| {
                calls.push(script.to_string());
                Ok(())
            },
        );
        assert!(result.is_ok(), "install must succeed with a recording executor");
        assert_eq!(calls.len(), 1, "install must run exactly one sudo transaction");
        let script = &calls[0];
        assert!(script.contains("cp -- "), "{script}");
        assert!(script.contains("systemctl enable 'clash-verge-cli'"), "{script}");
        assert!(script.contains("systemctl start 'clash-verge-cli'"), "{script}");
    }

    #[test]
    fn uninstall_transaction_invokes_executor_exactly_once() {
        let mut calls: Vec<String> = Vec::new();
        let result = uninstall_service_with(&mut |script: &str| {
            calls.push(script.to_string());
            Ok(())
        });
        assert!(result.is_ok(), "uninstall must succeed with a recording executor");
        assert_eq!(calls.len(), 1, "uninstall must run exactly one sudo transaction");
        assert!(
            calls[0].contains("rm -f '/etc/systemd/system/clash-verge-cli.service'"),
            "{}",
            calls[0]
        );
    }

    #[test]
    fn transaction_failure_is_propagated() {
        let mut calls = 0;
        let result = install_service_with(
            "/usr/bin/clash-verge-cli",
            "/home/u/.config/clash-verge-cli",
            false,
            &mut |_script: &str| {
                calls += 1;
                Err(std::io::Error::other("systemctl enable failed"))
            },
        );
        assert!(result.is_err(), "a failing transaction step must surface");
        assert_eq!(calls, 1, "the single boundary is still one call");
    }

    #[test]
    fn install_with_password_invokes_executor_once_with_script_and_password() {
        let mut calls: Vec<(String, String)> = Vec::new();
        let result = install_service_with_password_with(
            "/usr/bin/clash-verge-cli",
            "/home/u/.config/clash-verge-cli",
            true,
            "s3cret",
            &mut |script: &str, password: &str| {
                calls.push((script.to_string(), password.to_string()));
                Ok(())
            },
        );
        assert!(result.is_ok(), "install must succeed with a recording executor");
        assert_eq!(calls.len(), 1, "install must run exactly one sudo -S transaction");
        let (script, password) = &calls[0];
        assert_eq!(password, "s3cret", "the captured password must reach the executor");
        assert!(script.contains("cp -- "), "{script}");
        assert!(script.contains("systemctl enable 'clash-verge-cli'"), "{script}");
        assert!(script.contains("systemctl start 'clash-verge-cli'"), "{script}");
    }

    #[test]
    fn uninstall_with_password_invokes_executor_once_with_password() {
        let mut calls: Vec<(String, String)> = Vec::new();
        let result = uninstall_service_with_password_with("hunter2", &mut |script: &str, password: &str| {
            calls.push((script.to_string(), password.to_string()));
            Ok(())
        });
        assert!(result.is_ok(), "uninstall must succeed with a recording executor");
        assert_eq!(calls.len(), 1, "uninstall must run exactly one sudo -S transaction");
        let (script, password) = &calls[0];
        assert_eq!(password, "hunter2");
        assert!(
            script.contains("rm -f '/etc/systemd/system/clash-verge-cli.service'"),
            "{script}"
        );
    }

    #[test]
    fn installed_probe_classifies_disabled_unit_as_installed() {
        // P2b: `is-enabled` reports 'disabled' for an installed-but-disabled
        // unit; the installation probe (unit-file presence via
        // `list-unit-files`) must still report installed so uninstall stays
        // offered.
        let output = format!(
            "UNIT FILE               STATE   PRESET\n{name}.service disabled disabled\n",
            name = SERVICE_NAME
        );
        assert!(service_installed_state_with(&|_name| output.clone()));
    }

    #[test]
    fn installed_probe_classifies_missing_unit_as_not_installed() {
        // A unit that does not exist prints "0 unit files listed." with no
        // unit row: not installed.
        let output = "UNIT FILE STATE PRESET\n\n0 unit files listed.\n".to_string();
        assert!(!service_installed_state_with(&|_name| output.clone()));
    }

    #[test]
    fn installed_probe_error_is_treated_as_not_installed() {
        // systemctl unavailable (e.g. headless/no systemd) → conservative
        // "not installed".
        assert!(!service_installed_state_with(&|_name| String::new()));
    }

    #[test]
    fn password_transaction_failure_is_propagated() {
        let mut calls = 0;
        let result = install_service_with_password_with(
            "/usr/bin/clash-verge-cli",
            "/home/u/.config/clash-verge-cli",
            true,
            "wrong",
            &mut |_script: &str, _password: &str| {
                calls += 1;
                Err(std::io::Error::other("incorrect password"))
            },
        );
        assert!(result.is_err(), "a failing password transaction must surface");
        assert_eq!(calls, 1, "the single boundary is still one call");
    }

    #[test]
    fn sudo_password_command_uses_minus_s_and_no_askpass_env() {
        // The TUI password path must authenticate with `-S` (password on
        // stdin) and must NEVER set SUDO_ASKPASS: an askpass subprocess
        // would fight the raw-mode TUI popup.
        let command = sudo_password_transaction_command("echo hi");
        assert_eq!(command.get_program(), "sudo");
        let args: Vec<_> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args, vec!["-S", "sh", "-c", "echo hi"]);
        assert!(
            command.get_envs().all(|(key, _)| key != "SUDO_ASKPASS"),
            "no SUDO_ASKPASS may be set on the -S path"
        );
        // -S reads the password from stdin (that is the askpass-free
        // mechanism the TUI relies on); nothing may fall back to the
        // askpass flags.
        assert!(!args.contains(&"-A".to_string()));
        assert!(!args.contains(&"--askpass".to_string()));
    }

    #[test]
    fn unit_content_contains_exec_start_with_both_paths() {
        let unit = unit_content("/usr/bin/clash-verge-cli", "/home/u/.config/clash-verge-cli");
        assert!(unit.contains(
            "ExecStart=/usr/bin/clash-verge-cli start --foreground --config-dir /home/u/.config/clash-verge-cli"
        ));
    }

    #[test]
    fn systemd_escape_leaves_plain_paths_untouched() {
        assert_eq!(systemd_escape("/usr/bin/clash-verge-cli"), "/usr/bin/clash-verge-cli");
        assert_eq!(
            systemd_escape("/home/u/.config/clash-verge-cli"),
            "/home/u/.config/clash-verge-cli"
        );
    }

    #[test]
    fn systemd_escape_handles_spaces_percent_quotes_and_backslash() {
        // Whitespace becomes systemd's `\s` escape, NOT a bare backslash-
        // space (which is not a valid systemd escape and fails ExecStart).
        assert_eq!(
            systemd_escape("/opt/my apps/clash verge"),
            "/opt/my\\sapps/clash\\sverge"
        );
        // `%%` is systemd's documented literal-percent escape (specifier
        // expansion), NOT backslash-percent.
        assert_eq!(systemd_escape("/cfg/100%dir"), "/cfg/100%%dir");
        assert_eq!(systemd_escape("/a\"b'c\\d"), "/a\\\"b\\'c\\\\d");
        // Tab/newline become systemd's documented `\t` / `\n` escapes.
        assert_eq!(systemd_escape("/tab\tpath"), "/tab\\tpath");
        assert_eq!(systemd_escape("/line\npath"), "/line\\npath");
    }

    #[test]
    fn systemd_escape_doubles_literal_dollar_signs() {
        // `$VAR` / `${VAR}` are expanded by systemd; `$$` yields a literal
        // `$` so a path containing one survives unchanged.
        assert_eq!(systemd_escape("/var/$USER/cfg"), "/var/$$USER/cfg");
        assert_eq!(systemd_escape("/a${x}b"), "/a$${x}b");
        assert_eq!(systemd_escape("/plain"), "/plain");
    }

    #[test]
    fn unit_content_escapes_special_path_characters_in_exec_start() {
        let unit = unit_content("/opt/my apps/clash-verge-cli", "/home/u/.config/100% clash");
        assert!(unit.contains(
            "ExecStart=/opt/my\\sapps/clash-verge-cli start --foreground --config-dir /home/u/.config/100%%\\sclash"
        ));
    }

    #[test]
    fn unit_content_escapes_dollar_in_exec_start_but_keeps_mainpid_variable() {
        // The binary/config paths must not be subject to `$VAR` expansion,
        // while the template's own `$MAINPID` reference stays intact.
        let unit = unit_content("/usr/bin/clash-verge-cli", "/tmp/$cfg");
        assert!(unit.contains("--config-dir /tmp/$$cfg"));
        assert!(unit.contains("$MAINPID"));
        assert!(!unit.contains("/tmp/$cfg"), "raw $cfg must not survive escaping");
    }

    #[test]
    fn unit_content_never_contains_a_raw_percent_in_exec_start() {
        // `%` starts a systemd specifier; the `%%` escape keeps the value
        // literal and the $MAINPID variable intact.
        let unit = unit_content("/usr/bin/clash-verge-cli", "/tmp/100%");
        assert!(unit.contains("--config-dir /tmp/100%%"));
        assert!(unit.contains("$MAINPID"));
    }
}
