//! TUN capability detection, explicit setup, and non-mutating preflight.
//!
//! The ONLY paths that execute sudo/setcap are the explicit `tun setup` CLI
//! command and the equivalent TUI Settings action. Daily core lifecycle
//! (start / restart / daemon / TUN toggle / automatic restart) only *checks*
//! capabilities through [`require_tun_capability`], which never elevates.
//!
//! Password entry goes through `sudo -A` + `SUDO_ASKPASS` pointing at this
//! binary's hidden `askpass` subcommand (a bordered TUI popup), so it works
//! on headless servers over SSH.

use std::path::Path;
use std::process::Command;

use anyhow::Context;

/// Capabilities mihomo needs for TUN mode on Linux.
pub const TUN_CAPS: &str = "cap_net_admin,cap_net_raw+eip";

/// Command users run to fix a missing-capability failure.
pub const TUN_SETUP_COMMAND: &str = "clash-verge-cli tun setup";

/// Polkit JS rule authorizing the invoking user to configure
/// systemd-resolved DNS without auth dialogs. polkit auto-watches
/// `rules.d`, so writing the file needs no reload.
pub const RESOLVE1_RULE_PATH: &str = "/etc/polkit-1/rules.d/50-clash-verge-cli-resolved.rules";

/// systemd-resolved's polkit policy descriptor. Absent on non-resolved
/// systems; when absent the DNS rule is not needed (and not installed).
pub const RESOLVE1_POLICY_PATH: &str = "/usr/share/polkit-1/actions/org.freedesktop.resolve1.policy";

/// Content marker every rendered rule starts with. [`resolve1_rule_installed`]
/// requires this marker so a foreign/stale/broken file at the rule path is
/// NOT treated as installed — the next setup run rewrites (repairs) it.
pub const RESOLVE1_RULE_MARKER: &str = "// clash-verge-cli: allow the invoking user to configure systemd-resolved";

/// Whether the current process runs with effective root privileges. Root
/// bypasses the capability check because root can create TUN devices
/// without file capabilities (root service / daemon model).
pub fn running_as_root() -> bool {
    // SAFETY: geteuid(2) never fails and takes no arguments.
    unsafe { nix::libc::geteuid() == 0 }
}

/// Pure capability-state check on `getcap` output (injectable for tests).
///
/// `getcap` prints each capability with its flag set, e.g.
/// `cap_net_admin,cap_net_raw=eip`. Both TUN capabilities must carry the
/// effective bit: a permitted-only (`+p`) or flagless entry is NOT enough
/// for mihomo to create the tun device.
pub fn capability_output_has_tun(stdout: &[u8]) -> bool {
    let text = String::from_utf8_lossy(stdout);
    let mut admin_effective = false;
    let mut raw_effective = false;
    for word in text.split_whitespace() {
        // A capability token looks like `cap_net_admin,cap_net_raw=eip`
        // (comma-joined names sharing one flag suffix).
        if !word.starts_with("cap_") {
            continue;
        }
        let (names, flags) = split_cap_token(word);
        if flags.contains('e') {
            if names.contains(&"cap_net_admin") {
                admin_effective = true;
            }
            if names.contains(&"cap_net_raw") {
                raw_effective = true;
            }
        }
    }
    admin_effective && raw_effective
}

/// Split a `getcap` token into its cap names and flag set. The flag suffix
/// is introduced by `=` or `+`; without a suffix the flags are empty, so
/// permitted-only text forms cannot pass the effective-bit check.
fn split_cap_token(word: &str) -> (Vec<&str>, &str) {
    match word.find(['=', '+']) {
        Some(index) => (word[..index].split(',').collect(), &word[index + 1..]),
        None => (word.split(',').collect(), ""),
    }
}

/// Whether the binary already carries the TUN capabilities (read-only).
pub fn has_tun_capability(binary: &Path) -> bool {
    let Ok(output) = Command::new("getcap").arg(binary).output() else {
        return false;
    };
    capability_output_has_tun(&output.stdout)
}

/// Actionable error text for a binary missing TUN capabilities: identifies
/// the binary path and the explicit setup command.
pub fn missing_capability_error(binary: &Path) -> String {
    format!(
        "TUN is enabled but '{}' lacks {}.\nRun: {} (or use the TUI Settings → TUN setup action), then start again.",
        binary.display(),
        TUN_CAPS,
        TUN_SETUP_COMMAND
    )
}

