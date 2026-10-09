//! Runtime config write/reload primitives shared by the TUI event loop and the
//! headless daemon. Extracted from `tui/event_loop.rs` so both modes reuse one
//! implementation of backup → build → write → reload/rollback.

use std::sync::LazyLock;

use tokio::sync::Mutex;

/// Serializes all runtime-config read-modify-write sequences (mode switches,
/// TUN toggles, profile commits) across TUI/daemon tasks.
pub static RUNTIME_CONFIG_IO: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

/// How a committed config reaches the running core (task 3.4): reload the
/// running mihomo core from a config file via `PUT /configs`, or restart
/// the process for a core where hot reload is a no-op. Consumed as call
/// sites migrate from direct CoreKind checks; kept public so the strategy
/// model has one home.
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
    apply_singbox_restart_for_profile(manager, config_yaml, enable_tun, None).await
}

/// Profile-aware variant used by subscription refresh. DNS confirmation is
/// captured from the effective candidate before writing and persisted only
/// after the candidate has been committed and (when running) reloaded.
pub async fn apply_singbox_restart_for_profile(
    manager: &crate::mihomo_manager::MihomoManager,
    config_yaml: Option<&str>,
    enable_tun: bool,
    profile_uid: Option<&str>,
) -> Result<String, String> {
    let _guard = RUNTIME_CONFIG_IO.lock().await;
    let core_running = manager.state() == crate::app::CoreState::Running;
    if core_running && !manager.owns_child() {
        return Err("cannot apply sing-box settings: controller is attached to an externally managed core; reconfigure it through its owner".into());
    }
    let mut prepared_yaml = None;
    let mut dns_state = None;
    if let (Some(uid), Some(raw)) = (profile_uid, config_yaml)
        && !crate::subscribe::from_url::is_singbox_json_profile(raw)
    {
        let mapping = serde_yaml_ng::from_str(raw).map_err(|error| format!("invalid profile YAML: {error}"))?;
        let (mapping, state) = crate::services::profile::prepare_profile_dns_from_settings(uid, mapping).await?;
        prepared_yaml = Some(serde_yaml_ng::to_string(&mapping).map_err(|error| error.to_string())?);
        dns_state = Some(state);
    }
    let config_yaml = prepared_yaml.as_deref().or(config_yaml);
    let config_path = clash_verge_core::utils::dirs::singbox_config_path().map_err(|e| e.to_string())?;
    let previous = match tokio::fs::read(&config_path).await {
        Ok(bytes) => Some(bytes),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(format!("cannot read {}: {error}", config_path.display())),
    };
    let candidate = private_candidate_path(&config_path);
    let mut transaction = RuntimeCandidate::new(config_path.clone(), candidate.clone(), previous);
    let assembled = crate::mihomo_manager::ManagerInner::write_singbox_assembled_to(
        manager.config_dir(),
        config_yaml,
        enable_tun,
        &candidate,
    )
    .await;
    let (_candidate_path, parts) = match assembled {
        Ok(parts) => parts,
        Err(error) => {
            let _ = tokio::fs::remove_file(&candidate).await;
            return Err(error.to_string());
        }
    };

    let binary = match manager.binary_path() {
        Some(binary) => binary,
        None => {
            crate::mihomo_manager::singbox_binary::resolve_or_install()
                .await
                .map_err(|error| error.to_string())?
                .path
        }
    };
    // Reject bad configs before replacing the formal file or stopping the
    // running core. Candidate and formal configs live on the same filesystem.
    if let Err(error) = prevalidate_singbox_config(&binary, &candidate).await {
        let _ = tokio::fs::remove_file(&candidate).await;
        return Err(error);
    }
    // An adopted pid identifies a running external core but does not grant
    // this process restart ownership. Refreshes may persist while stopped,
    // while live sing-box replacement requires a child owned by this manager.
    transaction.install()?;

    // Keep the validated durable config current while stopped, but never turn
    // a refresh/settings write into an implicit core start.
    if !core_running {
        transaction.commit();
        persist_dns_override_state(dns_state.as_ref()).await;
        return Ok(if parts.profile_used {
            format!(
                "sing-box: {} nodes, {} skipped, {} fields degraded (saved; core stopped)",
                parts.conversion.outbounds.len(),
                parts.conversion.skipped.len(),
                parts.conversion.degraded.len()
            )
        } else {
            "sing-box: skeleton config saved (core stopped)".into()
        });
    }

    if let Err(restart_error) = manager.restart().await {
        transaction
            .rollback()
            .map_err(|rollback_error| format!("{restart_error}; rollback failed: {rollback_error}"))?;
        if transaction.previous.is_some() {
            if let Err(rollback_error) = manager.restart().await {
                return Err(format!(
                    "{restart_error}; previous configuration restored but fallback restart failed: {rollback_error}"
                ));
            }
        }
        return Err(restart_error.to_string());
    }

    transaction.commit();
    persist_dns_override_state(dns_state.as_ref()).await;

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

async fn persist_dns_override_state(state: Option<&clash_verge_core::config::DnsOverrideState>) {
    let Some(state) = state else { return };
    if let Err(error) = clash_verge_core::config::IVerge::persist_dns_override_after_apply(state).await {
        tracing::warn!(target: "config", "DNS override applied but confirmation persistence failed: {error}");
    }
}

fn private_candidate_path(config_path: &std::path::Path) -> std::path::PathBuf {
    config_path.with_file_name(format!(
        ".singbox.candidate-{}-{}.json",
        std::process::id(),
        uuid::Uuid::new_v4()
    ))
}

/// Own staged files and restore the previous bytes if an apply future is
/// cancelled after installation. Rename and state recording contain no await.
struct RuntimeCandidate {
    formal: std::path::PathBuf,
    candidate: std::path::PathBuf,
    previous: Option<Vec<u8>>,
    installed: bool,
}

impl RuntimeCandidate {
    fn new(formal: std::path::PathBuf, candidate: std::path::PathBuf, previous: Option<Vec<u8>>) -> Self {
        Self {
            formal,
            candidate,
            previous,
            installed: false,
        }
    }
    fn install(&mut self) -> Result<(), String> {
        std::fs::rename(&self.candidate, &self.formal)
            .map_err(|error| format!("failed to atomically install validated sing-box config: {error}"))?;
        self.installed = true;
        Ok(())
    }
    fn commit(&mut self) {
        self.installed = false;
    }
    fn rollback(&mut self) -> Result<(), String> {
        if !self.installed {
            return Ok(());
        }
        if let Some(previous) = &self.previous {
            use std::io::Write as _;
            let mut staged =
                tempfile::NamedTempFile::new_in(self.formal.parent().ok_or("runtime config has no parent")?)
                    .map_err(|error| error.to_string())?;
            staged.write_all(previous).map_err(|error| error.to_string())?;
            staged.as_file().sync_all().map_err(|error| error.to_string())?;
            staged.persist(&self.formal).map_err(|error| error.to_string())?;
        } else {
            match std::fs::remove_file(&self.formal) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.to_string()),
            }
        }
        self.installed = false;
        Ok(())
    }
}

