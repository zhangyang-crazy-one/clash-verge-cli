//! Shared subscription auto-update scheduler used by both the interactive TUI
//! event loop and the headless daemon (`start --foreground` / systemd).
//!
//! Two responsibilities:
//!
//! 1. **Interval refresh** — each tick re-snapshots `profiles.yaml` from disk,
//!    so external edits (via the GUI or the user) are picked up without a
//!    restart, and records per-UID attempt times so a failed refresh cools
//!    down instead of being retried on every tick.
//! 2. **Probe recovery** — when the currently selected node (the user's fixed
//!    exit) fails its delay test repeatedly, force-refresh the subscription
//!    immediately, bypassing the interval. If the refresh would lose the
//!    selected node name, the refresh is rolled back to preserve the exit.

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use clash_verge_core::config::PrfItem;
use serde_yaml_ng::{Mapping, Value};

use crate::mihomo_api::MihomoApi;
use crate::mihomo_api::error::MihomoError;
use crate::mihomo_api::types::ProxyDelay;
use crate::mihomo_manager::{CoreKind, MihomoManager};
use crate::profile_store::store::ProfileStore;

/// Strategy the auto-update scheduler uses to push a freshly refreshed
/// remote profile to the running core.
///
/// Hot reload is the mihomo path (`PUT /configs?force=true` against the
/// clash-api controller). Sing-box's controller accepts the request but
/// ignores the payload — every config change must go through
/// [`crate::runtime_config::apply_singbox_restart`], which regenerates
/// the JSON config, prevalidates with `sing-box check`, and restarts
/// the process. Used by the manager-aware wrappers in this module; the
/// mihomo-only `probe` / `reload_current_profile` / `force_refresh`
/// entry points are kept for the headless daemon and CLI commands
/// (out-of-scope here; they remain sing-box-broken — see `notes` in the
/// public function doc-comments).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RefreshPath {
    HotReload,
    SingboxRestart,
}

pub(crate) fn decide_refresh_path(kind: CoreKind) -> RefreshPath {
    match kind {
        CoreKind::Mihomo => RefreshPath::HotReload,
        CoreKind::SingBox => RefreshPath::SingboxRestart,
    }
}

/// Minimum time between attempts of the same profile after a refresh failure.
/// At least the profile's `update_interval` is enforced by `due_remote_uids`
/// (a failed refresh leaves `updated` stale, so the profile stays due); this
/// floor prevents hammering short-interval profiles.
const MIN_COOLDOWN_SECS: u64 = 30 * 60;

/// Probe test target and timeout (same as the manual delay test in the TUI).
const PROBE_TEST_URL: &str = "http://www.gstatic.com/generate_204";
const PROBE_TIMEOUT_MS: u64 = 5000;
/// Consecutive node failures (~90 s at a 30 s probe tick) before forcing a
/// refresh.
const PROBE_FAILURE_THRESHOLD: u32 = 3;
/// Debounce between forced refreshes.
const FORCE_REFRESH_DEBOUNCE_SECS: u64 = 5 * 60;

/// Outcome of one scheduler tick.
#[derive(Debug, Default)]
pub struct SchedulerOutcome {
    /// `(uid, is_current)` for successfully refreshed profiles.
    pub updated: Vec<(String, bool)>,
    /// `(uid, error)` for per-profile failures.
    pub failed: Vec<(String, String)>,
    /// Whole-batch failure (e.g. `profiles.yaml` unreadable).
    pub errored: Option<String>,
}

/// Outcome of one probe pass.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ProbeOutcome {
    /// A forced refresh was triggered by the probe.
    pub forced_refresh: bool,
    /// The forced refresh was rolled back because the selected node name
    /// vanished from the refreshed config (fixed-exit preservation).
    pub rolled_back: bool,
    /// The exit node still fails its delay test after a forced refresh.
    pub may_be_down: bool,
    /// Probe infrastructure error (not a user-facing notice).
    pub error: Option<String>,
}

/// How a delay-test result is classified for probe purposes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeVerdict {
    /// The node answered with a positive delay.
    Alive,
    /// The node is reachable via the controller but failed the test.
    Dead,
    /// The controller itself is unavailable — not a node problem.
    ApiIssue,
}

