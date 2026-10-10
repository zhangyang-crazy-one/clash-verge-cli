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
    // Captured at entry and used for the whole transaction: a failed restart
    // clears the manager's pid, so conditioning the rollback restart on the
    // post-failure state skipped the retry and left the previous config
    // restored on disk with no core running at all.
    let was_running = manager.state() == crate::app::CoreState::Running;
    // #55: sing-box has no hot reload, so applying a profile means
    // restarting the core. Requiring this process to be the parent made
    // `profile use` / `profile update --reload` impossible under sing-box:
    // `start` runs the core under a detached supervisor, so every later CLI
    // invocation (and a TUI attached to a service-started core) was
    // refused as "externally managed". A core with a verified pid record
    // is ours to replace — through its supervisor. Only a core with no
    // record at all is somebody else's.
    //
    // P1 (reviewer): the authorization is captured HERE, once, and the
    // rollback retry re-uses it. The first restart stops the adopted core
    // (clearing the manager's pid) and then fails to launch; re-running the
    // policy on the retry would refuse it and leave the previous config on
    // disk with nothing serving.
    let authorization = if was_running {
        match manager.capture_restart_authorization().await {
            Ok(authorization) => authorization,
            Err(error) => return Err(format!("cannot apply sing-box settings: {error}")),
        }
    } else {
        // Stopped: the apply only persists; no restart is ever attempted.
        None
    };
    use crate::commands::start::SupervisorLaunch;
    let restart = |mode: SupervisorLaunch| {
        let authorization = authorization.clone();
        async move {
            // An owned child restarts in place; an adopted one is replaced by
            // a detached supervisor so the replacement outlives this process.
            match &authorization {
                Some(authorization) => manager.apply_restart_authorization_with(authorization, mode).await,
                None => manager.restart_through_supervisor_with(mode).await,
            }
        }
    };
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
    if !was_running {
        transaction.commit();
        persist_dns_override_state(dns_state.as_ref()).await;
        record_singbox_degradation(&parts).await;
        return Ok(if parts.profile_used {
            format!(
                "sing-box: {} nodes, {} skipped, {} fields degraded, {} notes (saved; core stopped)",
                parts.conversion.outbounds.len(),
                parts.conversion.skipped.len(),
                parts.conversion.degraded.len(),
                parts.conversion.notes.len()
            )
        } else {
            "sing-box: skeleton config saved (core stopped)".into()
        });
    }

    if let Err(restart_error) = restart(SupervisorLaunch::Regenerate).await {
        transaction
            .rollback()
            .map_err(|rollback_error| format!("{restart_error}; rollback failed: {rollback_error}"))?;
        // Retry with the previous configuration whenever a core was running
        // when we entered: the failed restart already cleared the pid, so the
        // manager state can no longer answer this question — and the
        // authorization captured at entry is what makes the retry possible at
        // all.
        //
        // P1 (reviewer): the retry runs in RECOVERY mode. The previous
        // config (A) is back on disk, but the rules editor has already
        // persisted the newer profile (B); a supervisor launched the normal
        // way regenerates the runtime config from the active profile, so the
        // "recovered" service ran B and overwrote A again. The recovery must
        // start the core on the restored file, verbatim.
        if should_retry_rollback_restart(transaction.previous.is_some(), was_running)
            && let Err(rollback_error) = restart(SupervisorLaunch::UseExistingConfig).await
        {
            return Err(format!(
                "{restart_error}; previous configuration restored but fallback restart failed: {rollback_error}"
            ));
        }
        return Err(restart_error.to_string());
    }

    transaction.commit();
    persist_dns_override_state(dns_state.as_ref()).await;
    // What this apply lost, in a shape the TUI and `status --json` can show
    // grouped (the report string below only carries counts).
    record_singbox_degradation(&parts).await;

    // Human-readable degradation report for the status bar.
    let report = if parts.profile_used {
        format!(
            "sing-box: {} nodes, {} skipped, {} fields degraded, {} notes",
            parts.conversion.outbounds.len(),
            parts.conversion.skipped.len(),
            parts.conversion.degraded.len(),
            parts.conversion.notes.len()
        )
    } else {
        "sing-box: skeleton config applied".into()
    };
    // Detail lines: what was approximated or dropped while converting
    // (#52), so a degraded run is never silent.
    for note in parts.conversion.notes.iter().chain(parts.conversion.skipped.iter()) {
        tracing::info!(target: "config", "sing-box conversion: {note}");
    }
    Ok(report)
}

/// One bucket of what a Clash → sing-box conversion lost. The TUI notice
/// and `status --json` speak in buckets because the raw notes are one line
/// per lost item and nobody reads forty of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DegradationCategory {
    /// Nodes/groups the conversion could not express at all.
    Nodes,
    /// Route rules dropped or approximated.
    Rules,
    /// Profile DNS fields the typed model cannot carry.
    Dns,
    /// Everything else (approximated group semantics, pruned members).
    Groups,
}