/// Render the polkit JS rule that lets `username` drive systemd-resolved
/// D-Bus calls without a GUI polkit auth dialog: set DNS servers, set
/// search domains, set default route (the three actions mihomo performs on
/// every TUN-enabled core start) and revert (TUN teardown on stop / toggle
/// off — default policy auth_admin_keep, same as the others).
///
/// The username is embedded into a JS string that root polkitd evaluates,
/// so it is escaped here as defense-in-depth: callers must already have
/// passed it through [`is_valid_rule_username`] (both the passwd lookup and
/// the `$USER` fallback are validated identically) — this renderer stays
/// safe even if that charset ever widens.
pub fn resolved_polkit_rule(username: &str) -> String {
    format!(
        r#"// clash-verge-cli: allow the invoking user to configure systemd-resolved DNS for TUN without polkit dialogs
polkit.addRule(function(action, subject) {{
    if (subject.user == "{escaped}" && (action.id == "org.freedesktop.resolve1.set-dns-servers" || action.id == "org.freedesktop.resolve1.set-domains" || action.id == "org.freedesktop.resolve1.set-default-route" || action.id == "org.freedesktop.resolve1.revert")) {{
        return polkit.Result.YES;
    }}
}});
"#,
        escaped = escape_js_string(username),
    )
}

/// Strict username charset for the polkit rule: `^[A-Za-z0-9_.-]+$`.
///
/// The username lands in a JS string evaluated by root polkitd, so anything
/// outside this set (quotes, semicolons, whitespace, control chars) is
/// rejected outright — a crafted `$USER` must never be able to widen the
/// rule to other users or inject JS. POSIX login names always fit this set.
fn is_valid_rule_username(user: &str) -> bool {
    !user.is_empty()
        && user
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
}

/// Escape residual JS string metacharacters for the double-quoted polkit
/// string. After [`is_valid_rule_username`] nothing here can trigger, but
/// keeping the renderer escaping-aware prevents a future validation
/// regression from becoming an injection.
fn escape_js_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            other => out.push(other),
        }
    }
    out
}

/// The invoking user's username, used to scope the polkit rule. Prefers the
/// authoritative `getpwuid(getuid())`; falls back to `$USER`. BOTH sources
/// are validated against [`is_valid_rule_username`] identically, so a
/// forged `$USER` cannot smuggle polkit JS — an invalid name yields `None`
/// and the setup transaction fails instead of writing a rule.
pub fn current_username() -> Option<String> {
    // SAFETY: getuid(2) never fails; getpwuid(3) returns a pointer that is
    // null when no entry exists, and its pw_name is null-terminated. The
    // static buffer is only read once and no other libc passwd call runs on
    // this thread.
    let from_passwd = unsafe {
        let pwd = nix::libc::getpwuid(nix::libc::getuid());
        if !pwd.is_null() && !(*pwd).pw_name.is_null() {
            Some(std::ffi::CStr::from_ptr((*pwd).pw_name).to_string_lossy().into_owned())
        } else {
            None
        }
    };
    from_passwd.filter(|name| is_valid_rule_username(name)).or_else(|| {
        std::env::var("USER")
            .ok()
            .map(|user| user.trim().to_string())
            .filter(|user| is_valid_rule_username(user))
    })
}

/// Whether the systemd-resolved polkit policy descriptor exists (read-only).
pub fn resolved_policy_present() -> bool {
    Path::new(RESOLVE1_POLICY_PATH).exists()
}

/// Verdict of the pkcheck authorization probe for a resolve1 DNS action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PkcheckVerdict {
    /// exit 0 — the action is authorized for the invoking process (the
    /// installed rule is effective).
    Authorized,
    /// exit 1 — authorization required (rule missing or ineffective).
    NotAuthorized,
    /// any other exit, or the probe could not be executed.
    Indeterminate,
}

/// Combined authorization outcome used by [`resolve1_rule_installed`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Resolve1Auth {
    /// pkcheck authorizes, or the file-marker fallback read succeeded.
    Authorized,
    /// pkcheck requires authorization (rule genuinely missing/ineffective).
    NotAuthorized,
    /// Neither pkcheck nor the file marker could be verified (pkcheck
    /// errored, or the rules dir denies read like the root-owned EACCES
    /// incident). NEVER treated as "missing": that false negative caused a
    /// completed setup to abort its TUN start.
    Indeterminate,
}

/// File-marker fallback outcome for when pkcheck is unavailable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RuleFileState {
    /// File readable and carries our marker.
    Present,
    /// File readable without the marker, or genuinely absent (ENOENT).
    Absent,
    /// File could not be inspected (EACCES on a root-owned `rules.d` dir,
    /// or another IO error) — indeterminate, never "missing".
    Indeterminate,
}

/// Map a pkcheck probe exit code to a verdict. exit 0 = authorized, exit 1 =
/// auth required, anything else (or no exit code) = indeterminate.
fn pkcheck_verdict(exit_code: Option<i32>) -> PkcheckVerdict {
    match exit_code {
        Some(0) => PkcheckVerdict::Authorized,
        Some(1) => PkcheckVerdict::NotAuthorized,
        _ => PkcheckVerdict::Indeterminate,
    }
}