/// Classify a delay-test result. API-level failures (core down, bad secret)
/// must never count as node failures.
pub fn classify_delay(result: &Result<ProxyDelay, MihomoError>) -> ProbeVerdict {
    match result {
        Ok(delay) if delay.delay > 0 => ProbeVerdict::Alive,
        Ok(_) => ProbeVerdict::Dead,
        Err(MihomoError::CoreDown { .. } | MihomoError::Unauthorized) => ProbeVerdict::ApiIssue,
        Err(_) => ProbeVerdict::Dead,
    }
}

/// Policy pseudo-nodes cannot be delay-tested.
fn is_policy_node(name: &str) -> bool {
    matches!(name, "DIRECT" | "REJECT" | "REJECT-DROP" | "PASS" | "COMPATIBLE")
}

/// Whether a parsed clash config still contains `node` (top-level `proxies`
/// or a `proxy-groups` reference). Used for the fixed-exit rollback check.
pub fn config_contains_node(config: &Mapping, node: &str) -> bool {
    if let Some(Value::Sequence(proxies)) = config.get("proxies") {
        for proxy in proxies {
            if let Value::Mapping(map) = proxy
                && map.get("name").and_then(Value::as_str) == Some(node)
            {
                return true;
            }
        }
    }
    if let Some(Value::Sequence(groups)) = config.get("proxy-groups") {
        for group in groups {
            if let Value::Mapping(map) = group
                && let Some(Value::Sequence(names)) = map.get("proxies")
                && names.iter().any(|name| name.as_str() == Some(node))
            {
                return true;
            }
        }
    }
    false
}

/// Tracks per-profile attempt times and probe state.
#[derive(Debug, Default)]
pub struct AutoUpdateScheduler {
    /// uid -> unix seconds of the last refresh attempt (success or failure).
    last_attempt: HashMap<String, u64>,
    /// Floor on the retry gap after a failure.
    min_cooldown_secs: u64,
    /// Consecutive probe failures of the current exit node.
    probe_failures: u32,
    /// Unix seconds of the last forced (probe-triggered) refresh.
    last_force_refresh: u64,
}

impl AutoUpdateScheduler {
    pub fn new() -> Self {
        Self {
            last_attempt: HashMap::new(),
            min_cooldown_secs: MIN_COOLDOWN_SECS,
            probe_failures: 0,
            last_force_refresh: 0,
        }
    }

    /// Snapshot profiles, compute due UIDs (honoring cooldowns), refresh them.
    pub async fn tick(&mut self) -> SchedulerOutcome {
        let now = unix_now_secs();
        let items = match ProfileStore::snapshot().await {
            Ok(store) => store.items(),
            Err(error) => {
                return SchedulerOutcome {
                    errored: Some(error.to_string()),
                    ..Default::default()
                };
            }
        };
        let due = self.compute_due(&items, now);
        let outcome = self.run_batch(&due).await;
        // A successful refresh resets the attempt so the next due time is
        // governed purely by the profile's `update_interval`, not the cooldown.
        for (uid, _) in &outcome.updated {
            self.record_success(uid);
        }
        outcome
    }

    /// Compute the due UIDs for a fresh profile snapshot, recording attempt
    /// times for the profiles selected. Extracted from `tick` for unit tests.
    fn compute_due(&mut self, items: &[PrfItem], now: u64) -> Vec<String> {
        crate::subscribe::timer::due_remote_uids(items)
            .into_iter()
            .filter(|uid| {
                let cooled = self
                    .last_attempt
                    .get(uid)
                    .is_none_or(|last| now.saturating_sub(*last) >= self.min_cooldown_secs);
                if cooled {
                    self.last_attempt.insert(uid.clone(), now);
                }
                cooled
            })
            .collect()
    }

    /// Clear the cooldown for a successfully refreshed profile.
    pub fn record_success(&mut self, uid: &str) {
        self.last_attempt.remove(uid);
    }

    /// Refresh a batch of UIDs. Per-UID failures do not block the rest.
    async fn run_batch(&self, due: &[String]) -> SchedulerOutcome {
        if due.is_empty() {
            return SchedulerOutcome::default();
        }
        match ProfileStore::update_remotes_locked(due).await {
            Ok((updated, failed)) => SchedulerOutcome {
                updated,
                failed,
                errored: None,
            },
            Err(error) => SchedulerOutcome {
                errored: Some(error.to_string()),
                ..Default::default()
            },
        }
    }