impl Drop for RuntimeCandidate {
    fn drop(&mut self) {
        if let Err(error) = self.rollback() {
            tracing::error!(target: "config", "cancelled config apply rollback failed: {error}");
        }
        let _ = std::fs::remove_file(&self.candidate);
    }
}

/// Task 8.1/7.5 helper: regenerate from the ACTIVE profile (not a caller
/// snapshot) and restart sing-box so DNS/rule-set edits take effect.
pub async fn apply_singbox_active_reload(manager: &crate::mihomo_manager::MihomoManager) -> Result<String, String> {
    let yaml = crate::mihomo_manager::ManagerInner::active_profile_yaml()
        .await
        .map_err(|error| error.to_string())?;
    let enable_tun = crate::mihomo_manager::manager::runtime_tun_enabled()
        .await
        .unwrap_or(false);
    apply_singbox_restart(manager, yaml.as_deref(), enable_tun).await
}

pub async fn reload_config_file(api: &crate::mihomo_api::MihomoApi, path: &std::path::Path) -> Result<(), String> {
    let config_path = path
        .to_str()
        .ok_or_else(|| format!("config path is not valid UTF-8: {}", path.display()))?;
    let mut endpoint = api.path_url(&["configs"]).map_err(|error| error.to_string())?;
    endpoint.query_pairs_mut().append_pair("force", "true");
    let response = api
        .client
        .put(endpoint)
        .json(&serde_json::json!({ "path": config_path, "payload": "" }))
        .send()
        .await
        .map_err(|error| error.to_string())?;

    if response.status().is_success() {
        api.version()
            .await
            .map_err(|error| format!("config reload accepted but controller readiness failed: {error}"))?;
        Ok(())
    } else {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        Err(format!("Mihomo rejected config reload ({status}): {body}"))
    }
}

/// Read a remote profile's YAML from disk and compose its configured rules
/// fragment (`option.rules`) before returning the parsed mapping.
///
/// Returns an error early when the fragment reference or file is broken, so
/// callers never write or reload the runtime config on top of a silently
/// discarded local override.
pub async fn load_remote_profile_with_rules(
    item: &clash_verge_core::config::PrfItem,
) -> Result<serde_yaml_ng::Mapping, String> {
    let profiles_dir = clash_verge_core::utils::dirs::app_profiles_dir().map_err(|error| error.to_string())?;
    let all_items = crate::profile_store::store::ProfileStore::snapshot()
        .await
        .map_err(|error| error.to_string())?
        .all_items();
    let mut app_config = clash_verge_core::config::IClashTemp::new().await.0;
    crate::enhance::apply_verge_ports(&mut app_config).await;
    let verge = clash_verge_core::config::IVerge::new().await;
    app_config = crate::enhance::use_tun(app_config, verge.enable_tun_mode.unwrap_or(false));
    compose_profile_with_controls(item, &profiles_dir, &all_items, Some(&app_config)).await
}

