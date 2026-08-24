//! Runtime config write/reload primitives shared by the TUI event loop and the
//! headless daemon. Extracted from `tui/event_loop.rs` so both modes reuse one
//! implementation of backup → build → write → reload/rollback.

use std::sync::LazyLock;

use tokio::sync::Mutex;

/// Serializes all runtime-config read-modify-write sequences (mode switches,
/// TUN toggles, profile commits) across TUI/daemon tasks.
pub static RUNTIME_CONFIG_IO: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

/// Reload the running mihomo core from a config file via `PUT /configs`.

/// How a committed config reaches the running core (task 3.4).
/// Consumed as call sites migrate from direct CoreKind checks; kept
/// public so the strategy model has one home.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReloadStrategy {
    /// mihomo: `PUT /configs` hot reload; roll the file back on rejection.
    HotReload,
    /// sing-box: `PUT /configs` is a no-op — prevalidate with
    /// `sing-box check`, then restart the process; the spawn-path
    /// readiness probe (manager task 3.3) confirms the new config came up.
    Restart,
}

#[allow(dead_code)]
impl ReloadStrategy {
    pub fn for_core(kind: crate::mihomo_manager::CoreKind) -> Self {
        match kind {
            crate::mihomo_manager::CoreKind::Mihomo => Self::HotReload,
            crate::mihomo_manager::CoreKind::SingBox => Self::Restart,
        }
    }
}

/// Pre-validate a sing-box config without starting the core
/// (`sing-box check -c`). Runs before any restart so most bad configs
/// are rejected while the old one is still running.
pub async fn prevalidate_singbox_config(binary: &std::path::Path, config: &std::path::Path) -> Result<(), String> {
    let output = tokio::process::Command::new(binary)
        .arg("check")
        .arg("-c")
        .arg(config)
        .output()
        .await
        .map_err(|error| format!("failed to run {} check: {error}", binary.display()))?;
    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        Err(format!(
            "sing-box check rejected {}: {stderr}{stdout}",
            config.display()
        ))
    }
}

/// Sing-box ReloadStrategy application (task 3.4): regenerate the runtime
/// config, prevalidate it while the old core is still serving, then restart
/// through the manager (barrier + readiness probe inside). On a failed
/// restart the previous config file is restored and one fallback restart
/// is attempted.
///
/// Task 7.5: a single assembled generation pass covers nodes/groups, route
/// rules (profile rules + stored logical rules), rule-sets and structured
/// DNS - every save lands in one restart instead of several.
pub async fn apply_singbox_restart(
    manager: &crate::mihomo_manager::MihomoManager,
    config_yaml: Option<&str>,
    enable_tun: bool,
) -> Result<String, String> {
    let _guard = RUNTIME_CONFIG_IO.lock().await;
    let config_path = clash_verge_core::utils::dirs::singbox_config_path().map_err(|e| e.to_string())?;
    let previous = tokio::fs::read(&config_path).await.ok();

    let (_path, parts) =
        crate::mihomo_manager::ManagerInner::write_singbox_assembled(manager.config_dir(), config_yaml, enable_tun)
            .await
            .map_err(|e| e.to_string())?;

    let Some(binary) = crate::mihomo_manager::singbox_binary::candidate_without_install() else {
        return Err("sing-box binary not found".into());
    };
    // Reject bad configs BEFORE stopping the running core.
    prevalidate_singbox_config(&binary, &config_path).await?;

    if let Err(restart_error) = manager.restart().await {
        if let Some(previous) = previous {
            let _ = tokio::fs::write(&config_path, &previous).await;
            let _ = manager.restart().await; // best-effort fallback to old config
        }
        return Err(restart_error.to_string());
    }

    // Human-readable degradation report for the status bar.
    let report = if parts.profile_used {
        format!(
            "sing-box: {} nodes, {} skipped, {} fields degraded",
            parts.conversion.outbounds.len(),
            parts.conversion.skipped.len(),
            parts.conversion.degraded.len()
        )
    } else {
        "sing-box: skeleton config applied".into()
    };
    Ok(report)
}