/// Grouped digest of one conversion run's losses.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct SingboxDegradationDigest {
    /// Total note lines behind the buckets (the raw conversion report size).
    pub notes: usize,
    /// Node/group count the conversion dropped entirely.
    pub nodes_skipped: usize,
    /// Per-node field-level degradations (`node.field` lines).
    pub fields_degraded: usize,
    /// Counts per category, ascending by category so the notice is stable.
    pub categories: std::collections::BTreeMap<DegradationCategory, usize>,
}

/// A digest plus the apply generation it belongs to, so a consumer shows it
/// once instead of on every later event.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct SingboxDegradationRecord {
    /// Monotonic id: a newer apply supersedes an older record even when the
    /// new apply produced fewer notes.
    pub seq: u64,
    pub digest: SingboxDegradationDigest,
}

impl SingboxDegradationDigest {
    /// Number of non-empty categories — the "N 类降级" headline count.
    pub fn category_count(&self) -> usize {
        self.categories.values().filter(|count| **count > 0).count()
    }
}

/// Which note line belongs to which bucket. `dns.` prefixed notes come from
/// the DNS conversion report, rule mentions from the route rule pass, and
/// the remainder is group/member pruning.
pub fn classify_degradation_note(note: &str) -> DegradationCategory {
    let lower = note.to_ascii_lowercase();
    if lower.starts_with("dns.") {
        DegradationCategory::Dns
    } else if lower.contains("rule") {
        DegradationCategory::Rules
    } else {
        DegradationCategory::Groups
    }
}

/// Build the digest from a finished conversion. Pure: unit-tested without a
/// core, a config dir or a network.
pub fn singbox_degradation_digest(parts: &crate::mihomo_manager::manager::SingboxParts) -> SingboxDegradationDigest {
    let mut categories = std::collections::BTreeMap::new();
    if !parts.conversion.skipped.is_empty() {
        categories.insert(DegradationCategory::Nodes, parts.conversion.skipped.len());
    }
    for note in &parts.conversion.notes {
        *categories.entry(classify_degradation_note(note)).or_insert(0) += 1;
    }
    categories.retain(|_, count| *count > 0);
    SingboxDegradationDigest {
        notes: parts.conversion.notes.len(),
        nodes_skipped: parts.conversion.skipped.len(),
        fields_degraded: parts.conversion.degraded.len(),
        categories,
    }
}

/// The notice is only true for a *converted* profile running on sing-box:
/// a native sing-box subscription passes through untouched (no losses to
/// report) and a mihomo core never converts anything.
pub fn should_surface_singbox_degradation(
    profile_used: bool,
    core: crate::mihomo_manager::CoreKind,
    digest: &SingboxDegradationDigest,
) -> bool {
    let total: usize = digest.notes + digest.nodes_skipped + digest.fields_degraded;
    profile_used && core == crate::mihomo_manager::CoreKind::SingBox && total > 0 && digest.category_count() > 0
}

static LAST_SINGBOX_DEGRADATION: LazyLock<Mutex<Option<SingboxDegradationRecord>>> = LazyLock::new(|| Mutex::new(None));
static SINGBOX_DEGRADATION_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Remember what the last successful sing-box apply lost. Records the
/// *absence* of losses too (as `None`) so a clean apply clears a stale
/// warning instead of leaving it on screen forever.
pub async fn record_singbox_degradation(parts: &crate::mihomo_manager::manager::SingboxParts) {
    let digest = singbox_degradation_digest(parts);
    let record =
        should_surface_singbox_degradation(parts.profile_used, crate::mihomo_manager::CoreKind::SingBox, &digest).then(
            || {
                let seq = SINGBOX_DEGRADATION_SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                SingboxDegradationRecord { seq, digest }
            },
        );
    *LAST_SINGBOX_DEGRADATION.lock().await = record;
}