/// Compose the runtime mapping for a remote profile: the upstream profile
/// with its configured rules fragment applied on top.
///
/// `all_items` resolves `option.rules` — a profile UID — to the fragment
/// item carrying the on-disk `file` name. With no configured rules fragment
/// the upstream profile is returned unchanged.
#[cfg(test)]
async fn compose_remote_profile(
    item: &clash_verge_core::config::PrfItem,
    profiles_dir: &std::path::Path,
    all_items: &[clash_verge_core::config::PrfItem],
) -> Result<serde_yaml_ng::Mapping, String> {
    compose_profile_with_controls(item, profiles_dir, all_items, None).await
}

async fn compose_profile_with_controls(
    item: &clash_verge_core::config::PrfItem,
    profiles_dir: &std::path::Path,
    all_items: &[clash_verge_core::config::PrfItem],
    app_controls: Option<&serde_yaml_ng::Mapping>,
) -> Result<serde_yaml_ng::Mapping, String> {
    let file = item
        .file
        .as_deref()
        .ok_or_else(|| "remote profile is missing file".to_string())?;
    let profile_path = profiles_dir.join(file);
    if !profile_path.exists() {
        return Err(format!("profile file not found: {}", profile_path.display()));
    }

    let raw = tokio::fs::read_to_string(&profile_path)
        .await
        .map_err(|error| format!("failed to read {}: {error}", profile_path.display()))?;
    let mut profile: serde_yaml_ng::Mapping = serde_yaml_ng::from_str(&raw)
        .map_err(|error| format!("invalid YAML in {}: {error}", profile_path.display()))?;

    let option = item.option.as_ref();
    let profile_name = item.name.as_deref().unwrap_or_default();
    // GUI v2.5.7 runs sequence fragments first, global Merge/Script next,
    // then profile Merge/Script. Absent options resolve the literal default
    // UIDs, including applying global Script again as the profile default.
    let steps = [
        ("rules", "Rules", option.and_then(|option| option.rules.as_deref())),
        (
            "proxies",
            "Proxies",
            option.and_then(|option| option.proxies.as_deref()),
        ),
        ("groups", "Groups", option.and_then(|option| option.groups.as_deref())),
        ("merge", "Merge", None),
        ("script", "Script", None),
        ("merge", "Merge", option.and_then(|option| option.merge.as_deref())),
        ("script", "Script", option.and_then(|option| option.script.as_deref())),
    ];
    let mut authority = None;
    for (index, (expected_type, default_uid, configured_uid)) in steps.into_iter().enumerate() {
        if index == 3
            && let Some(app) = app_controls
        {
            let mut control = crate::enhance::snapshot_control_plane(app);
            let mut tun = profile
                .get("tun")
                .and_then(serde_yaml_ng::Value::as_mapping)
                .cloned()
                .unwrap_or_default();
            tun.extend(
                app.get("tun")
                    .and_then(serde_yaml_ng::Value::as_mapping)
                    .cloned()
                    .unwrap_or_default(),
            );
            control.insert("tun".into(), tun.into());
            profile = crate::enhance::enforce_control_plane(profile, control);
            // DNS source confirmation and its overlay remain at the runtime
            // transaction boundary after hooks, exactly once. Applying them
            // here would re-hash the overlaid DNS later and disable its own
            // saved confirmation.
            authority = Some(crate::enhance::snapshot_control_plane(&profile));
        }
        let uid = configured_uid.unwrap_or(default_uid);
        let fragment = all_items.iter().find(|entry| entry.uid.as_deref() == Some(uid));
        let Some(fragment) = fragment else {
            if configured_uid.is_some() {
                return Err(format!(
                    "{expected_type} fragment profile not found for option.{expected_type}={uid}"
                ));
            }
            continue;
        };
        if fragment.itype.as_deref() != Some(expected_type) {
            return Err(format!(
                "{expected_type} fragment profile {uid} has wrong type; expected {expected_type}"
            ));
        }
        let chain = crate::chain::resolve_chain(fragment, profiles_dir)
            .await
            .map_err(|error| format!("{error:#}"))?;
        crate::chain::apply_chain_to_profile(&mut profile, &chain, profile_name)
            .map_err(|error| format!("{error:#}"))?;
    }
    if let Some(mut authority) = authority {
        // Protect GUI-known TUN values while retaining profile/script extras.
        let mut tun = profile
            .get("tun")
            .and_then(serde_yaml_ng::Value::as_mapping)
            .cloned()
            .unwrap_or_default();
        if let Some(app) = app_controls
            .and_then(|app| app.get("tun"))
            .and_then(serde_yaml_ng::Value::as_mapping)
        {
            tun.extend(app.clone());
        }
        authority.insert("tun".into(), tun.into());
        profile = crate::enhance::enforce_control_plane(profile, authority);
    }
    Ok(profile)
}