    /// Probe the current exit node and force a subscription refresh when it
    /// dies. Returns notices for the caller to surface. No-op unless
    /// `probe_enabled` is set (default on), the core is running, and a
    /// current profile with a delay-testable exit node exists.
    ///
    /// **Mihomo-only entry point.** A sing-box forced refresh also needs
    /// the manager (to restart the process). Use [`probe_with_manager`]
    /// for the dispatching path used by the TUI; the headless daemon still
    /// calls this and remains sing-box-broken (out of scope here).
    pub async fn probe(&mut self, api: &MihomoApi, enable_tun: bool, core_running: bool) -> ProbeOutcome {
        let gui = clash_verge_core::config::IVerge::new().await;
        if !gui.probe_enabled.unwrap_or(true) {
            return ProbeOutcome::default();
        }
        if !core_running {
            return ProbeOutcome::default();
        }

        let store = match ProfileStore::snapshot().await {
            Ok(store) => store,
            Err(error) => {
                return ProbeOutcome {
                    error: Some(error.to_string()),
                    ..Default::default()
                };
            }
        };
        let Some(current) = store.current_uid() else {
            return ProbeOutcome::default();
        };
        let items = store.items();
        let Some(node) = current_exit_node(api, &items, &current).await else {
            return ProbeOutcome::default();
        };

        let verdict = classify_delay(&api.delay_test(&node, PROBE_TEST_URL, PROBE_TIMEOUT_MS).await);
        match verdict {
            ProbeVerdict::Alive => {
                self.probe_failures = 0;
                ProbeOutcome::default()
            }
            ProbeVerdict::ApiIssue => ProbeOutcome::default(),
            ProbeVerdict::Dead => {
                if self.record_probe_failure(unix_now_secs()) {
                    Self::force_refresh(api, &current, &node, enable_tun, core_running).await
                } else {
                    ProbeOutcome::default()
                }
            }
        }
    }

    /// Probe the current exit node and, on a sustained failure, force a
    /// subscription refresh that dispatches by [`CoreKind`]. The
    /// controller transport is reached through `manager.api()`; the
    /// forced-refresh path additionally takes the manager so sing-box can
    /// restart through [`crate::runtime_config::apply_singbox_restart`]
    /// instead of `reload_current_profile` (a no-op for sing-box).
    ///
    /// Used by the TUI's auto-update tick (`spawn_auto_update`). The
    /// headless daemon still calls [`probe`](Self::probe) and stays on
    /// the mihomo-only path.
    pub async fn probe_with_manager(
        &mut self,
        manager: &MihomoManager,
        enable_tun: bool,
        core_running: bool,
    ) -> ProbeOutcome {
        let gui = clash_verge_core::config::IVerge::new().await;
        if !gui.probe_enabled.unwrap_or(true) {
            return ProbeOutcome::default();
        }
        if !core_running {
            return ProbeOutcome::default();
        }

        let api = manager.api();
        let store = match ProfileStore::snapshot().await {
            Ok(store) => store,
            Err(error) => {
                return ProbeOutcome {
                    error: Some(error.to_string()),
                    ..Default::default()
                };
            }
        };
        let Some(current) = store.current_uid() else {
            return ProbeOutcome::default();
        };
        let items = store.items();
        let Some(node) = current_exit_node(&api, &items, &current).await else {
            return ProbeOutcome::default();
        };

        let verdict = classify_delay(&api.delay_test(&node, PROBE_TEST_URL, PROBE_TIMEOUT_MS).await);
        match verdict {
            ProbeVerdict::Alive => {
                self.probe_failures = 0;
                ProbeOutcome::default()
            }
            ProbeVerdict::ApiIssue => ProbeOutcome::default(),
            ProbeVerdict::Dead => {
                if self.record_probe_failure(unix_now_secs()) {
                    Self::force_refresh_with_manager(manager, &current, &node, enable_tun, core_running).await
                } else {
                    ProbeOutcome::default()
                }
            }
        }
    }

    /// Count a node failure; returns true when the threshold is reached and
    /// the forced-refresh debounce has elapsed. Extracted for unit tests.
    fn record_probe_failure(&mut self, now: u64) -> bool {
        self.probe_failures += 1;
        let due = self.probe_failures >= PROBE_FAILURE_THRESHOLD
            && now.saturating_sub(self.last_force_refresh) >= FORCE_REFRESH_DEBOUNCE_SECS;
        if due {
            self.probe_failures = 0;
            self.last_force_refresh = now;
        }
        due
    }