/// Run the pkcheck authorization probe for the resolve1 `set-dns-servers`
/// action on the invoking process (plain `--process <pid>` form, verified
/// working). `None` when pkcheck is not installed / could not be spawned.
fn pkcheck_probe() -> Option<PkcheckVerdict> {
    let output = Command::new("pkcheck")
        .args(["--action-id", "org.freedesktop.resolve1.set-dns-servers", "--process"])
        .arg(std::process::id().to_string())
        .output()
        .ok()?;
    Some(pkcheck_verdict(output.status.code()))
}

/// Public pkcheck probe surface: whether the invoking user is authorized for
/// the resolve1 DNS actions. `Some(Authorized)` proves the installed rule is
/// effective even when the rules file is unreadable (EACCES); `None` means
/// pkcheck is unavailable and the file-marker fallback applies.
pub fn resolve1_authorized_via_pkcheck() -> Option<PkcheckVerdict> {
    pkcheck_probe()
}

/// Inspect the rule file's content marker (fallback path). ENOENT counts as
/// genuinely absent; EACCES and other IO errors are indeterminate, not
/// missing — the EACCES incident taught us that.
fn rule_file_state(path: &Path) -> RuleFileState {
    match std::fs::read_to_string(path) {
        Ok(content) if content.contains(RESOLVE1_RULE_MARKER) => RuleFileState::Present,
        Ok(_) => RuleFileState::Absent,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => RuleFileState::Absent,
        Err(_) => RuleFileState::Indeterminate,
    }
}

/// Injectable core of [`resolve1_auth_probe`]: pkcheck wins whenever it ran;
/// the file marker is consulted only when pkcheck is unavailable; EACCES →
/// Indeterminate (never "missing").
fn resolve1_auth_probe_impl(pkcheck: Option<PkcheckVerdict>, file_state: RuleFileState) -> Resolve1Auth {
    match pkcheck {
        Some(PkcheckVerdict::Authorized) => Resolve1Auth::Authorized,
        Some(PkcheckVerdict::NotAuthorized) => Resolve1Auth::NotAuthorized,
        Some(PkcheckVerdict::Indeterminate) => Resolve1Auth::Indeterminate,
        None => match file_state {
            RuleFileState::Present => Resolve1Auth::Authorized,
            RuleFileState::Absent => Resolve1Auth::NotAuthorized,
            RuleFileState::Indeterminate => Resolve1Auth::Indeterminate,
        },
    }
}

/// Authoritative resolve1 authorization probe: pkcheck verdict first, the
/// file-marker read as fallback only when pkcheck is unavailable, and
/// unreadable states → Indeterminate.
fn resolve1_auth_probe() -> Resolve1Auth {
    resolve1_auth_probe_impl(
        resolve1_authorized_via_pkcheck(),
        rule_file_state(Path::new(RESOLVE1_RULE_PATH)),
    )
}

/// Injectable mapping from a probe outcome to the bool used by
/// [`resolve1_rule_installed`]: only a genuine `NotAuthorized` is "missing";
/// an indeterminate probe (EACCES, pkcheck error) is treated as installed so
/// the false negative cannot re-trigger setup or abort a completed
/// transaction.
fn resolve1_rule_installed_impl(auth: Resolve1Auth) -> bool {
    auth != Resolve1Auth::NotAuthorized
}

/// Whether the DNS polkit rule is installed and effective.
///
/// Prefers the authoritative pkcheck authorization probe (exit 0 = rule
/// effective) — this works even when `/etc/polkit-1/rules.d` denies
/// unprivileged reads (drwxr-x--- root:polkitd), which is exactly the
/// EACCES false-negative incident: the setup ran fine as root but the app
/// reported the rule missing and aborted the TUN start. Falls back to the
/// content-marker file read only when pkcheck is unavailable; EACCES and
/// other unreadable states are indeterminate (treated as installed), never
/// "missing".
pub fn resolve1_rule_installed() -> bool {
    resolve1_rule_installed_impl(resolve1_auth_probe())
}

/// Read-only: would a TUN-enabled start hit systemd-resolved polkit auth
/// dialogs? True only when TUN is enabled, the process is not root, the
/// resolve1 policy exists, and our rule is absent. Never elevates.
pub fn resolve1_rule_needed(tun_enabled: bool) -> bool {
    resolve1_rule_needed_impl(
        tun_enabled,
        running_as_root(),
        resolved_policy_present(),
        resolve1_rule_installed(),
    )
}

/// Injectable core of [`resolve1_rule_needed`] for tests (no filesystem or
/// uid lookups).
fn resolve1_rule_needed_impl(tun_enabled: bool, root: bool, policy_present: bool, rule_installed: bool) -> bool {
    tun_enabled && !root && policy_present && !rule_installed
}

/// Actionable warning text for a missing DNS polkit rule. Surfaced by the
/// TUI as a status warning and by the manager/daemon as a log warning so a
/// system polkit dialog is never the first notice a user gets.
pub fn missing_resolve1_rule_warning() -> String {
    let user = current_username().unwrap_or_else(|| "your user".to_string());
    format!(
        "TUN DNS needs systemd-resolved polkit authorization: the rule for '{user}' is missing, so core start will trigger polkit dialogs. Run {TUN_SETUP_COMMAND} (or Settings → TUN setup) once to install it."
    )
}

