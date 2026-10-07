//! Runtime config write/reload primitives shared by the TUI event loop and the
//! headless daemon. Extracted from `tui/event_loop.rs` so both modes reuse one
//! implementation of backup → build → write → reload/rollback.

use std::sync::LazyLock;

use tokio::sync::Mutex;

use crate::chain::{apply_rules_fragment, parse_rules_fragment};

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
    compose_remote_profile(item, &profiles_dir, &all_items).await
}

/// Compose the runtime mapping for a remote profile: the upstream profile
/// with its configured rules fragment applied on top.
///
/// `all_items` resolves `option.rules` — a profile UID — to the fragment
/// item carrying the on-disk `file` name. With no configured rules fragment
/// the upstream profile is returned unchanged.
async fn compose_remote_profile(
    item: &clash_verge_core::config::PrfItem,
    profiles_dir: &std::path::Path,
    all_items: &[clash_verge_core::config::PrfItem],
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

    if item
        .option
        .as_ref()
        .and_then(|option| option.script.as_deref())
        .is_some()
    {
        return Err(
            "profile script execution is unsupported; remove the script override or use a preprocessed profile".into(),
        );
    }
    if let Some(merge_uid) = item.option.as_ref().and_then(|option| option.merge.as_deref()) {
        let merge_item = all_items
            .iter()
            .find(|entry| entry.uid.as_deref() == Some(merge_uid))
            .ok_or_else(|| format!("merge fragment profile not found: {merge_uid}"))?;
        let chain = crate::chain::resolve_chain(merge_item, profiles_dir)
            .await
            .map_err(|error| error.to_string())?;
        crate::chain::apply_chain_to_config(&mut profile, &chain).map_err(|error| error.to_string())?;
    }
    let Some(rules_uid) = item.option.as_ref().and_then(|option| option.rules.as_deref()) else {
        return Ok(profile);
    };

    let fragment_item = all_items
        .iter()
        .find(|candidate| candidate.uid.as_deref() == Some(rules_uid))
        .ok_or_else(|| format!("rules fragment profile not found for option.rules={rules_uid}"))?;
    let fragment_file = fragment_item
        .file
        .as_deref()
        .ok_or_else(|| format!("rules fragment profile {rules_uid} is missing file"))?;
    let fragment_path = profiles_dir.join(fragment_file);
    if !fragment_path.exists() {
        return Err(format!("rules fragment file not found: {}", fragment_path.display()));
    }
    let fragment_raw = tokio::fs::read_to_string(&fragment_path)
        .await
        .map_err(|error| format!("failed to read {}: {error}", fragment_path.display()))?;
    let fragment = parse_rules_fragment(&fragment_raw, &fragment_path).map_err(|error| error.to_string())?;
    apply_rules_fragment(&mut profile, &fragment);
    Ok(profile)
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