    /// Force-refresh the current profile, bypassing interval and cooldown.
    /// Preserves the fixed exit: if the selected node name vanishes from the
    /// refreshed config, the old profile file and `updated` timestamp are
    /// restored and the old config reloaded.
    async fn force_refresh(
        api: &MihomoApi,
        uid: &str,
        node: &str,
        enable_tun: bool,
        core_running: bool,
    ) -> ProbeOutcome {
        let items = match ProfileStore::snapshot().await {
            Ok(store) => store.items(),
            Err(error) => {
                return ProbeOutcome {
                    error: Some(error.to_string()),
                    ..Default::default()
                };
            }
        };
        let Some((old_bytes, old_updated)) = profile_snapshot(&items, uid).await else {
            return ProbeOutcome {
                error: Some(format!("profile {uid} file unavailable for snapshot")),
                ..Default::default()
            };
        };

        match ProfileStore::update_remotes_locked(&[uid.to_string()]).await {
            Ok((updated, _failed)) if !updated.is_empty() => {
                if refreshed_config_has_node(uid, node).await {
                    match reload_current_profile(api, uid, enable_tun, core_running).await {
                        Ok(()) => {
                            let verdict = classify_delay(&api.delay_test(node, PROBE_TEST_URL, PROBE_TIMEOUT_MS).await);
                            ProbeOutcome {
                                forced_refresh: true,
                                may_be_down: verdict != ProbeVerdict::Alive,
                                ..Default::default()
                            }
                        }
                        Err(error) => ProbeOutcome {
                            forced_refresh: true,
                            error: Some(format!("reload after forced refresh: {error}")),
                            ..Default::default()
                        },
                    }
                } else {
                    // Fixed-exit rollback: restore the old file and timestamp.
                    let file = items
                        .iter()
                        .find(|item| item.uid.as_deref() == Some(uid))
                        .and_then(|item| item.file.clone());
                    if let Some(file) = file {
                        restore_profile_snapshot(uid, &file, &old_bytes, old_updated).await;
                    }
                    let _ = reload_current_profile(api, uid, enable_tun, core_running).await;
                    ProbeOutcome {
                        forced_refresh: true,
                        rolled_back: true,
                        ..Default::default()
                    }
                }
            }
            Ok(_) => ProbeOutcome {
                forced_refresh: true,
                error: Some(format!("forced refresh of {uid} produced no update")),
                ..Default::default()
            },
            Err(error) => ProbeOutcome {
                error: Some(format!("forced refresh failed: {error}")),
                ..Default::default()
            },
        }
    }

    /// Force-refresh the current profile, dispatching the post-refresh
    /// config application by [`CoreKind`]. Same fixed-exit rollback as
    /// [`force_refresh`](Self::force_refresh); the sing-box branch routes
    /// through [`reload_current_profile_for_core`] so the JSON config is
    /// regenerated and the core restarted instead of issuing a no-op
    /// `PUT /configs`.
    async fn force_refresh_with_manager(
        manager: &MihomoManager,
        uid: &str,
        node: &str,
        enable_tun: bool,
        core_running: bool,
    ) -> ProbeOutcome {
        let api = manager.api();
        let items = match ProfileStore::snapshot().await {
            Ok(store) => store.items(),
            Err(error) => {
                return ProbeOutcome {
                    error: Some(error.to_string()),
                    ..Default::default()
                };
            }
        };
        let Some((old_bytes, old_updated)) = profile_snapshot(&items, uid).await else {
            return ProbeOutcome {
                error: Some(format!("profile {uid} file unavailable for snapshot")),
                ..Default::default()
            };
        };

        match ProfileStore::update_remotes_locked(&[uid.to_string()]).await {
            Ok((updated, _failed)) if !updated.is_empty() => {
                if refreshed_config_has_node(uid, node).await {
                    match reload_current_profile_for_core(manager, uid, enable_tun, core_running).await {
                        Ok(()) => {
                            let verdict = classify_delay(&api.delay_test(node, PROBE_TEST_URL, PROBE_TIMEOUT_MS).await);
                            ProbeOutcome {
                                forced_refresh: true,
                                may_be_down: verdict != ProbeVerdict::Alive,
                                ..Default::default()
                            }
                        }
                        Err(error) => ProbeOutcome {
                            forced_refresh: true,
                            error: Some(format!("reload after forced refresh: {error}")),
                            ..Default::default()
                        },
                    }
                } else {
                    let file = items
                        .iter()
                        .find(|item| item.uid.as_deref() == Some(uid))
                        .and_then(|item| item.file.clone());
                    if let Some(file) = file {
                        restore_profile_snapshot(uid, &file, &old_bytes, old_updated).await;
                    }
                    let _ = reload_current_profile_for_core(manager, uid, enable_tun, core_running).await;
                    ProbeOutcome {
                        forced_refresh: true,
                        rolled_back: true,
                        ..Default::default()
                    }
                }
            }
            Ok(_) => ProbeOutcome {
                forced_refresh: true,
                error: Some(format!("forced refresh of {uid} produced no update")),
                ..Default::default()
            },
            Err(error) => ProbeOutcome {
                error: Some(format!("forced refresh failed: {error}")),
                ..Default::default()
            },
        }
    }
}