/// Compose the single root shell transaction run under sudo.
///
/// Applies the TUN file capability and — when the systemd-resolved polkit
/// policy exists (`install_rule`) — installs the DNS polkit rule for the
/// invoking `username` as root:root 0644. Rendered as one `sh -c` script so
/// a single sudo authentication covers both changes. polkit watches
/// `rules.d`, so no reload command is needed.
///
/// The mihomo binary path is deliberately NOT part of the script text: it is
/// passed as a positional argument (`sh -c "$script" sh "$binary"`, see
/// [`sh_c_argv`]), so a `'`, `;`, or other metacharacter in the path
/// (HOME/XDG_DATA_HOME-derived, user-controlled) can never break out of the
/// script and execute commands as root. The script references it as `"$1"`.
fn compose_tun_setup_script(username: &str, install_rule: bool) -> String {
    let mut script = format!("set -e\nsetcap {TUN_CAPS} \"$1\"\n");
    if install_rule {
        script.push_str(&format!(
            "if [ -d /etc/polkit-1/rules.d ]; then\n\
             cat > {RESOLVE1_RULE_PATH} <<'CV_CLASH_VERGE_RULE'\n\
             {rule}\
             CV_CLASH_VERGE_RULE\n\
             chown root:root {RESOLVE1_RULE_PATH}\n\
             chmod 0644 {RESOLVE1_RULE_PATH}\n\
             test -f {RESOLVE1_RULE_PATH} && grep -qF '{RESOLVE1_RULE_MARKER}' {RESOLVE1_RULE_PATH}\n\
             fi\n",
            rule = resolved_polkit_rule(username),
        ));
    }
    script
}

/// Build the `sh -c` argument vector for the setup transaction.
///
/// `binary` travels as its own argv element after the script string, never
/// embedded in the shell text: `sh -c "$script" sh "$binary"` makes the
/// path `$1` inside the script. Both the CLI and TUI sudo paths use this so
/// the positional-arg contract is single-sourced and testable.
fn sh_c_argv(script: &str, binary: &Path) -> Vec<std::ffi::OsString> {
    vec![
        std::ffi::OsString::from("sh"),
        std::ffi::OsString::from("-c"),
        std::ffi::OsString::from(script),
        std::ffi::OsString::from("sh"),
        binary.as_os_str().to_owned(),
    ]
}

/// Check-only preflight core: succeed when the capability is present OR the
/// process is root. Never runs sudo/setcap/askpass. `root` and `probe` are
/// injectable so the three states (capable / missing / root-bypass) are
/// testable without executing `getcap`.
fn require_capability_impl(binary: &Path, root: bool, probe: &dyn Fn(&Path) -> bool) -> anyhow::Result<()> {
    if root || probe(binary) {
        return Ok(());
    }
    anyhow::bail!("{}", missing_capability_error(binary))
}

/// Non-mutating preflight used by every TUN-enabled spawn path: resolves
/// nothing, applies nothing, prompts nothing. Root processes bypass the
/// check.
pub fn require_tun_capability(binary: &Path) -> anyhow::Result<()> {
    require_capability_impl(binary, running_as_root(), &has_tun_capability)
}

/// Explicit foreground apply: `sudo -A sh -c` with `SUDO_ASKPASS` pointed
/// at this binary's hidden `askpass` subcommand.
///
/// The single root transaction installs the TUN file capability AND (when
/// the systemd-resolved polkit policy exists) the DNS polkit rule for the
/// invoking user, so the one-time setup needs only one sudo prompt and a
/// later TUN start triggers zero polkit dialogs.
///
/// Returns `Ok(false)` when nothing needs doing (capability present and no
/// rule to install), `Ok(true)` when the transaction applied changes. This
/// is the only CLI path that executes sudo for TUN privileges.
pub fn apply_tun_capability(binary: &Path) -> anyhow::Result<bool> {
    let install_rule = resolved_policy_present();
    let need_cap = !has_tun_capability(binary);
    let need_rule = install_rule && !resolve1_rule_installed();
    if !need_cap && !need_rule {
        return Ok(false);
    }
    let askpass = std::env::current_exe().context("cannot locate own executable for SUDO_ASKPASS")?;

    // Validate/refresh the sudo timestamp first (single prompt); the
    // transaction below then reuses the cached credential. Bail on cancel so
    // we do not double-prompt.
    let probe = Command::new("sudo")
        .arg("-A")
        .env("SUDO_ASKPASS", &askpass)
        .arg("-v")
        .status()
        .map_err(|error| anyhow::anyhow!("cannot run sudo (is sudo installed?): {error}"))?;
    if !probe.success() {
        anyhow::bail!("sudo authentication cancelled or failed");
    }

    let username = if install_rule {
        current_username()
            .context("cannot determine a valid invoking username for the DNS polkit rule (expected [A-Za-z0-9_.-]+)")?
    } else {
        String::new()
    };
    let script = compose_tun_setup_script(&username, install_rule);

    let status = Command::new("sudo")
        .arg("-A")
        .args(sh_c_argv(&script, binary))
        .status()
        .map_err(|error| anyhow::anyhow!("cannot run sudo (is sudo installed?): {error}"))?;
    if !status.success() {
        anyhow::bail!("sudo TUN setup transaction failed (status {status})");
    }

    if need_cap && !has_tun_capability(binary) {
        anyhow::bail!("setcap reported success but getcap still shows no capabilities");
    }
    if need_rule && !resolve1_rule_installed() {
        anyhow::bail!(
            "polkit rule write reported success but pkcheck still requires authorization (the rule may be rejected by polkit)"
        );
    }
    Ok(true)
}