/// Task 8.1/7.5 helper: regenerate from the ACTIVE profile (not a caller
/// snapshot) and restart sing-box so DNS/rule-set edits take effect.
pub async fn apply_singbox_active_reload(manager: &crate::mihomo_manager::MihomoManager) -> Result<String, String> {
    let yaml = crate::mihomo_manager::ManagerInner::active_profile_yaml().await;
    let enable_tun = crate::mihomo_manager::manager::runtime_tun_enabled()
        .await
        .unwrap_or(false);
    apply_singbox_restart(manager, yaml.as_deref(), enable_tun).await
}

pub async fn reload_config_file(api: &crate::mihomo_api::MihomoApi, path: &std::path::Path) -> Result<(), String> {
    let config_path = path
        .to_str()
        .ok_or_else(|| format!("config path is not valid UTF-8: {}", path.display()))?;
    let response = api
        .client
        .put("http://localhost/configs?force=true")
        .json(&serde_json::json!({ "path": config_path, "payload": "" }))
        .send()
        .await
        .map_err(|error| error.to_string())?;

    if response.status().is_success() {
        Ok(())
    } else {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        Err(format!("Mihomo rejected config reload ({status}): {body}"))
    }
}

/// Regenerate the runtime config from a refreshed remote profile and reload it.
pub async fn reload_remote_profile(
    api: &crate::mihomo_api::MihomoApi,
    item: &clash_verge_core::config::PrfItem,
    enable_tun: bool,
    core_running: bool,
) -> Result<(), String> {
    let file = item
        .file
        .as_deref()
        .ok_or_else(|| "remote profile is missing file".to_string())?;
    let profile_path = clash_verge_core::utils::dirs::app_profiles_dir()
        .map_err(|error| error.to_string())?
        .join(file);
    if !profile_path.exists() {
        return Err(format!("profile file not found: {}", profile_path.display()));
    }

    let raw = tokio::fs::read_to_string(&profile_path)
        .await
        .map_err(|error| format!("failed to read {}: {error}", profile_path.display()))?;
    let profile: serde_yaml_ng::Mapping = serde_yaml_ng::from_str(&raw)
        .map_err(|error| format!("invalid YAML in {}: {error}", profile_path.display()))?;

    // Control-plane snapshot happens inside commit_runtime_config under the IO lock.
    commit_runtime_config(api, enable_tun, core_running, Some(item), |app_config| {
        let control_plane = crate::enhance::snapshot_control_plane(&app_config);
        Ok(crate::enhance::enforce_control_plane(profile, control_plane))
    })
    .await?;
    Ok(())
}

/// Restore the user's saved node selection into the running core after a reload.
pub async fn restore_selected_nodes(api: &crate::mihomo_api::MihomoApi, item: &clash_verge_core::config::PrfItem) {
    let Some(selected) = item.selected.as_ref() else {
        return;
    };
    for entry in selected {
        let Some(group) = entry.name.as_deref() else {
            continue;
        };
        let Some(node) = entry.now.as_deref() else {
            continue;
        };
        if let Err(error) = api.select_proxy(group, node).await {
            tracing::debug!(target: "profile", "restore selected {group}/{node}: {error}");
        }
    }
}

/// Write runtime config under the shared IO lock (no reload).
pub async fn write_runtime_config(
    config: serde_yaml_ng::Mapping,
    enable_tun: bool,
) -> Result<std::path::PathBuf, String> {
    let _guard = RUNTIME_CONFIG_IO.lock().await;
    write_runtime_config_unlocked(config, enable_tun).await
}