/// Resolve the current exit node: the live `GLOBAL` group selection from the
/// controller, falling back to the saved `PrfSelected` entry.
async fn current_exit_node(api: &MihomoApi, items: &[PrfItem], current_uid: &str) -> Option<String> {
    if let Ok(data) = api.get_proxies().await
        && let Some(global) = data.proxies.get("GLOBAL")
        && let Some(now) = global.now.as_deref()
        && !is_policy_node(now)
    {
        return Some(now.to_string());
    }
    items
        .iter()
        .find(|item| item.uid.as_deref() == Some(current_uid))
        .and_then(|item| item.selected.as_ref())
        .and_then(|selected| selected.iter().find(|entry| entry.name.as_deref() == Some("GLOBAL")))
        .and_then(|entry| entry.now.as_deref().map(str::to_string))
}

/// Snapshot a remote profile's file bytes and `updated` timestamp so a forced
/// refresh can be rolled back.
async fn profile_snapshot(items: &[PrfItem], uid: &str) -> Option<(Vec<u8>, usize)> {
    let item = items.iter().find(|item| item.uid.as_deref() == Some(uid))?;
    let file = item.file.as_ref()?;
    let path = clash_verge_core::utils::dirs::app_profiles_dir()
        .ok()?
        .join(file.as_str());
    let bytes = tokio::fs::read(&path).await.ok()?;
    Some((bytes, item.updated.unwrap_or(0)))
}

/// Restore a profile's file bytes and `updated` timestamp (rollback path).
async fn restore_profile_snapshot(uid: &str, file: &str, bytes: &[u8], updated: usize) {
    if let Ok(dir) = clash_verge_core::utils::dirs::app_profiles_dir() {
        let _ = tokio::fs::write(dir.join(file), bytes).await;
    }
    let _ = ProfileStore::restore_updated_locked(uid, updated).await;
}

/// Whether the freshly refreshed profile config still contains `node`.
async fn refreshed_config_has_node(uid: &str, node: &str) -> bool {
    let Ok(store) = ProfileStore::snapshot().await else {
        return false;
    };
    let Some(item) = store.items().into_iter().find(|item| item.uid.as_deref() == Some(uid)) else {
        return false;
    };
    let Some(file) = item.file.as_ref() else {
        return false;
    };
    let Ok(dir) = clash_verge_core::utils::dirs::app_profiles_dir() else {
        return false;
    };
    let Ok(raw) = tokio::fs::read_to_string(dir.join(file.as_str())).await else {
        return false;
    };
    let Ok(config) = serde_yaml_ng::from_str::<Mapping>(&raw) else {
        return false;
    };
    config_contains_node(&config, node)
}