/// Apply TUN capabilities and the DNS polkit rule using a password captured
/// by the TUI password popup: `sudo -S` reads the password from stdin, so no
/// askpass subprocess is needed and the popup stays rendered by the TUI.
/// Used only by the explicit Settings → TUN setup action. The password is
/// written to sudo's stdin and never logged.
pub fn apply_tun_capability_with_password(binary: &Path, password: &str) -> anyhow::Result<()> {
    use std::io::Write as _;
    use std::process::Stdio;

    let install_rule = resolved_policy_present();
    let need_cap = !has_tun_capability(binary);
    let need_rule = install_rule && !resolve1_rule_installed();
    if !need_cap && !need_rule {
        return Ok(());
    }
    let username = if install_rule {
        current_username()
            .context("cannot determine a valid invoking username for the DNS polkit rule (expected [A-Za-z0-9_.-]+)")?
    } else {
        String::new()
    };
    let script = compose_tun_setup_script(&username, install_rule);

    let mut child = Command::new("sudo")
        .arg("-S")
        .args(sh_c_argv(&script, binary))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| anyhow::anyhow!("cannot run sudo (is sudo installed?): {error}"))?;

    if let Some(mut stdin) = child.stdin.take() {
        writeln!(stdin, "{password}")?;
    }
    let output = child.wait_with_output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("sudo TUN setup failed: {}", stderr.trim());
    }
    if need_cap && !has_tun_capability(binary) {
        anyhow::bail!("setcap reported success but getcap still shows no capabilities");
    }
    if need_rule && !resolve1_rule_installed() {
        anyhow::bail!(
            "polkit rule write reported success but pkcheck still requires authorization (the rule may be rejected by polkit)"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capability_output_parse_detects_full_tun_set() {
        assert!(capability_output_has_tun(
            b"/usr/bin/verge-mihomo cap_net_admin,cap_net_raw=eip\n"
        ));
        assert!(capability_output_has_tun(b"cap_net_admin,cap_net_raw=eip"));
        assert!(capability_output_has_tun(b"cap_net_admin=eip cap_net_raw=eip"));
        assert!(capability_output_has_tun(b"cap_net_admin,cap_net_raw+epi"));
    }

    #[test]
    fn capability_output_rejects_permitted_only_flags() {
        // +p (permitted only) or missing flags must NOT satisfy the check:
        // the effective bit is what lets mihomo create the TUN device.
        assert!(!capability_output_has_tun(b"cap_net_admin+p cap_net_raw+p"));
        assert!(!capability_output_has_tun(b"cap_net_admin,cap_net_raw+p"));
        assert!(!capability_output_has_tun(b"cap_net_admin,cap_net_raw"));
        assert!(!capability_output_has_tun(b"cap_net_admin,cap_net_raw=ip"));
    }

    #[test]
    fn capability_output_rejects_partial_capability() {
        assert!(!capability_output_has_tun(b""));
        assert!(!capability_output_has_tun(b"cap_net_admin=eip only"));
        assert!(!capability_output_has_tun(b"cap_net_raw=eip only"));
        // A different admin-capable name must not count as cap_net_admin.
        assert!(!capability_output_has_tun(b"cap_sys_admin,cap_net_raw=eip"));
    }

    #[test]
    fn split_cap_token_handles_equal_plus_and_bare_forms() {
        assert_eq!(
            split_cap_token("cap_net_admin,cap_net_raw=eip"),
            (vec!["cap_net_admin", "cap_net_raw"], "eip")
        );
        assert_eq!(split_cap_token("cap_net_admin+p"), (vec!["cap_net_admin"], "p"));
        assert_eq!(split_cap_token("cap_net_admin"), (vec!["cap_net_admin"], ""));
    }

    #[test]
    fn missing_capability_error_identifies_setup_command_and_binary() {
        let path = Path::new("/tmp/verge-mihomo");
        let error = missing_capability_error(path);
        assert!(error.contains("tun setup"), "{error}");
        assert!(error.contains("/tmp/verge-mihomo"), "{error}");
        assert!(error.contains("cap_net_admin"), "{error}");
    }

    #[test]
    fn require_capability_passes_for_capable_binary() {
        let path = Path::new("/fake/mihomo");
        require_capability_impl(path, false, &|_| true).expect("capable binary must pass");
    }

    #[test]
    fn require_capability_rejects_uncapped_binary_with_guidance() {
        let path = Path::new("/fake/mihomo");
        let error = require_capability_impl(path, false, &|_| false)
            .expect_err("uncapped binary must fail")
            .to_string();
        assert!(error.contains("tun setup"), "{error}");
        assert!(error.contains("/fake/mihomo"), "{error}");
    }

    #[test]
    fn require_capability_bypasses_for_root_process() {
        let path = Path::new("/fake/mihomo");
        require_capability_impl(path, true, &|_| false).expect("root must bypass the check");
    }

    #[test]
    fn replaced_binary_error_names_the_new_path() {
        // Simulates an upgraded binary whose file capability was lost: the
        // error must name the exact path so setup can target it.
        let replaced = Path::new("/home/u/.local/share/clash-verge-cli/mihomo");
        let error = missing_capability_error(replaced);
        assert!(error.contains("/home/u/.local/share/clash-verge-cli/mihomo"), "{error}");
    }

    #[test]
    fn plain_file_has_no_tun_capability() {
        let tmp = std::env::temp_dir().join(format!("cv-cap-{}.txt", uuid::Uuid::new_v4()));
        match std::fs::write(&tmp, "x") {
            Ok(()) => {}
            Err(error) => panic!("write: {error}"),
        }
        assert!(!has_tun_capability(&tmp));
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn nonexistent_binary_has_no_capability() {
        assert!(!has_tun_capability(Path::new("/nonexistent/definitely-not-here")));
    }

    #[test]
    fn resolved_polkit_rule_renders_username_and_the_four_dns_actions() {
        let rule = resolved_polkit_rule("alice");
        assert!(
            rule.contains("subject.user == \"alice\""),
            "username must scope the rule"
        );
        assert!(rule.contains("org.freedesktop.resolve1.set-dns-servers"));
        assert!(rule.contains("org.freedesktop.resolve1.set-domains"));
        assert!(rule.contains("org.freedesktop.resolve1.set-default-route"));
        assert!(rule.contains("polkit.Result.YES"));
        assert!(
            rule.contains("polkit.addRule(function(action, subject)"),
            "rule must be a JS addRule"
        );
        assert!(
            rule.starts_with(RESOLVE1_RULE_MARKER),
            "rendered rule must carry the installed-check marker"
        );
        // A different user must not be authorized by this rule.
        assert!(!resolved_polkit_rule("bob").contains("subject.user == \"alice\""));
    }

    #[test]
    fn resolved_polkit_rule_covers_teardown_revert_action() {
        // TUN teardown (core stop / TUN toggle-off) calls
        // org.freedesktop.resolve1.revert, default policy auth_admin_keep
        // like the other three. Without it the rule would keep start silent
        // but leave stop/toggle-off prompting — the live incident.
        for username in ["alice", "user-1_x.y"] {
            let rule = resolved_polkit_rule(username);
            assert!(
                rule.contains("org.freedesktop.resolve1.revert"),
                "rule must cover the revert (teardown) action"
            );
            // revert sits inside the same user-scoped authorization branch.
            assert!(rule.contains(&format!("subject.user == \"{username}\" && ")));
        }
    }

    #[test]
    fn malicious_username_is_rejected_and_escaped() {
        // A crafted $USER must never pass validation: quotes, semicolons,
        // spaces, and control chars would otherwise let it widen the rule to
        // other users or inject JS evaluated by root polkitd.
        assert!(!is_valid_rule_username("evil\" || true || \""));
        assert!(!is_valid_rule_username("alice; rm -rf /"));
        assert!(!is_valid_rule_username("has space"));
        assert!(!is_valid_rule_username("line\nbreak"));
        assert!(!is_valid_rule_username(""));
        // Valid POSIX login names (the charset ^[A-Za-z0-9_.-]+$) pass.
        assert!(is_valid_rule_username("alice"));
        assert!(is_valid_rule_username("user-1_x.y"));

        // Even if an invalid string slipped past validation, the renderer
        // escapes the JS string so the quote stays inside the literal.
        let rule = resolved_polkit_rule("evil\" + true");
        assert!(
            rule.contains("subject.user == \"evil\\\" + true\""),
            "quote must be escaped into the JS string"
        );
        assert!(
            !rule.contains("subject.user == \"evil\" + true\""),
            "no raw unterminated quote"
        );
    }

    #[test]
    fn compose_tun_setup_script_includes_capability_and_rule_heredoc() {
        let script = compose_tun_setup_script("alice", true);
        assert!(
            script.starts_with("set -e\n"),
            "transaction must abort on first failure"
        );
        // The binary is referenced as $1 (positional argument), never inline.
        assert!(script.contains(&format!("setcap {TUN_CAPS} \"$1\"")));
        // The DNS rule is written via a quoted heredoc, chowned and chmodded.
        assert!(
            script.contains("cat > /etc/polkit-1/rules.d/50-clash-verge-cli-resolved.rules <<'CV_CLASH_VERGE_RULE'")
        );
        assert!(script.contains("subject.user == \"alice\""));
        assert!(script.contains("chown root:root /etc/polkit-1/rules.d/50-clash-verge-cli-resolved.rules"));
        assert!(script.contains("chmod 0644 /etc/polkit-1/rules.d/50-clash-verge-cli-resolved.rules"));
        // Root-side self-check: the transaction verifies the file exists with
        // our marker, so a genuine write failure aborts loudly (set -e).
        assert!(
            script.contains(&format!(
                "test -f {RESOLVE1_RULE_PATH} && grep -qF '{RESOLVE1_RULE_MARKER}' {RESOLVE1_RULE_PATH}"
            )),
            "root-side self-check must verify the written rule"
        );
        // A guard keeps the write safe even when polkit's rules dir is absent.
        assert!(script.contains("if [ -d /etc/polkit-1/rules.d ]; then"));
        assert!(script.contains("fi\n"));
    }

    #[test]
    fn compose_tun_setup_script_omits_rule_without_policy() {
        let script = compose_tun_setup_script("alice", false);
        assert!(script.contains("setcap"));
        assert!(!script.contains("polkit-1"), "no rule write on non-resolved systems");
        assert!(!script.contains("chown"), "no chown without a rule file");
        assert!(!script.contains("chmod"));
        assert!(
            !script.contains("grep -qF"),
            "no root-side self-check without a rule write"
        );
    }

    #[test]
    fn binary_path_never_enters_the_script_text() {
        // P1a regression: a `'` (or `;`, `$()`, ...) in the HOME/XDG-derived
        // binary path used to be embedded in single quotes inside the sudo
        // script. The path is now a positional argument, so the script text
        // must never contain it at all.
        let evil = "/home/x'; touch /tmp/clash-pwned; '";
        let script = compose_tun_setup_script("alice", true);
        assert!(!script.contains(evil), "script text must not contain the path");
        assert!(
            !script.contains("touch /tmp/clash-pwned"),
            "no command from the path in the script"
        );
        assert!(
            script.contains(&format!("setcap {TUN_CAPS} \"$1\"")),
            "path is referenced as $1"
        );
    }

    #[test]
    fn sh_c_argv_passes_path_as_positional_argument_not_shell_text() {
        // P1a regression at the argv level: the shell sees the path as $1
        // (after `sh -c script sh`), so even an apostrophe-laden path is
        // data, not code — and it never appears inside the `-c` script.
        let evil = Path::new("/home/x'; touch /tmp/clash-pwned; '");
        let script = compose_tun_setup_script("alice", true);
        let args = sh_c_argv(&script, evil);
        assert_eq!(args.len(), 5, "sh -c <script> sh <binary>");
        assert_eq!(args[0].to_string_lossy(), "sh");
        assert_eq!(args[1].to_string_lossy(), "-c");
        assert_eq!(args[2].to_string_lossy(), script, "the script is the -c argument");
        assert_eq!(args[3].to_string_lossy(), "sh", "$0 for the script");
        assert_eq!(
            args[4].to_string_lossy(),
            evil.to_string_lossy(),
            "the path is its own argv element ($1), untouched by the shell"
        );
        assert!(
            !script.contains("touch /tmp/clash-pwned"),
            "path stays out of the script text"
        );
    }

    #[test]
    fn rule_file_state_distinguishes_marker_present_absent_and_unreadable() {
        // P2 regression: foreign/stale content is NOT installed (rewrite on
        // next setup), our marker counts as installed, ENOENT is genuinely
        // absent.
        let tmp = std::env::temp_dir().join(format!("cv-rule-{}.rules", uuid::Uuid::new_v4()));
        match std::fs::write(&tmp, "polkit.addRule(function() { return polkit.Result.NO; });\n") {
            Ok(()) => {}
            Err(error) => panic!("write foreign rule: {error}"),
        }
        assert_eq!(rule_file_state(&tmp), RuleFileState::Absent, "foreign rule is not ours");

        assert_eq!(
            rule_file_state(Path::new("/nonexistent/rule.rules")),
            RuleFileState::Absent,
            "ENOENT is genuinely absent"
        );

        match std::fs::write(&tmp, resolved_polkit_rule("alice")) {
            Ok(()) => {}
            Err(error) => panic!("write our rule: {error}"),
        }
        assert_eq!(
            rule_file_state(&tmp),
            RuleFileState::Present,
            "our rule counts as installed"
        );

        // A stale file that still contains the marker counts as ours (no
        // needless rewrite), but truncated/garbage content does not.
        match std::fs::write(&tmp, format!("{RESOLVE1_RULE_MARKER}\n// stale-but-ours\n")) {
            Ok(()) => {}
            Err(error) => panic!("write stale rule: {error}"),
        }
        assert_eq!(rule_file_state(&tmp), RuleFileState::Present);
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn rule_file_state_unreadable_is_indeterminate_not_missing() {
        // EACCES false-negative regression: a rule file that cannot be READ
        // (root-owned `rules.d`, drwxr-x--- root:polkitd) must never be
        // reported as "missing". A directory is unreadable as a file the
        // same way EACCES is — both must yield Indeterminate, not Absent.
        let dir = std::env::temp_dir().join(format!("cv-rule-dir-{}", uuid::Uuid::new_v4()));
        match std::fs::create_dir_all(&dir) {
            Ok(()) => {}
            Err(error) => panic!("create dir: {error}"),
        }
        assert_eq!(
            rule_file_state(&dir),
            RuleFileState::Indeterminate,
            "unreadable path must be indeterminate, never missing"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pkcheck_exit_codes_map_to_authorization_verdicts() {
        assert_eq!(pkcheck_verdict(Some(0)), PkcheckVerdict::Authorized);
        assert_eq!(pkcheck_verdict(Some(1)), PkcheckVerdict::NotAuthorized);
        // Any other exit (pkcheck error, unknown action) or a signal-killed
        // process (no exit code) is indeterminate — never "missing".
        assert_eq!(pkcheck_verdict(Some(2)), PkcheckVerdict::Indeterminate);
        assert_eq!(pkcheck_verdict(Some(127)), PkcheckVerdict::Indeterminate);
        assert_eq!(pkcheck_verdict(None), PkcheckVerdict::Indeterminate);
    }

    #[test]
    fn resolve1_auth_probe_prefers_pkcheck_and_treats_eacces_as_indeterminate() {
        use PkcheckVerdict::{Authorized, Indeterminate, NotAuthorized};
        use Resolve1Auth as Auth;

        // pkcheck authorizes → authorized even when the file is unreadable
        // (the EACCES incident: rules.d denies reads but the rule works).
        assert_eq!(
            resolve1_auth_probe_impl(Some(Authorized), RuleFileState::Indeterminate),
            Auth::Authorized
        );
        // pkcheck requires auth → not authorized even if the file marker is
        // present (rule ineffective/rejected by polkit).
        assert_eq!(
            resolve1_auth_probe_impl(Some(NotAuthorized), RuleFileState::Present),
            Auth::NotAuthorized
        );
        // pkcheck errored → indeterminate, NO file fallback (per spec).
        assert_eq!(
            resolve1_auth_probe_impl(Some(Indeterminate), RuleFileState::Present),
            Auth::Indeterminate
        );

        // pkcheck unavailable → file-marker fallback.
        assert_eq!(resolve1_auth_probe_impl(None, RuleFileState::Present), Auth::Authorized);
        assert_eq!(
            resolve1_auth_probe_impl(None, RuleFileState::Absent),
            Auth::NotAuthorized
        );
        // EACCES fallback → indeterminate, never "missing".
        assert_eq!(
            resolve1_auth_probe_impl(None, RuleFileState::Indeterminate),
            Auth::Indeterminate
        );
    }

    #[test]
    fn resolve1_rule_installed_never_false_on_indeterminate() {
        // The bool consumed by the setup decision and the s-path preflight:
        // only a genuine NotAuthorized counts as missing. Indeterminate
        // (EACCES / pkcheck error) must be treated as installed so a
        // completed transaction never aborts and the prompt never reappears
        // after pkcheck authorizes.
        assert!(resolve1_rule_installed_impl(Resolve1Auth::Authorized));
        assert!(
            resolve1_rule_installed_impl(Resolve1Auth::Indeterminate),
            "EACCES/indeterminate must not report missing"
        );
        assert!(!resolve1_rule_installed_impl(Resolve1Auth::NotAuthorized));
    }

    #[test]
    fn resolve1_rule_needed_matches_all_conditions() {
        // Only TUN enabled + non-root + policy present + rule absent needs it.
        assert!(resolve1_rule_needed_impl(true, false, true, false));
        assert!(
            !resolve1_rule_needed_impl(false, false, true, false),
            "TUN off → no rule"
        );
        assert!(
            !resolve1_rule_needed_impl(true, true, true, false),
            "root bypasses polkit"
        );
        assert!(
            !resolve1_rule_needed_impl(true, false, false, false),
            "no resolved policy → not needed"
        );
        assert!(
            !resolve1_rule_needed_impl(true, false, true, true),
            "rule already installed"
        );
    }

    #[test]
    fn missing_resolve1_rule_warning_names_the_setup_command() {
        let warning = missing_resolve1_rule_warning();
        assert!(warning.contains("tun setup"), "warning must direct the user to setup");
        assert!(warning.contains("polkit"), "warning must name the polkit rule");
    }
}