/// The last apply's degradation record, for `status --json` and the TUI.
pub async fn last_singbox_degradation() -> Option<SingboxDegradationRecord> {
    LAST_SINGBOX_DEGRADATION.lock().await.clone()
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

/// Whether a failed restart should be retried against the restored previous
/// config.
///
/// Driven by the state captured at entry (`was_running`), never by the state
/// left behind by the failure: a failed restart clears the manager's pid, so
/// `owns_child || pid.is_some()` is false exactly when a core was running and
/// needs to come back — which left the previous config on disk with nothing
/// serving it. There is nothing to bring up when no core was running, and
/// nothing to restore when there was no previous config.
fn should_retry_rollback_restart(has_previous: bool, was_running: bool) -> bool {
    has_previous && was_running
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
    compose_profile_with_controls(item, &profiles_dir, &all_items, Some(&app_config), None).await
}

/// The rule list the core actually evaluates: the profile's `rules:` AFTER
/// the Rules/Merge/Script chain has been applied.
///
/// This is THE canonical rule identity. A rule order's `Profile(i)` entries
/// are indices into this list (the generator interleaves the composed rules),
/// so the fingerprint recorded next to them must be this list's fingerprint
/// too. Fingerprinting the raw profile file instead made every save look like
/// a subscription drift as soon as the profile had a prepend fragment, and
/// the saved interleaving was demoted to append-after.
pub async fn composed_profile_rules(
    item: &clash_verge_core::config::PrfItem,
) -> Result<Vec<crate::routing::IRouteRule>, String> {
    let yaml = load_profile_yaml(item).await?;
    crate::routing::load_profile_rules(&yaml)
}

/// Compose `item` with `file_rules` substituted for the profile file's
/// `rules:` list and return the composed list — what a write of
/// `file_rules` to the profile would produce once the chain runs.
///
/// The rules editor uses it to prove a save before it touches the profile:
/// the chain re-applies its own contribution on top of whatever the file
/// holds, so writing the composed list back verbatim would duplicate every
/// fragment rule.
pub async fn compose_profile_rules(
    item: &clash_verge_core::config::PrfItem,
    file_rules: &[crate::routing::IRouteRule],
) -> Result<Vec<crate::routing::IRouteRule>, String> {
    let profiles_dir = clash_verge_core::utils::dirs::app_profiles_dir().map_err(|error| error.to_string())?;
    let all_items = crate::profile_store::store::ProfileStore::snapshot()
        .await
        .map_err(|error| error.to_string())?
        .all_items();
    let mut app_config = clash_verge_core::config::IClashTemp::new().await.0;
    crate::enhance::apply_verge_ports(&mut app_config).await;
    let verge = clash_verge_core::config::IVerge::new().await;
    app_config = crate::enhance::use_tun(app_config, verge.enable_tun_mode.unwrap_or(false));
    let substituted: Vec<serde_yaml_ng::Value> = file_rules
        .iter()
        .map(|rule| {
            let raw = match rule {
                crate::routing::IRouteRule::Raw { clash_raw, .. } => clash_raw.clone(),
                other => crate::routing::to_clash_rule_str(other),
            };
            serde_yaml_ng::Value::String(raw)
        })
        .collect();
    let mapping =
        compose_profile_with_controls(item, &profiles_dir, &all_items, Some(&app_config), Some(substituted)).await?;
    let yaml = serde_yaml_ng::to_string(&mapping).map_err(|error| error.to_string())?;
    crate::routing::load_profile_rules(&yaml)
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
    compose_profile_with_controls(item, profiles_dir, all_items, None, None).await
}

/// `rules_override` replaces the profile's `rules:` list BEFORE the chain
/// runs, i.e. exactly as if the profile file had been rewritten with those
/// rules and re-read.
async fn compose_profile_with_controls(
    item: &clash_verge_core::config::PrfItem,
    profiles_dir: &std::path::Path,
    all_items: &[clash_verge_core::config::PrfItem],
    app_controls: Option<&serde_yaml_ng::Mapping>,
    rules_override: Option<Vec<serde_yaml_ng::Value>>,
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
    if let Some(rules) = rules_override {
        profile.insert("rules".into(), serde_yaml_ng::Value::Sequence(rules));
    }

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

    fn parts_with(
        profile_used: bool,
        skipped: &[&str],
        degraded: &[&str],
        notes: &[&str],
    ) -> crate::mihomo_manager::manager::SingboxParts {
        let owned = |items: &[&str]| items.iter().map(|item| (*item).to_string()).collect::<Vec<_>>();
        crate::mihomo_manager::manager::SingboxParts {
            conversion: crate::singbox::convert::ProfileConversion {
                outbounds: vec![],
                groups: vec![],
                skipped: owned(skipped),
                degraded: owned(degraded),
                notes: owned(notes),
            },
            profile_used,
            route_rules: vec![],
            rule_sets: vec![],
            dns_section: None,
            default_domain_resolver: None,
        }
    }

    #[test]
    fn degradation_notes_group_into_nodes_rules_dns_and_groups() {
        // The user-visible shape of a real converted Clash profile:
        // unsupported nodes, dropped rules and DNS fields the typed model
        // cannot carry, plus group pruning.
        let parts = parts_with(
            true,
            &[
                "edge: unsupported protocol mieru",
                "pool: group type 'relay' unsupported",
            ],
            &["tls-node.skip-cert-verify: dropped"],
            &[
                "profile rules[3]: GEOSITE matcher unsupported",
                "dns.fallback-filter: ignored; no typed equivalent",
                "dns.fake-ip-filter: unsupported DNS field; dropped",
                "select: dropped redundant DIRECT member (selectors always expose direct)",
            ],
        );
        let digest = singbox_degradation_digest(&parts);
        assert_eq!(digest.nodes_skipped, 2);
        assert_eq!(digest.fields_degraded, 1);
        assert_eq!(digest.notes, 4);
        assert_eq!(digest.category_count(), 4);
        assert_eq!(digest.categories.get(&DegradationCategory::Nodes), Some(&2));
        assert_eq!(digest.categories.get(&DegradationCategory::Rules), Some(&1));
        assert_eq!(digest.categories.get(&DegradationCategory::Dns), Some(&2));
        assert_eq!(digest.categories.get(&DegradationCategory::Groups), Some(&1));
    }

    #[test]
    fn the_notice_is_gated_on_a_converted_profile_running_sing_box() {
        use crate::mihomo_manager::CoreKind;

        let converted = singbox_degradation_digest(&parts_with(
            true,
            &["edge: unsupported protocol mieru"],
            &[],
            &["dns.listen: unsupported DNS field; dropped"],
        ));
        assert!(
            should_surface_singbox_degradation(true, CoreKind::SingBox, &converted),
            "a converted profile with losses is exactly what must be surfaced"
        );
        assert!(
            !should_surface_singbox_degradation(false, CoreKind::SingBox, &converted),
            "a skeleton apply lost nothing; do not warn"
        );
        assert!(
            !should_surface_singbox_degradation(true, CoreKind::Mihomo, &converted),
            "mihomo never converts a profile; the hint would be a lie"
        );
        // A native sing-box subscription passes through `write_singbox_assembled_to`
        // untouched: no skipped entries and no notes.
        let native = singbox_degradation_digest(&parts_with(true, &[], &[], &[]));
        assert!(
            !should_surface_singbox_degradation(true, CoreKind::SingBox, &native),
            "a native passthrough profile has no degradations to report"
        );
    }

    #[tokio::test]
    async fn a_recorded_apply_replaces_the_previous_record_and_clears_on_a_clean_one() {
        let lossy = parts_with(
            true,
            &["edge: unsupported protocol mieru"],
            &[],
            &["dns.listen: ignored"],
        );
        record_singbox_degradation(&lossy).await;
        let first = last_singbox_degradation().await.expect("a lossy apply is recorded");
        assert_eq!(first.digest.nodes_skipped, 1);

        // A later, cleaner apply supersedes it (newer seq, fewer buckets) so
        // the TUI shows the current truth instead of an old warning.
        let milder = parts_with(true, &[], &[], &["dns.fallback-filter: ignored"]);
        record_singbox_degradation(&milder).await;
        let second = last_singbox_degradation().await.expect("still recorded");
        assert!(second.seq > first.seq);
        assert_eq!(second.digest.nodes_skipped, 0);

        record_singbox_degradation(&parts_with(true, &[], &[], &[])).await;
        assert!(
            last_singbox_degradation().await.is_none(),
            "a lossless apply must clear a stale degradation notice"
        );
    }

    #[tokio::test]
    async fn a_recorded_sing_box_core_is_reconfigurable_but_a_foreign_one_is_not() {
        // #55: the manager here never spawned the core (the supervisor
        // does), so `owns_child` is false in every later CLI invocation.
        // The pid record is what makes the core ours to replace.
        use crate::mihomo_manager::CoreKind;
        use crate::mihomo_manager::manager::supervisor_restart_policy;

        supervisor_restart_policy(false, Some(1234), CoreKind::SingBox, CoreKind::SingBox)
            .expect("an adopted sing-box core may be restarted");
        let foreign = supervisor_restart_policy(false, None, CoreKind::SingBox, CoreKind::SingBox)
            .expect_err("a core with no record stays off limits");
        assert!(
            foreign.to_string().contains("no clash-verge-cli pid record"),
            "{foreign}"
        );
    }

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
        let unchanged = compose_remote_profile(&item, &dir, std::slice::from_ref(&script))
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
        let result = compose_profile_with_controls(&item, home.path(), &all, Some(&controls), None)
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
    /// The rollback restart is driven by the state captured at entry, not by
    /// the manager state a failed restart leaves behind (which has no pid at
    /// all — the case that used to skip the retry and strand the previous
    /// config on disk with nothing running).
    #[test]
    fn the_rollback_restart_follows_the_entry_state() {
        assert!(
            should_retry_rollback_restart(true, true),
            "a failed restart of a running core must retry the previous config"
        );
        assert!(
            !should_retry_rollback_restart(true, false),
            "nothing to bring back when no core was running"
        );
        assert!(
            !should_retry_rollback_restart(false, true),
            "nothing to restore when there was no previous config"
        );
    }

    /// P1 (reviewer): the adopted-core rollback must restore the SERVICE,
    /// not just the file. The first restart stops the adopted core (which
    /// clears the manager's pid) and then fails to launch the supervisor;
    /// the retry cannot pass `supervisor_restart_policy` any more, so the
    /// old config used to be left on disk with nothing serving it.
    ///
    /// This drives the real transaction (assemble → prevalidate → install →
    /// restart → rollback → retry) against a real adopted pid, with only the
    /// supervisor *process launch* injected, and a controller that answers
    /// 200 only while the previous config is the one on disk.
    #[tokio::test]
    async fn an_adopted_core_rollback_restores_the_previous_config_and_its_service() {
        use crate::mihomo_api::error::MihomoError;
        use crate::mihomo_manager::MihomoManager;
        use crate::mihomo_manager::pidfile::{self, CoreKind, CoreRecord};
        use std::path::Path;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::{Mutex, OnceLock};

        const PREVIOUS: &str =
            r#"{"marker":"old","experimental":{"clash_api":{"external_controller":"127.0.0.1:19997"}}}}"#;

        let home = tempfile::tempdir().expect("tempdir");
        let _home_guard = crate::profile_store::store::tests::claim_test_app_home(home.path().to_path_buf()).await;

        struct Seam {
            attempts: AtomicUsize,
            live: &'static Mutex<Vec<u32>>,
            formal: PathBuf,
            supervisor: &'static Mutex<Vec<u32>>,
            /// Launch mode each attempt asked for.
            modes: &'static Mutex<Vec<crate::commands::start::SupervisorLaunch>>,
        }
        static SEAM: OnceLock<&'static Seam> = OnceLock::new();
        static AUTHED: AtomicUsize = AtomicUsize::new(0);

        // A controller that only answers while the PREVIOUS config is
        // installed: the recovery restart must come up on the restored one.
        let blocking = std::net::TcpListener::bind("127.0.0.1:19997").expect("bind fake controller");
        blocking.set_nonblocking(true).expect("nonblocking");
        let listener = tokio::net::TcpListener::from_std(blocking).expect("tokio listener");
        let formal = clash_verge_core::utils::dirs::singbox_config_path().expect("singbox config path");
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let formal = formal.clone();
                tokio::spawn(async move {
                    let mut buffer = [0u8; 4096];
                    let Ok(count) = stream.read(&mut buffer).await else {
                        return;
                    };
                    let request = String::from_utf8_lossy(&buffer[..count]).to_string();
                    let served_previous = std::fs::read_to_string(&formal)
                        .map(|body| body.contains("\"marker\":\"old\""))
                        .unwrap_or(false);
                    let (status, body) = if served_previous {
                        AUTHED.fetch_add(1, Ordering::SeqCst);
                        ("200 OK", r#"{"version":"1.19.0"}"#)
                    } else {
                        ("503 Unavailable", r#"{"message":"candidate not serving"}"#)
                    };
                    let _ = stream
                        .write_all(
                            format!(
                                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                                body.len()
                            )
                            .as_bytes(),
                        )
                        .await;
                    let _ = request;
                });
            }
        });

        #[allow(clippy::zombie_processes)]
        fn spawn_dummy(live: &Mutex<Vec<u32>>) -> u32 {
            let child = std::process::Command::new("/bin/sleep")
                .arg("300")
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .expect("spawn stand-in core");
            live.lock().expect("live").push(child.id());
            child.id()
        }

        fn launch(
            config_dir: &Path,
            log: &Path,
            mode: crate::commands::start::SupervisorLaunch,
        ) -> anyhow::Result<std::process::Child> {
            let seam = *SEAM.get().expect("seam");
            if let Some(dir) = log.parent() {
                std::fs::create_dir_all(dir)?;
            }
            std::fs::write(log, b"stand-in supervisor\n")?;
            let attempt = seam.attempts.fetch_add(1, Ordering::SeqCst);
            seam.modes.lock().expect("modes").push(mode);
            if attempt == 0 {
                // Injected failure: the core is stopped and nothing starts.
                anyhow::bail!("injected supervisor launch failure");
            }
            let _ = config_dir;
            // The replacement supervisor brings up a fresh core and records it.
            let pid = spawn_dummy(seam.live);
            pidfile::write(
                &pidfile::path_for(&seam.formal.parent().expect("home").join("controller.sock")),
                CoreRecord::with_kind_and_exe(
                    pid,
                    chrono::Utc::now(),
                    CoreKind::SingBox,
                    Some("/bin/sleep".to_string()),
                ),
            )
            .expect("record");
            #[allow(clippy::zombie_processes)]
            let child = std::process::Command::new("/bin/sleep")
                .arg("300")
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()?;
            seam.supervisor.lock().expect("supervisor").push(child.id());
            Ok(child)
        }

        std::fs::write(home.path().join("verge.yaml"), "proxy_core: singbox\n").expect("verge");
        std::fs::write(
            home.path().join("config.yaml"),
            "mixed-port: 39183\nexternal-controller: 127.0.0.1:19997\nsecret: stable-test-secret\n",
        )
        .expect("config");
        let formal = clash_verge_core::utils::dirs::singbox_config_path().expect("formal");
        std::fs::write(&formal, PREVIOUS).expect("previous config");

        let live: &'static Mutex<Vec<u32>> = Box::leak(Box::new(Mutex::new(Vec::new())));
        let supervisor: &'static Mutex<Vec<u32>> = Box::leak(Box::new(Mutex::new(Vec::new())));
        let modes: &'static Mutex<Vec<crate::commands::start::SupervisorLaunch>> =
            Box::leak(Box::new(Mutex::new(Vec::new())));
        let seam: &'static Seam = Box::leak(Box::new(Seam {
            attempts: AtomicUsize::new(0),
            live,
            supervisor,
            formal: formal.clone(),
            modes,
        }));
        SEAM.set(seam).ok().expect("seam once");

        let config_dir = home.path().join("run");
        std::fs::create_dir_all(&config_dir).expect("config dir");
        let socket = home.path().join("controller.sock");
        let manager = MihomoManager::new(config_dir)
            .with_socket(socket.clone())
            .with_singbox_controller("127.0.0.1:19997".parse().expect("addr"))
            .with_core_kind(CoreKind::SingBox)
            .with_secret("stable-test-secret".to_string());
        *manager.inner().resolved_binary.lock() = Some(PathBuf::from("/bin/true"));

        // An adopted core: this process never spawned it (owns_child false).
        let adopted = spawn_dummy(live);
        pidfile::write(
            &pidfile::path_for(&socket),
            CoreRecord::with_kind_and_exe(
                adopted,
                chrono::Utc::now(),
                CoreKind::SingBox,
                Some("/bin/sleep".to_string()),
            ),
        )
        .expect("record");
        manager.adopt_running_core();
        // `adopt_running_core` validates the recorded process shape, which a
        // stand-in `sleep` cannot satisfy; the transaction's own view of the
        // adopted core is set the way a real adoption leaves it.
        *manager.inner().pid.lock() = Some(adopted);
        *manager.inner().state.lock() = crate::app::CoreState::Running;
        assert!(!manager.owns_child(), "the case under test is an adopted core");

        let _launcher = crate::commands::start::install_supervisor_launcher(launch).expect("install seam");
        let error = apply_singbox_restart_for_profile(&manager, None, false, None)
            .await
            .expect_err("the first supervisor launch was injected to fail");
        assert!(error.contains("injected supervisor launch failure"), "{error}");
        for pid in supervisor.lock().expect("supervisor").drain(..) {
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(pid as i32),
                nix::sys::signal::Signal::SIGKILL,
            );
        }

        assert_eq!(seam.attempts.load(Ordering::SeqCst), 2, "the rollback must retry once");
        // P1 (reviewer): the retry is a RECOVERY — it must be launched to
        // serve the restored config, not to regenerate it from the (newer)
        // active profile.
        assert_eq!(
            *seam.modes.lock().expect("modes"),
            vec![
                crate::commands::start::SupervisorLaunch::Regenerate,
                crate::commands::start::SupervisorLaunch::UseExistingConfig,
            ]
        );
        assert_eq!(
            std::fs::read_to_string(&formal).expect("formal"),
            PREVIOUS,
            "the previous configuration must be back on disk"
        );
        assert!(
            AUTHED.load(Ordering::SeqCst) >= 1,
            "the recovery must reach a core serving the restored configuration"
        );
        assert!(
            !crate::mihomo_manager::pidfile::is_running(adopted),
            "the replaced core must be stopped, not left behind"
        );
        let running = live.lock().expect("live").clone();
        assert_eq!(
            running
                .iter()
                .filter(|pid| crate::mihomo_manager::pidfile::is_running(**pid))
                .count(),
            1,
            "exactly one replacement core may be left running: {running:?}"
        );
        for pid in running {
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(pid as i32),
                nix::sys::signal::Signal::SIGKILL,
            );
        }

        // The controller is healthy on the restored config...
        assert!(matches!(
            manager.api().version().await,
            Ok(_) | Err(MihomoError::CoreDown { .. })
        ));
        // ...and the no-pid foreign-core refusal is untouched: the receipt
        // only covers the core the policy already accepted.
        let foreign = MihomoManager::new(home.path().join("run"))
            .with_socket(socket)
            .with_singbox_controller("127.0.0.1:19997".parse().expect("addr"))
            .with_core_kind(CoreKind::SingBox);
        assert!(
            foreign
                .capture_restart_authorization()
                .await
                .expect_err("a core with no record stays somebody else's")
                .to_string()
                .contains("no clash-verge-cli pid record"),
            "the global foreign-core refusal must not be loosened by the recovery path"
        );
    }

    /// P1 (reviewer), REAL chain: the rollback recovery must bring the
    /// SERVICE back on the restored config, going through the real
    /// foreground start (`commands::daemon::run` → `manager.start` →
    /// `start_singbox`) and a REAL sing-box process — no stand-in launcher
    /// and no independent HTTP listener.
    ///
    /// Scene: profile B is persisted, the running core serves A, the first
    /// supervisor launch fails. Both the final disk config and the live
    /// service must be A. Before the fix the recovery supervisor
    /// regenerated from the active profile, so the "recovered" service ran B
    /// and A was overwritten again.
    ///
    /// Tagged `#[ignore]` like the repository's other real-core e2e tests
    /// (it spawns a real binary and binds ports); runs with:
    /// `cargo test -p clash-verge-cli -- --ignored`.
    #[tokio::test]
    #[ignore = "spawns a real sing-box core; run: cargo test -p clash-verge-cli -- --ignored"]
    async fn the_recovery_brings_the_service_back_on_the_restored_config_through_the_real_supervisor() {
        use crate::mihomo_manager::pidfile::{self, CoreKind, CoreRecord};
        use std::path::Path;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::{Mutex, OnceLock};

        const PROFILE_A: &str = "proxies: []\nrules:\n  - DOMAIN,a-recovery-marker.example,DIRECT\n  - MATCH,DIRECT\n";
        const PROFILE_B: &str = "proxies: []\nrules:\n  - DOMAIN,b-apply-marker.example,DIRECT\n  - MATCH,DIRECT\n";

        let Some(core_binary) = crate::mihomo_manager::singbox_binary::candidate_without_install() else {
            eprintln!("skipping: no sing-box binary found");
            return;
        };

        let home = tempfile::tempdir().expect("tempdir");
        let _home_guard = crate::profile_store::store::tests::claim_test_app_home(home.path().to_path_buf()).await;

        // Two free ports: the clash_api controller and the mixed port.
        let controller = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
            listener.local_addr().expect("addr")
        };
        let mixed_port = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
            listener.local_addr().expect("addr").port()
        };
        let socket = home.path().join("controller.sock");
        std::fs::write(
            home.path().join("config.yaml"),
            format!(
                "mixed-port: {mixed_port}\nexternal-controller: {controller}\nexternal-controller-unix: {}\n\
secret: fixture-secret\ntun: {{enable: false}}\n",
                socket.display()
            ),
        )
        .expect("config");
        std::fs::write(
            home.path().join("verge.yaml"),
            format!("proxy_core: singbox\nverge_mixed_port: {mixed_port}\n"),
        )
        .expect("verge");
        // The editor has already persisted profile B — the config the failed
        // apply was installing, and the one a regenerating recovery would
        // wrongly serve.
        std::fs::write(
            home.path().join("profiles.yaml"),
            "current: base\nitems:\n  - {uid: base, type: remote, name: fixture, file: base.yaml}\n",
        )
        .expect("profiles");
        std::fs::write(home.path().join("profiles/base.yaml"), PROFILE_B).expect("profile B");

        // Config A: what the core is running right now and what the
        // transaction will restore.
        let formal = clash_verge_core::utils::dirs::singbox_config_path().expect("formal path");
        crate::mihomo_manager::ManagerInner::write_singbox_assembled_to(home.path(), Some(PROFILE_A), false, &formal)
            .await
            .expect("assemble A");
        let config_a = std::fs::read(&formal).expect("read A");
        assert!(String::from_utf8_lossy(&config_a).contains("a-recovery-marker.example"));

        // The old core: a REAL sing-box serving A, recorded the way
        // `spawn_core` records it.
        #[allow(clippy::zombie_processes)]
        let core_log = home.path().join("core-a.log");
        let core_err = std::fs::File::create(&core_log).expect("core log");
        let old_core = std::process::Command::new(&core_binary)
            .arg("run")
            .arg("-c")
            .arg(&formal)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(core_err)
            .spawn()
            .expect("spawn the running core");
        // The old core is killed and reaped even if an assertion fails.
        struct OldCore(std::process::Child);
        impl Drop for OldCore {
            fn drop(&mut self) {
                let _ = nix::sys::signal::kill(
                    nix::unistd::Pid::from_raw(self.0.id() as i32),
                    nix::sys::signal::Signal::SIGKILL,
                );
                let _ = self.0.wait();
            }
        }
        let old_core = OldCore(old_core);
        pidfile::write(
            &pidfile::path_for(&socket),
            CoreRecord::with_kind_and_exe(
                old_core.0.id(),
                chrono::Utc::now(),
                CoreKind::SingBox,
                std::fs::canonicalize(&core_binary)
                    .ok()
                    .map(|p| p.to_string_lossy().into_owned()),
            ),
        )
        .expect("record");

        let config_dir = home.path().join("run");
        std::fs::create_dir_all(&config_dir).expect("config dir");
        let manager = crate::mihomo_manager::MihomoManager::new(config_dir.clone())
            .with_socket(socket.clone())
            .with_singbox_controller(controller)
            .with_core_kind(CoreKind::SingBox)
            .with_secret("fixture-secret".to_string());
        *manager.inner().resolved_binary.lock() = Some(core_binary.clone());
        // Wait for the old core to answer, then adopt it (a real process
        // shape, so adoption validates: cmdline `-c <formal>` and the
        // clash_api endpoint inside it).
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while manager.api().version().await.is_err() && std::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        manager.adopt_running_core();
        assert!(
            manager.pid().is_some(),
            "the running core A must be adopted for the case under test; core log:\n{}",
            std::fs::read_to_string(&core_log).unwrap_or_default()
        );

        struct Seam {
            attempts: AtomicUsize,
            modes: &'static Mutex<Vec<crate::commands::start::SupervisorLaunch>>,
            daemons: &'static Mutex<Vec<tokio::task::JoinHandle<anyhow::Result<()>>>>,
            placeholders: &'static Mutex<Vec<u32>>,
            config_dir: PathBuf,
        }
        static SEAM: OnceLock<&'static Seam> = OnceLock::new();

        /// Panic-safe cleanup: a failed assertion must not leave a real core
        /// (or a placeholder) running after the test.
        struct Cleanup {
            seam: &'static Seam,
            pids: Mutex<Vec<u32>>,
        }
        impl Drop for Cleanup {
            fn drop(&mut self) {
                for handle in self
                    .seam
                    .daemons
                    .lock()
                    .map(|mut d| d.drain(..).collect::<Vec<_>>())
                    .unwrap_or_default()
                {
                    handle.abort();
                }
                let mut pids = self
                    .seam
                    .placeholders
                    .lock()
                    .map(|mut p| p.drain(..).collect::<Vec<_>>())
                    .unwrap_or_default();
                pids.extend(
                    self.pids
                        .lock()
                        .map(|mut p| p.drain(..).collect::<Vec<u32>>())
                        .unwrap_or_default(),
                );
                for pid in pids {
                    let _ = nix::sys::signal::kill(
                        nix::unistd::Pid::from_raw(pid as i32),
                        nix::sys::signal::Signal::SIGKILL,
                    );
                }
            }
        }

        /// The supervisor seam: attempt 0 fails (the injected fault);
        /// attempt 1 runs the REAL foreground supervisor in-process, which
        /// is the code `start --foreground` executes.
        fn launch(
            config_dir: &Path,
            log: &Path,
            mode: crate::commands::start::SupervisorLaunch,
        ) -> anyhow::Result<std::process::Child> {
            let seam = *SEAM.get().expect("seam");
            seam.modes.lock().expect("modes").push(mode);
            if let Some(dir) = log.parent() {
                std::fs::create_dir_all(dir)?;
            }
            std::fs::write(log, b"in-process supervisor\n")?;
            let attempt = seam.attempts.fetch_add(1, Ordering::SeqCst);
            // A placeholder `Child` stands for the detached supervisor process
            // boundary; the supervisor's work itself runs as a task.
            #[allow(clippy::zombie_processes)]
            let placeholder = std::process::Command::new("/bin/sleep")
                .arg("300")
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()?;
            seam.placeholders.lock().expect("placeholders").push(placeholder.id());
            if attempt == 0 {
                anyhow::bail!("injected supervisor launch failure");
            }
            let dir = seam.config_dir.clone();
            seam.daemons.lock().expect("daemons").push(tokio::spawn(async move {
                // The detached child learns its mode from the
                // environment; reproduce that here.
                let guard = if mode == crate::commands::start::SupervisorLaunch::UseExistingConfig {
                    "existing"
                } else {
                    ""
                };
                let previous = std::env::var_os(crate::commands::start::START_CONFIG_MODE_ENV);
                // SAFETY: single-threaded access to the environment is
                // guarded by the app-home lock held for this test.
                unsafe { std::env::set_var(crate::commands::start::START_CONFIG_MODE_ENV, guard) };
                let outcome = crate::commands::daemon::run(dir).await;
                match previous {
                    Some(value) => unsafe { std::env::set_var(crate::commands::start::START_CONFIG_MODE_ENV, value) },
                    None => unsafe { std::env::remove_var(crate::commands::start::START_CONFIG_MODE_ENV) },
                }
                outcome
            }));
            let _ = config_dir;
            Ok(placeholder)
        }

        let modes: &'static Mutex<Vec<crate::commands::start::SupervisorLaunch>> =
            Box::leak(Box::new(Mutex::new(Vec::new())));
        let daemons: &'static Mutex<Vec<tokio::task::JoinHandle<anyhow::Result<()>>>> =
            Box::leak(Box::new(Mutex::new(Vec::new())));
        let placeholders: &'static Mutex<Vec<u32>> = Box::leak(Box::new(Mutex::new(Vec::new())));
        let seam: &'static Seam = Box::leak(Box::new(Seam {
            attempts: AtomicUsize::new(0),
            modes,
            daemons,
            placeholders,
            config_dir,
        }));
        SEAM.set(seam).ok().expect("seam once");

        let _launcher = crate::commands::start::install_supervisor_launcher(launch).expect("install seam");
        let _cleanup = Cleanup {
            seam: SEAM.get().expect("seam"),
            pids: Mutex::new(Vec::new()),
        };
        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(120),
            apply_singbox_restart_for_profile(&manager, Some(PROFILE_B), false, None),
        )
        .await
        .expect("the transaction must finish inside its own timeouts")
        .expect_err("the first supervisor launch was injected to fail");
        assert!(outcome.contains("injected supervisor launch failure"), "{outcome}");
        assert_eq!(seam.attempts.load(Ordering::SeqCst), 2, "the rollback must retry once");
        assert_eq!(
            *seam.modes.lock().expect("modes"),
            vec![
                crate::commands::start::SupervisorLaunch::Regenerate,
                crate::commands::start::SupervisorLaunch::UseExistingConfig,
            ]
        );

        // The recovery is up: the controller answers again, on A's service.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while manager.api().version().await.is_err() && std::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        manager.adopt_running_core();

        // Disk: the restored config, byte for byte. A regeneration would
        // have replaced it with the profile-B generation.
        assert_eq!(
            String::from_utf8_lossy(&std::fs::read(&formal).expect("formal")).to_string(),
            String::from_utf8_lossy(&config_a).to_string(),
            "the restored configuration must survive the recovery"
        );
        // Service: the live core serves A, not B.
        let rules = format!(
            "{:?}",
            manager.api().get_rules().await.expect("query the live core").rules
        );
        assert!(rules.contains("a-recovery-marker.example"), "{rules}");
        assert!(
            !rules.contains("b-apply-marker.example"),
            "the recovered service must not be running the profile that failed to apply: {rules}"
        );
        // Identity: the answering core is a NEW one, recorded for it.
        let record = pidfile::read_record(&pidfile::path_for(&socket)).expect("replacement record");
        assert_eq!(record.kind, CoreKind::SingBox);
        assert_ne!(record.pid, old_core.0.id(), "the recovery must start its own core");
        assert!(pidfile::is_running(record.pid));

        // The recovered core is the only process left to the cleanup guard
        // (which also stops the placeholder children and the supervisor
        // tasks), so even a failing assertion leaves nothing behind.
        _cleanup.pids.lock().expect("pids").push(record.pid);
    }
}