/// Reload the running core from a freshly refreshed current profile.
/// Shared by the TUI (which sends `ProxiesRefresh` after success) and the
/// daemon (which logs the outcome).
///
/// **Mihomo-only entry point.** Sing-box's `PUT /configs` is a no-op, so
/// calling this against a sing-box controller silently leaves the running
/// config unchanged. The TUI uses [`reload_current_profile_for_core`]
/// instead; the headless daemon and CLI `profile update --reload` still
/// call this function and remain sing-box-broken (out of scope here).
pub async fn reload_current_profile(
    api: &MihomoApi,
    uid: &str,
    enable_tun: bool,
    core_running: bool,
) -> Result<(), String> {
    let store = ProfileStore::snapshot().await.map_err(|error| error.to_string())?;
    let item = store
        .items()
        .into_iter()
        .find(|item| item.uid.as_deref() == Some(uid))
        .ok_or_else(|| format!("profile {uid} not found after refresh"))?;
    crate::runtime_config::reload_remote_profile(api, &item, enable_tun, core_running).await
}

/// Reload the running core from a freshly refreshed current profile,
/// dispatching by [`CoreKind`]. Mirrors [`reload_current_profile`] but
/// additionally handles sing-box, whose controller does not honour
/// `PUT /configs` — the sing-box path regenerates the JSON config from
/// the on-disk profile YAML and restarts the core (with prevalidation +
/// rollback inside [`crate::runtime_config::apply_singbox_restart`]).
///
/// Used by the TUI (`tui/handlers/profile.rs::note_updated`). The
/// scheduler's probe path uses [`AutoUpdateScheduler::probe_with_manager`]
/// to forward the manager through to a forced refresh.
pub async fn reload_current_profile_for_core(
    manager: &MihomoManager,
    uid: &str,
    enable_tun: bool,
    core_running: bool,
) -> Result<(), String> {
    match decide_refresh_path(manager.core_kind()) {
        RefreshPath::HotReload => reload_current_profile(&manager.api(), uid, enable_tun, core_running).await,
        RefreshPath::SingboxRestart => {
            // The refresh already wrote the new YAML to disk via
            // `ProfileStore::update_remote_locked`; read it back so the
            // sing-box pipeline regenerates from the post-refresh body.
            let store = ProfileStore::snapshot().await.map_err(|error| error.to_string())?;
            let item = store
                .items()
                .into_iter()
                .find(|item| item.uid.as_deref() == Some(uid))
                .ok_or_else(|| format!("profile {uid} not found after refresh"))?;
            let yaml = read_profile_yaml(&item).await?;
            crate::runtime_config::apply_singbox_restart(manager, Some(yaml.as_str()), enable_tun)
                .await
                .map(|_report| ())
        }
    }
}

/// Read a profile's on-disk YAML body. The sing-box path needs the raw
/// upstream YAML because the sing-box pipeline (`write_singbox_assembled`)
/// runs its own conversion; pulling from the in-memory store would skip
/// any post-refresh writes that the caller flushed just before calling.
///
/// Returns an error early when the profile has no `file` reference or
/// the file is missing — the sing-box path would otherwise fall back to
/// the skeleton config, which silently produces an empty outbound set.
async fn read_profile_yaml(item: &PrfItem) -> Result<String, String> {
    let file = item
        .file
        .as_deref()
        .ok_or_else(|| format!("profile {} has no file", item.uid.as_deref().unwrap_or("?")))?;
    let profiles_dir =
        clash_verge_core::utils::dirs::app_profiles_dir().map_err(|error| format!("profiles dir: {error}"))?;
    read_profile_yaml_from(&profiles_dir, file).await
}

/// Test seam: read a profile file from an explicit directory. The caller-
/// facing wrapper resolves `app_profiles_dir()` so production code keeps
/// one path; this lets unit tests avoid the global app-home OnceLock.
async fn read_profile_yaml_from(profiles_dir: &std::path::Path, file: &str) -> Result<String, String> {
    let path = profiles_dir.join(file);
    tokio::fs::read_to_string(&path)
        .await
        .map_err(|error| format!("failed to read profile {}: {error}", path.display()))
}