/// Shared source loading for either core. Native sing-box JSON bypasses Clash
/// enhancement only when no hook was configured; no lossy cross-format script
/// contract is implied. Local Clash profiles use the same pipeline as remote.
pub async fn load_profile_yaml(item: &clash_verge_core::config::PrfItem) -> Result<String, String> {
    let profiles_dir = clash_verge_core::utils::dirs::app_profiles_dir().map_err(|error| error.to_string())?;
    let file = item.file.as_deref().ok_or("selected profile has no file")?;
    let path = profiles_dir.join(file);
    let raw = tokio::fs::read_to_string(&path)
        .await
        .map_err(|error| format!("failed to read profile {}: {error}", path.display()))?;
    if crate::subscribe::from_url::is_singbox_json_profile(&raw) {
        let all = crate::profile_store::store::ProfileStore::snapshot()
            .await
            .map_err(|error| error.to_string())?
            .all_items();
        validate_native_profile_hooks(item, &profiles_dir, &all).await?;
        return Ok(raw);
    }
    if !matches!(item.itype.as_deref(), Some("remote" | "local")) {
        return Err("selected profile must be a local or remote base profile".into());
    }
    let mapping = load_remote_profile_with_rules(item).await?;
    serde_yaml_ng::to_string(&mapping).map_err(|error| error.to_string())
}

async fn validate_native_profile_hooks(
    item: &clash_verge_core::config::PrfItem,
    profiles_dir: &std::path::Path,
    all: &[clash_verge_core::config::PrfItem],
) -> Result<(), String> {
    let option = item.option.as_ref();
    for (kind, default_uid, explicit) in [
        ("merge", "Merge", option.and_then(|option| option.merge.as_deref())),
        ("script", "Script", option.and_then(|option| option.script.as_deref())),
        ("rules", "Rules", option.and_then(|option| option.rules.as_deref())),
        (
            "proxies",
            "Proxies",
            option.and_then(|option| option.proxies.as_deref()),
        ),
        ("groups", "Groups", option.and_then(|option| option.groups.as_deref())),
        ("merge", "Merge", None),
        ("script", "Script", None),
    ] {
        let uid = explicit.unwrap_or(default_uid);
        let Some(fragment) = all.iter().find(|entry| entry.uid.as_deref() == Some(uid)) else {
            if explicit.is_some() {
                return Err(format!("{kind} fragment profile not found: {uid}"));
            }
            continue;
        };
        if fragment.itype.as_deref() != Some(kind) {
            return Err(format!("{kind} fragment profile {uid} has wrong type"));
        }
        let chain = crate::chain::resolve_chain(fragment, profiles_dir)
            .await
            .map_err(|error| format!("{error:#}"))?;
        if !crate::chain::is_noop(&chain) {
            return Err(format!(
                "native sing-box JSON cannot use nonempty Clash {kind} enhancement {uid}; select a Clash YAML profile"
            ));
        }
    }
    Ok(())
}

/// Regenerate the runtime config from a refreshed remote profile and reload it.
pub async fn reload_remote_profile(
    api: &crate::mihomo_api::MihomoApi,
    item: &clash_verge_core::config::PrfItem,
    enable_tun: bool,
    core_running: bool,
) -> Result<(), String> {
    let profile = load_remote_profile_with_rules(item).await?;

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
    let (config, dns_state) = if let Some(uid) = restore_item.and_then(|item| item.uid.as_deref()) {
        let (config, state) = crate::services::profile::prepare_profile_dns_from_settings(uid, config).await?;
        (config, Some(state))
    } else {
        (config, None)
    };
    let path = clash_verge_core::utils::dirs::clash_path().map_err(|error| error.to_string())?;
    let previous = match tokio::fs::read(&path).await {
        Ok(bytes) => Some(bytes),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(format!("failed to back up {}: {error}", path.display())),
    };
    let mut transaction = RuntimeCandidate::new(path.clone(), private_candidate_path(&path), previous);

    write_runtime_config_unlocked(config, enable_tun).await?;
    transaction.installed = true;
    if !core_running {
        // Keep the newly selected runtime config for the next Start; do not API-reload
        // (or roll it back) while no controller is available.
        transaction.commit();
        persist_dns_override_state(dns_state.as_ref()).await;
        return Ok(path);
    }
    if let Err(error) = reload_config_file(api, &path).await {
        transaction
            .rollback()
            .map_err(|rollback_error| format!("{error}; rollback failed: {rollback_error}"))?;
        if transaction.previous.is_some() {
            let _ = reload_config_file(api, &path).await;
            return Err(format!("{error}; restored the previous config"));
        }
        return Err(error);
    }
    if let Some(item) = restore_item {
        restore_selected_nodes(api, item).await;
    }
    transaction.commit();
    persist_dns_override_state(dns_state.as_ref()).await;
    Ok(path)
}

