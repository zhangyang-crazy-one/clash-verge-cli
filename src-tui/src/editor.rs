//! Suspend the TUI and open a YAML file in the user's `$EDITOR`.
//!
//! After the editor exits, the file is validated as YAML. If parsing fails
//! and a pre-edit snapshot was taken, the snapshot is restored.

use std::path::Path;

use anyhow::Context;

use crate::tui::TerminalGuard;

/// Pick the user's preferred editor: `$VISUAL`, then `$EDITOR`, then `vi`.
fn pick_editor() -> String {
    for var in ["VISUAL", "EDITOR"] {
        if let Ok(val) = std::env::var(var)
            && !val.is_empty()
        {
            return val;
        }
    }
    "vi".to_string()
}

/// Suspend the TUI, run `$EDITOR <path>`, and resume the TUI.
/// Returns an error if the editor process fails but NOT if the user exits
/// with non-zero (that's normal for "no changes").
pub fn edit_file_blocking(guard: &mut TerminalGuard, path: &Path) -> anyhow::Result<()> {
    let editor = pick_editor();

    // Suspend: leave alternate screen, disable raw mode, show cursor.
    guard.suspend()?;

    let result = std::process::Command::new(&editor)
        .arg(path.as_os_str())
        .status()
        .with_context(|| format!("failed to launch editor '{editor}'"));

    // Resume before checking the result so the TUI always restores.
    guard.resume()?;

    match result {
        Ok(status) if status.success() => Ok(()),
        Ok(status) => {
            // Non-zero exit is normal (e.g. vim :cq), not a TUI error.
            tracing::info!(target: "editor", "{editor} exited with {status}");
            Ok(())
        }
        Err(e) => Err(e),
    }
}

/// Validate a YAML file. Returns `Ok(())` if the file contains valid YAML.
pub fn validate_yaml(path: &Path) -> anyhow::Result<()> {
    let contents = std::fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    serde_yaml_ng::from_str::<serde_yaml_ng::Value>(&contents)
        .map(|_| ())
        .context("invalid YAML")
}

/// Snapshot a file's contents for rollback on invalid edits.
pub fn snapshot(path: &Path) -> anyhow::Result<Vec<u8>> {
    std::fs::read(path).with_context(|| format!("failed to snapshot {}", path.display()))
}

/// Restore a snapshot to the given path.
pub fn restore_snapshot(path: &Path, data: &[u8]) -> anyhow::Result<()> {
    std::fs::write(path, data).with_context(|| format!("failed to restore {}", path.display()))
}

/// Task 8.5: validate a JSON config file (sing-box runtime config).
pub fn validate_json(path: &Path) -> anyhow::Result<()> {
    let body = std::fs::read_to_string(path)?;
    serde_json::from_str::<serde_json::Value>(&body)
        .map(|_| ())
        .map_err(|e| anyhow::anyhow!("invalid JSON in {}: {e}", path.display()))
}

/// Task 8.5: prevalidate via `sing-box check -c` when a binary is
/// available; falls back to pure JSON parsing otherwise.
pub fn validate_singbox(path: &Path) -> anyhow::Result<()> {
    validate_json(path)?;
    if let Some(binary) =
        crate::mihomo_manager::singbox_binary::candidate_without_install()
    {
        let output = std::process::Command::new(&binary)
            .arg("check")
            .arg("-c")
            .arg(path)
            .output()
            .map_err(|e| anyhow::anyhow!("failed to run sing-box check: {e}"))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("sing-box check rejected the config: {stderr}");
        }
    }
    Ok(())
}