fn unix_now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clash_verge_core::config::PrfOption;

    fn due_item(uid: &str, interval_minutes: u64, updated_at: u64) -> PrfItem {
        PrfItem {
            uid: Some(uid.into()),
            itype: Some("remote".into()),
            updated: Some(updated_at as usize),
            option: Some(PrfOption {
                allow_auto_update: Some(true),
                update_interval: Some(interval_minutes),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn decide_refresh_path_picks_hot_reload_for_mihomo() {
        // The dispatcher lives in `subscribe::scheduler`; this is the one
        // assertion all three TUI callsites rely on. Mihomo must stay on
        // `reload_current_profile` (PUT /configs hot reload) so the
        // existing rule-fragment composition keeps applying.
        assert_eq!(decide_refresh_path(CoreKind::Mihomo), RefreshPath::HotReload);
    }

    #[test]
    fn decide_refresh_path_picks_singbox_restart_for_singbox() {
        // sing-box's controller does NOT honour `PUT /configs` — every
        // profile refresh must regenerate the JSON config and restart
        // the process through `apply_singbox_restart`.
        assert_eq!(decide_refresh_path(CoreKind::SingBox), RefreshPath::SingboxRestart);
    }

    #[test]
    fn manager_core_kind_propagates_to_dispatch_decision() {
        // The dispatcher is invoked from `reload_current_profile_for_core`
        // and `force_refresh_with_manager`; both reach the manager via
        // `&MihomoManager`. Confirm `core_kind()` round-trips so the
        // dispatcher sees the value the manager was built with.
        let mgr = MihomoManager::new(std::path::PathBuf::from("/tmp/cfg"));
        assert_eq!(decide_refresh_path(mgr.core_kind()), RefreshPath::HotReload);
        let mgr = mgr.with_core_kind(CoreKind::SingBox);
        assert_eq!(decide_refresh_path(mgr.core_kind()), RefreshPath::SingboxRestart);
    }

    #[tokio::test]
    async fn read_profile_yaml_from_returns_file_contents() {
        // The sing-box path reads the post-refresh YAML straight from disk
        // (the refresh wrote it via `update_remote_locked`); the helper
        // must surface the raw body for `apply_singbox_restart` to feed
        // into the sing-box pipeline.
        let dir = std::env::temp_dir().join(format!(
            "clash-verge-sched-yaml-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        std::fs::write(dir.join("sub.yaml"), "proxies: []\n").expect("write");

        let body = read_profile_yaml_from(&dir, "sub.yaml").await.expect("read");
        assert_eq!(body, "proxies: []\n");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn read_profile_yaml_from_surfaces_missing_file_error() {
        // A vanished file must not silently fall through to the skeleton
        // config (which would produce an empty outbound set and a
        // confusing "skeleton applied" report). The helper must reject
        // before `apply_singbox_restart` is reached.
        let dir = std::env::temp_dir().join(format!(
            "clash-verge-sched-yaml-missing-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");

        let error = read_profile_yaml_from(&dir, "nope.yaml")
            .await
            .expect_err("missing file must error");
        assert!(
            error.contains("nope.yaml") && error.contains("failed to read"),
            "error names the missing file: {error}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn failed_profile_cools_down_until_min_cooldown() {
        let mut scheduler = AutoUpdateScheduler::new();
        let now = unix_now_secs();
        let items = vec![due_item("Rcool", 1, now.saturating_sub(120))];

        // First pass: due (interval elapsed), attempt recorded.
        assert_eq!(scheduler.compute_due(&items, now), vec!["Rcool"]);
        // Immediate retry on the next tick is suppressed by the cooldown.
        assert!(scheduler.compute_due(&items, now).is_empty());
        // After the 30-minute cooldown floor, the profile is due again.
        let later = now + MIN_COOLDOWN_SECS;
        assert_eq!(scheduler.compute_due(&items, later), vec!["Rcool"]);
    }

    #[test]
    fn success_resets_the_cooldown() {
        let mut scheduler = AutoUpdateScheduler::new();
        let now = unix_now_secs();
        let items = vec![due_item("Rok", 1, now.saturating_sub(120))];

        assert_eq!(scheduler.compute_due(&items, now), vec!["Rok"]);
        assert!(scheduler.compute_due(&items, now).is_empty());

        // A successful refresh clears the attempt so the next tick can run again.
        scheduler.record_success("Rok");
        assert_eq!(scheduler.compute_due(&items, now), vec!["Rok"]);
    }

    #[test]
    fn external_interval_edit_is_picked_up_without_restart() {
        let mut scheduler = AutoUpdateScheduler::new();
        let now = unix_now_secs();

        // Stale interval (1440 min) → not yet due.
        let stale = vec![due_item("Rfresh", 1440, now.saturating_sub(1200))];
        assert!(scheduler.compute_due(&stale, now).is_empty());

        // Fresh snapshot (next tick) sees the edited interval (15 min) → due.
        let fresh = vec![due_item("Rfresh", 15, now.saturating_sub(1200))];
        assert_eq!(scheduler.compute_due(&fresh, now), vec!["Rfresh"]);
    }

    #[test]
    fn classify_delay_distinguishes_node_from_api_failures() {
        let alive = ProxyDelay { delay: 42 };
        assert_eq!(classify_delay(&Ok(alive)), ProbeVerdict::Alive);

        let zero = ProxyDelay { delay: 0 };
        assert_eq!(classify_delay(&Ok(zero)), ProbeVerdict::Dead);

        assert_eq!(
            classify_delay(&Err(MihomoError::CoreDown {
                endpoint: "/tmp/x.sock".into()
            })),
            ProbeVerdict::ApiIssue
        );
        assert_eq!(classify_delay(&Err(MihomoError::Unauthorized)), ProbeVerdict::ApiIssue);
        assert_eq!(
            classify_delay(&Err(MihomoError::NotFound("no proxy".into()))),
            ProbeVerdict::Dead
        );
    }

    #[test]
    fn probe_failure_threshold_and_debounce() {
        let mut scheduler = AutoUpdateScheduler::new();
        let now = 1_000_000;

        // Two failures: below the threshold, no forced refresh.
        assert!(!scheduler.record_probe_failure(now));
        assert!(!scheduler.record_probe_failure(now));

        // Third failure: threshold reached → forced refresh, state reset.
        assert!(scheduler.record_probe_failure(now));
        assert!(!scheduler.record_probe_failure(now + 60));

        // Debounce: a new burst inside 5 minutes cannot force again...
        assert!(!scheduler.record_probe_failure(now + 100));
        assert!(!scheduler.record_probe_failure(now + 160));
        assert!(!scheduler.record_probe_failure(now + 220));
        // ...but once the debounce elapses, accumulated failures force
        // immediately (the node is still down — no need to re-accumulate).
        let later = now + FORCE_REFRESH_DEBOUNCE_SECS + 1;
        assert!(scheduler.record_probe_failure(later));
        // State was reset by the forced refresh at `later`: the next burst
        // needs 3 failures again, and the new debounce window must elapse.
        assert!(!scheduler.record_probe_failure(later + 1)); // 1/3
        assert!(!scheduler.record_probe_failure(later + 2)); // 2/3
        let after = later + FORCE_REFRESH_DEBOUNCE_SECS + 1;
        assert!(scheduler.record_probe_failure(after)); // 3/3 + debounce passed
        // And the cycle repeats: 3 failures inside the next window don't force.
        assert!(!scheduler.record_probe_failure(after + 1));
        assert!(!scheduler.record_probe_failure(after + 2));
        assert!(!scheduler.record_probe_failure(after + 100));
    }

    #[test]
    fn config_contains_node_checks_proxies_and_group_references() {
        let config: Mapping = match serde_yaml_ng::from_str(
            r#"
proxies:
  - name: JP-4
    type: ss
proxy-groups:
  - name: GLOBAL
    proxies: [JP-4, JP-5, DIRECT]
"#,
        ) {
            Ok(config) => config,
            Err(error) => panic!("invalid test yaml: {error}"),
        };

        assert!(config_contains_node(&config, "JP-4"));
        assert!(config_contains_node(&config, "JP-5"));
        assert!(!config_contains_node(&config, "US-1"));
    }

    #[test]
    fn policy_nodes_are_not_probe_targets() {
        assert!(is_policy_node("DIRECT"));
        assert!(is_policy_node("REJECT"));
        assert!(!is_policy_node("JP-4"));
    }

    #[test]
    fn rollback_decision_is_the_inverse_of_node_presence() {
        // The forced-refresh path reloads when the node survives and rolls
        // back when it vanishes; `config_contains_node` is the decision core.
        let with_node: Mapping = match serde_yaml_ng::from_str("proxies:\n  - {name: M, type: ss}\n") {
            Ok(config) => config,
            Err(error) => panic!("invalid test yaml: {error}"),
        };
        let without_node: Mapping = match serde_yaml_ng::from_str("proxies:\n  - {name: JP-9, type: ss}\n") {
            Ok(config) => config,
            Err(error) => panic!("invalid test yaml: {error}"),
        };

        assert!(config_contains_node(&with_node, "M"));
        assert!(!config_contains_node(&without_node, "M"));
    }
}