pub async fn write_runtime_config_unlocked(
    mut config: serde_yaml_ng::Mapping,
    enable_tun: bool,
) -> Result<std::path::PathBuf, String> {
    // Honour the CLI's own verge.yaml port settings before the control plane
    // is snapshotted, so a port chosen to avoid the Clash Verge GUI sticks.
    crate::enhance::apply_verge_ports(&mut config).await;
    config = crate::enhance::prepare_runtime_config(config, enable_tun);
    let yaml = serde_yaml_ng::to_string(&config).map_err(|error| error.to_string())?;
    let path = clash_verge_core::utils::dirs::clash_path().map_err(|error| error.to_string())?;
    use std::io::Write as _;
    let parent = path.parent().ok_or("runtime config has no parent")?;
    std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    let mut staged = tempfile::NamedTempFile::new_in(parent).map_err(|error| error.to_string())?;
    staged.write_all(yaml.as_bytes()).map_err(|error| error.to_string())?;
    staged.as_file().sync_all().map_err(|error| error.to_string())?;
    staged
        .persist(&path)
        .map_err(|error| format!("failed to replace {}: {error}", path.display()))?;
    Ok(path)
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {

    use super::*;
    use clash_verge_core::config::{PrfItem, PrfOption};
    use serde_yaml_ng::{Mapping, Value};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static TEST_SEQ: AtomicUsize = AtomicUsize::new(0);

    #[tokio::test]
    async fn accepted_hot_reload_requires_controller_readiness_before_success() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        for ready in [true, false] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                for step in 0..2 {
                    let (mut stream, _) = listener.accept().await.unwrap();
                    let mut buffer = [0; 8192];
                    let count = stream.read(&mut buffer).await.unwrap();
                    let request = String::from_utf8_lossy(&buffer[..count]);
                    assert!(request.starts_with(if step == 0 {
                        "PUT /configs?force=true "
                    } else {
                        "GET /version "
                    }));
                    let (status, body) = if step == 0 {
                        ("204 No Content", "")
                    } else if ready {
                        ("200 OK", r#"{"version":"v1.19.32"}"#)
                    } else {
                        ("503 Unavailable", "unavailable")
                    };
                    stream
                        .write_all(
                            format!(
                                "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                                body.len()
                            )
                            .as_bytes(),
                        )
                        .await
                        .unwrap();
                }
            });
            let api = crate::mihomo_api::MihomoApi::with_transport(crate::mihomo_api::Transport::Tcp(addr), "fixture")
                .unwrap();
            let dir = tempfile::tempdir().unwrap();
            let result = reload_config_file(&api, &dir.path().join("candidate.yaml")).await;
            assert_eq!(result.is_ok(), ready);
            if !ready {
                assert!(result.unwrap_err().contains("readiness failed"));
            }
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn cancelled_apply_restores_previous_runtime_and_removes_staged_candidate() {
        let dir = tempfile::tempdir().unwrap();
        let formal = dir.path().join("runtime.json");
        let candidate = private_candidate_path(&formal);
        std::fs::write(&formal, b"old").unwrap();
        std::fs::write(&candidate, b"new").unwrap();
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let formal_owned = formal.clone();
        let candidate_owned = candidate.clone();
        let apply = tokio::spawn(async move {
            let mut transaction = RuntimeCandidate::new(formal_owned, candidate_owned, Some(b"old".to_vec()));
            transaction.install().unwrap();
            ready_tx.send(()).unwrap();
            std::future::pending::<()>().await;
        });
        ready_rx.await.unwrap();
        assert_eq!(std::fs::read(&formal).unwrap(), b"new");
        apply.abort();
        assert!(apply.await.unwrap_err().is_cancelled());
        assert_eq!(std::fs::read(&formal).unwrap(), b"old");
        assert!(!candidate.exists());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn rejected_candidate_and_completed_apply_have_distinct_cleanup() {
        let dir = tempfile::tempdir().unwrap();
        let formal = dir.path().join("runtime.json");
        let candidate = private_candidate_path(&formal);
        std::fs::write(&formal, b"old").unwrap();
        std::fs::write(&candidate, b"invalid").unwrap();
        drop(RuntimeCandidate::new(
            formal.clone(),
            candidate.clone(),
            Some(b"old".to_vec()),
        ));
        assert_eq!(std::fs::read(&formal).unwrap(), b"old");
        assert!(!candidate.exists());
        std::fs::write(&candidate, b"valid").unwrap();
        let mut transaction = RuntimeCandidate::new(formal.clone(), candidate, Some(b"old".to_vec()));
        transaction.install().unwrap();
        transaction.commit();
        drop(transaction);
        assert_eq!(std::fs::read(formal).unwrap(), b"valid");
    }

    /// Unique temp profiles dir per test. `compose_remote_profile` takes the
    /// profiles dir and the profile items explicitly, so tests need no global
    /// app-home state or cross-test locking.
    fn test_profiles_dir(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
        let seq = TEST_SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "clash-verge-cli-rules-{}-{label}-{seq}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("create test profiles dir");
        dir
    }

    fn write_file(dir: &std::path::Path, name: &str, body: &str) {
        std::fs::write(dir.join(name), body).expect("write test file");
    }

    fn rules_strings(mapping: &Mapping) -> Vec<String> {
        mapping
            .get("rules")
            .and_then(Value::as_sequence)
            .expect("rules sequence")
            .iter()
            .filter_map(|entry| entry.as_str().map(str::to_owned))
            .collect()
    }

    fn remote_item(file: &str, rules_uid: Option<&str>) -> PrfItem {
        PrfItem {
            uid: Some("Rremote01ab".into()),
            itype: Some("remote".into()),
            name: Some("demo".into()),
            file: Some(file.into()),
            option: rules_uid.map(|uid| PrfOption {
                rules: Some(uid.into()),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn rules_fragment_item(uid: &str, file: &str) -> PrfItem {
        PrfItem {
            uid: Some(uid.into()),
            itype: Some("rules".into()),
            file: Some(file.into()),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn profile_script_default_and_real_transform_compose_before_core_conversion() {
        let dir = test_profiles_dir("script");
        write_file(&dir, "sub.yaml", "rules: [MATCH,DIRECT]\nfuture: {keep: true}\n");
        let mut item = remote_item("sub.yaml", None);
        item.option = Some(PrfOption {
            script: Some("sHook".into()),
            ..Default::default()
        });
        let script = PrfItem {
            uid: Some("sHook".into()),
            itype: Some("script".into()),
            file: Some("hook.js".into()),
            ..Default::default()
        };
        write_file(&dir, "hook.js", clash_verge_core::utils::tmpl::ITEM_SCRIPT);
        let unchanged = compose_remote_profile(&item, &dir, &[script.clone()])
            .await
            .expect("GUI default script executes");
        assert_eq!(unchanged["future"]["keep"], Value::from(true));
        write_file(
            &dir,
            "hook.js",
            "function main(config, profileName) { config.future.name = profileName; config.rules.unshift('DOMAIN,script.example,DIRECT'); return config; }",
        );
        let transformed = compose_remote_profile(&item, &dir, &[script])
            .await
            .expect("real JavaScript transform executes");
        assert_eq!(transformed["future"]["name"], Value::from("demo"));
        assert_eq!(transformed["rules"][0], Value::from("DOMAIN,script.example,DIRECT"));
        assert_eq!(
            std::fs::read_to_string(dir.join("sub.yaml")).unwrap(),
            "rules: [MATCH,DIRECT]\nfuture: {keep: true}\n"
        );
    }

    #[tokio::test]
    async fn profile_scripts_follow_gui_sequence_global_profile_order_for_local_and_remote() {
        let home = tempfile::tempdir().unwrap();
        let dir = home.path();
        write_file(
            dir,
            "base.yaml",
            "rules: [base]\nproxies: [{name: old, type: direct}]\nproxy-groups: [{name: G, type: select, proxies: [old]}]\nfuture: {keep: true}\n",
        );
        let mut all = Vec::new();
        for (uid, kind, file, raw) in [
            (
                "Rules",
                "rules",
                "rules.yaml",
                "prepend: [seq]\nappend: []\ndelete: []\n",
            ),
            (
                "Proxies",
                "proxies",
                "proxies.yaml",
                "prepend: [{name: new, type: direct}]\nappend: []\ndelete: [old]\n",
            ),
            (
                "Groups",
                "groups",
                "groups.yaml",
                "prepend: []\nappend: [{name: H, type: select, proxies: [new]}]\ndelete: []\n",
            ),
            ("Merge", "merge", "merge.yaml", "future: {merged: true}\n"),
            (
                "Script",
                "script",
                "script.js",
                "function main(c, n) { if (c.rules[0] !== 'seq' || c['proxy-groups'][0].proxies[0] !== 'new' || c['proxy-groups'].length !== 2 || !c.future.merged) throw new Error('order'); c.future.count = (c.future.count || 0) + 1; c.future.name = n; return c; }",
            ),
        ] {
            write_file(dir, file, raw);
            all.push(PrfItem {
                uid: Some(uid.into()),
                itype: Some(kind.into()),
                file: Some(file.into()),
                ..Default::default()
            });
        }
        for kind in ["local", "remote"] {
            let mut item = remote_item("base.yaml", None);
            item.itype = Some(kind.into());
            let result = compose_remote_profile(&item, dir, &all).await.unwrap();
            assert_eq!(
                result["future"]["count"],
                Value::from(2),
                "default global script also runs as the profile default"
            );
            assert_eq!(result["future"]["name"], Value::from("demo"));
            assert_eq!(result["future"]["keep"], Value::from(true));
            assert_eq!(result["proxies"][0]["name"], Value::from("new"));
        }
    }

    #[tokio::test]
    async fn scripts_observe_authoritative_app_controls_and_cannot_override_them() {
        let home = tempfile::tempdir().unwrap();
        write_file(
            home.path(),
            "base.yaml",
            "mode: global\nmixed-port: 1\ntun: {enable: true, future: inherited}\nfuture: {keep: true}\n",
        );
        write_file(
            home.path(),
            "script.js",
            "function main(c) { c.observed = [c.mode, c['mixed-port'], c.tun.enable]; c.mode = 'direct'; c['mixed-port'] = 2; c.secret = 'bad'; c.tun.enable = true; c.tun.extra = 'script'; return c; }",
        );
        let mut item = remote_item("base.yaml", None);
        item.uid = None; // pure fixture: no on-disk DNS settings lookup.
        item.option = Some(PrfOption {
            script: Some("sHook".into()),
            ..Default::default()
        });
        let all = [PrfItem {
            uid: Some("sHook".into()),
            itype: Some("script".into()),
            file: Some("script.js".into()),
            ..Default::default()
        }];
        let controls: Mapping = serde_yaml_ng::from_str(
            "mode: rule\nmixed-port: 35123\nsecret: fixture\ntun: {enable: false, mtu: 1500}\n",
        )
        .unwrap();
        let result = compose_profile_with_controls(&item, home.path(), &all, Some(&controls))
            .await
            .unwrap();
        assert_eq!(result["observed"][0], Value::from("rule"));
        assert_eq!(result["observed"][1], Value::from(35123));
        assert_eq!(result["observed"][2], Value::from(false));
        assert_eq!(result["mode"], Value::from("rule"));
        assert_eq!(result["mixed-port"], Value::from(35123));
        assert_eq!(result["secret"], Value::from("fixture"));
        assert_eq!(result["tun"]["enable"], Value::from(false));
        assert_eq!(result["tun"]["future"], Value::from("inherited"));
        assert_eq!(result["tun"]["extra"], Value::from("script"));
    }

    #[tokio::test]
    async fn script_references_require_the_correct_uid_type_and_file() {
        let home = tempfile::tempdir().unwrap();
        write_file(home.path(), "base.yaml", "future: {keep: true}\n");
        let mut item = remote_item("base.yaml", None);
        item.option = Some(PrfOption {
            script: Some("sHook".into()),
            ..Default::default()
        });
        assert!(
            compose_remote_profile(&item, home.path(), &[])
                .await
                .unwrap_err()
                .contains("sHook")
        );
        let mut hook = PrfItem {
            uid: Some("sHook".into()),
            itype: Some("merge".into()),
            file: Some("missing.js".into()),
            ..Default::default()
        };
        assert!(
            compose_remote_profile(&item, home.path(), &[hook.clone()])
                .await
                .unwrap_err()
                .contains("wrong type")
        );
        hook.itype = Some("script".into());
        assert!(
            compose_remote_profile(&item, home.path(), &[hook])
                .await
                .unwrap_err()
                .contains("missing.js")
        );
    }

    #[tokio::test]
    async fn native_json_import_default_fragments_pass_through_but_real_hooks_reject() {
        let home = tempfile::tempdir().unwrap();
        let mut all = vec![
            PrfItem::from_merge(None).unwrap(),
            PrfItem::from_script(None).unwrap(),
            PrfItem::from_rules().unwrap(),
            PrfItem::from_proxies().unwrap(),
            PrfItem::from_groups().unwrap(),
        ];
        for fragment in &mut all {
            write_file(
                home.path(),
                fragment.file.as_deref().unwrap(),
                fragment.file_data.as_deref().unwrap(),
            );
        }
        let mut item = remote_item("native.json", None);
        item.option = Some(PrfOption {
            merge: all[0].uid.clone(),
            script: all[1].uid.clone(),
            rules: all[2].uid.clone(),
            proxies: all[3].uid.clone(),
            groups: all[4].uid.clone(),
            ..Default::default()
        });
        validate_native_profile_hooks(&item, home.path(), &all)
            .await
            .expect("normal importer no-op references are accepted");
        write_file(
            home.path(),
            all[1].file.as_deref().unwrap(),
            "function main(c) { c.rules = ['MATCH,DIRECT']; return c; }",
        );
        assert!(
            validate_native_profile_hooks(&item, home.path(), &all)
                .await
                .unwrap_err()
                .contains("nonempty Clash script")
        );
    }

    #[tokio::test]
    async fn remote_refresh_composes_local_rules_with_fresh_upstream_rules() {
        let dir = test_profiles_dir("compose");
        write_file(&dir, "sub.yaml", "rules:\n  - A\n  - B\n  - C\n");
        write_file(&dir, "rules.yaml", "prepend:\n  - P\nappend:\n  - Q\ndelete:\n  - B\n");
        let item = remote_item("sub.yaml", Some("rFrag01"));
        let all = vec![rules_fragment_item("rFrag01", "rules.yaml")];

        let mapping = compose_remote_profile(&item, &dir, &all)
            .await
            .expect("fragment composes with fresh upstream rules");

        assert_eq!(rules_strings(&mapping), vec!["P", "A", "C", "Q"]);
    }

    #[tokio::test]
    async fn legacy_sequence_fragment_replaces_upstream_rules() {
        let dir = test_profiles_dir("legacy");
        write_file(&dir, "sub.yaml", "rules:\n  - A\n  - B\n");
        write_file(&dir, "rules.yaml", "- X\n- Y\n");
        let item = remote_item("sub.yaml", Some("rFrag01"));
        let all = vec![rules_fragment_item("rFrag01", "rules.yaml")];

        let mapping = compose_remote_profile(&item, &dir, &all)
            .await
            .expect("legacy fragment replaces upstream rules");

        assert_eq!(rules_strings(&mapping), vec!["X", "Y"]);
    }

    #[tokio::test]
    async fn remote_refresh_with_no_rules_option_keeps_upstream_unchanged() {
        let dir = test_profiles_dir("no-option");
        write_file(&dir, "sub.yaml", "rules:\n  - A\n  - B\n");
        let item = remote_item("sub.yaml", None);

        let mapping = compose_remote_profile(&item, &dir, &[])
            .await
            .expect("no fragment keeps upstream rules");

        assert_eq!(rules_strings(&mapping), vec!["A", "B"]);
    }

    #[tokio::test]
    async fn remote_refresh_rejects_malformed_fragment() {
        let dir = test_profiles_dir("bad-fragment");
        write_file(&dir, "sub.yaml", "rules:\n  - A\n  - B\n");
        write_file(&dir, "rules.yaml", "prepend: []\nappends: []\n");
        let item = remote_item("sub.yaml", Some("rFrag01"));
        let all = vec![rules_fragment_item("rFrag01", "rules.yaml")];

        let error = compose_remote_profile(&item, &dir, &all)
            .await
            .expect_err("malformed fragment must reject the reload");

        assert!(
            error.contains("appends") && error.contains("rules.yaml"),
            "error cites the bad key and the fragment path: {error}"
        );
    }

    #[tokio::test]
    async fn remote_refresh_rejects_unknown_rules_uid() {
        let dir = test_profiles_dir("unknown-uid");
        write_file(&dir, "sub.yaml", "rules:\n  - A\n");
        let item = remote_item("sub.yaml", Some("rMissing1"));

        let error = compose_remote_profile(&item, &dir, &[])
            .await
            .expect_err("unresolvable rules uid must reject the reload");

        assert!(error.contains("rMissing1"), "error names the uid: {error}");
    }

    #[tokio::test]
    async fn remote_refresh_rejects_missing_fragment_file() {
        let dir = test_profiles_dir("missing-file");
        write_file(&dir, "sub.yaml", "rules:\n  - A\n");
        let item = remote_item("sub.yaml", Some("rFrag01"));
        let all = vec![rules_fragment_item("rFrag01", "does-not-exist.yaml")];

        let error = compose_remote_profile(&item, &dir, &all)
            .await
            .expect_err("missing fragment file must reject the reload");

        assert!(
            error.contains("does-not-exist.yaml"),
            "error names the missing fragment: {error}"
        );
    }

    #[tokio::test]
    async fn remote_refresh_rejects_invalid_upstream_profile_yaml() {
        let dir = test_profiles_dir("bad-profile");
        write_file(&dir, "sub.yaml", "rules: [A, B\n");
        let item = remote_item("sub.yaml", None);

        let error = compose_remote_profile(&item, &dir, &[])
            .await
            .expect_err("invalid upstream YAML must reject the reload");

        assert!(error.contains("invalid YAML"), "error names the parse failure: {error}");
    }
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

    #[test]
    fn candidate_paths_are_private_and_unique() {
        let config = std::path::Path::new("/tmp/singbox.json");
        let first = private_candidate_path(config);
        let second = private_candidate_path(config);
        assert_ne!(first, second);
        assert!(
            first
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with(".singbox.candidate-")
        );
        assert_eq!(first.parent(), config.parent());
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
