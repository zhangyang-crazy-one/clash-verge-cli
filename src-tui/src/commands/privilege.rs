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

/// Explicit foreground apply: `sudo -A setcap` with `SUDO_ASKPASS` pointed
/// at this binary's hidden `askpass` subcommand.
///
/// Returns `Ok(false)` when the binary is already capable (no sudo invoked),
/// `Ok(true)` when the capability was applied just now. This is the only
/// CLI path that executes sudo for TUN privileges.
pub fn apply_tun_capability(binary: &Path) -> anyhow::Result<bool> {
    if has_tun_capability(binary) {
        return Ok(false);
    }
    let askpass = std::env::current_exe().context("cannot locate own executable for SUDO_ASKPASS")?;

    // Validate/refresh the sudo timestamp first (single prompt); the setcap
    // call below then reuses the cached credential. Bail on cancel so we do
    // not double-prompt.
    let probe = Command::new("sudo")
        .arg("-A")
        .env("SUDO_ASKPASS", &askpass)
        .arg("-v")
        .status()
        .map_err(|error| anyhow::anyhow!("cannot run sudo (is sudo installed?): {error}"))?;
    if !probe.success() {
        anyhow::bail!("sudo authentication cancelled or failed");
    }

    let status = Command::new("sudo")
        .args(["setcap", TUN_CAPS])
        .arg(binary)
        .status()
        .map_err(|error| anyhow::anyhow!("cannot run setcap (is libcap installed?): {error}"))?;
    if !status.success() {
        anyhow::bail!("sudo setcap failed (status {status})");
    }

    if !has_tun_capability(binary) {
        anyhow::bail!("setcap reported success but getcap still shows no capabilities");
    }
    Ok(true)
}

/// Apply TUN capabilities using a password captured by the TUI password
/// popup: `sudo -S` reads the password from stdin, so no askpass subprocess
/// is needed and the popup stays rendered by the TUI. Used only by the
/// explicit Settings → TUN setup action. The password is written to sudo's
/// stdin and never logged.
pub fn apply_tun_capability_with_password(binary: &Path, password: &str) -> anyhow::Result<()> {
    use std::io::Write as _;
    use std::process::Stdio;

    let mut child = Command::new("sudo")
        .args(["-S", "setcap", TUN_CAPS])
        .arg(binary)
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
        anyhow::bail!("sudo setcap failed: {}", stderr.trim());
    }
    if !has_tun_capability(binary) {
        anyhow::bail!("setcap reported success but getcap still shows no capabilities");
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
}