/// Backup → build (from a fresh on-disk snapshot) → write → reload/rollback.
///
/// `build` receives the latest `clash.yaml` mapping while `RUNTIME_CONFIG_IO` is held,
/// so concurrent mode/TUN commits are not overwritten by a stale pre-lock snapshot.
pub async fn commit_runtime_config<F>(
    api: &crate::mihomo_api::MihomoApi,
    enable_tun: bool,
    core_running: bool,
    restore_item: Option<&clash_verge_core::config::PrfItem>,
    build: F,
) -> Result<std::path::PathBuf, String>
where
    F: FnOnce(serde_yaml_ng::Mapping) -> Result<serde_yaml_ng::Mapping, String>,
{
    let _guard = RUNTIME_CONFIG_IO.lock().await;
    let app_config = clash_verge_core::config::IClashTemp::new().await.0;
    let config = build(app_config)?;
    let path = clash_verge_core::utils::dirs::clash_path().map_err(|error| error.to_string())?;
    let previous = if core_running && path.exists() {
        Some(
            tokio::fs::read(&path)
                .await
                .map_err(|error| format!("failed to back up {}: {error}", path.display()))?,
        )
    } else {
        None
    };

    write_runtime_config_unlocked(config, enable_tun).await?;
    if !core_running {
        // Keep the newly selected runtime config for the next Start; do not API-reload
        // (or roll it back) while no controller is available.
        return Ok(path);
    }
    if let Err(error) = reload_config_file(api, &path).await {
        if let Some(previous) = previous {
            let _ = tokio::fs::write(&path, previous).await;
            let _ = reload_config_file(api, &path).await;
            return Err(format!("{error}; restored the previous config"));
        }
        return Err(error);
    }
    if let Some(item) = restore_item {
        restore_selected_nodes(api, item).await;
    }
    Ok(path)
}

pub async fn write_runtime_config_unlocked(
    mut config: serde_yaml_ng::Mapping,
    enable_tun: bool,
) -> Result<std::path::PathBuf, String> {
    config = crate::enhance::prepare_runtime_config(config, enable_tun);
    let yaml = serde_yaml_ng::to_string(&config).map_err(|error| error.to_string())?;
    let path = clash_verge_core::utils::dirs::clash_path().map_err(|error| error.to_string())?;
    let temporary_path = path.with_extension(format!("yaml.tui-runtime.{}.tmp", uuid::Uuid::new_v4()));

    #[cfg(unix)]
    let permissions = {
        use std::os::unix::fs::PermissionsExt;
        if tokio::fs::try_exists(&path).await.unwrap_or(false) {
            tokio::fs::metadata(&path).await.ok().map(|meta| meta.permissions())
        } else {
            Some(std::fs::Permissions::from_mode(0o600))
        }
    };

    tokio::fs::write(&temporary_path, yaml)
        .await
        .map_err(|error| format!("failed to stage {}: {error}", temporary_path.display()))?;

    #[cfg(unix)]
    if let Some(permissions) = permissions {
        tokio::fs::set_permissions(&temporary_path, permissions)
            .await
            .map_err(|error| format!("failed to set permissions on {}: {error}", temporary_path.display()))?;
    }

    #[cfg(windows)]
    if tokio::fs::try_exists(&path).await.unwrap_or(false) {
        tokio::fs::remove_file(&path)
            .await
            .map_err(|error| format!("failed to remove {}: {error}", path.display()))?;
    }
    tokio::fs::rename(&temporary_path, &path)
        .await
        .map_err(|error| format!("failed to replace {}: {error}", path.display()))?;
    Ok(path)
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn strategy_maps_from_core_kind() {
        assert_eq!(
            ReloadStrategy::for_core(crate::mihomo_manager::CoreKind::Mihomo),
            ReloadStrategy::HotReload
        );
        assert_eq!(
            ReloadStrategy::for_core(crate::mihomo_manager::CoreKind::SingBox),
            ReloadStrategy::Restart
        );
    }

    #[tokio::test]
    async fn prevalidate_passes_on_zero_exit() {
        // /bin/true ignores arguments and exits 0 — stands in for a
        // sing-box binary accepting the config.
        let config = std::env::temp_dir().join("rv-fake-config.json");
        std::fs::write(&config, "{}").expect("write");
        prevalidate_singbox_config(std::path::Path::new("/bin/true"), &config)
            .await
            .expect("/bin/true must pass");
        let _ = std::fs::remove_file(&config);
    }

    #[tokio::test]
    async fn prevalidate_surfaces_stderr_on_failure() {
        let config = std::env::temp_dir().join("rv-fake-bad.json");
        std::fs::write(&config, "{}").expect("write");
        let err = prevalidate_singbox_config(std::path::Path::new("/bin/false"), &config)
            .await
            .expect_err("/bin/false always fails");
        assert!(err.contains(&config.display().to_string()), "{err}");
        let _ = std::fs::remove_file(&config);
    }
}
