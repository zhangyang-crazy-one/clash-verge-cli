//! Shared subscription auto-update scheduler used by both the interactive TUI
//! event loop and the headless daemon (`start --foreground` / systemd).
//!
//! Each tick re-snapshots `profiles.yaml` from disk, so external edits (e.g.
//! via the GUI or the user) are picked up without a restart, and records
//! per-UID attempt times so a failed refresh cools down instead of being
//! retried on every tick.

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use clash_verge_core::config::PrfItem;

use crate::profile_store::store::ProfileStore;

/// Minimum time between attempts of the same profile after a refresh failure.
/// At least the profile's `update_interval` is enforced by `due_remote_uids`
/// (a failed refresh leaves `updated` stale, so the profile stays due); this
/// floor prevents hammering short-interval profiles.
const MIN_COOLDOWN_SECS: u64 = 30 * 60;

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

/// Tracks per-profile attempt times so failures cool down.
#[derive(Debug, Default)]
pub struct AutoUpdateScheduler {
    /// uid -> unix seconds of the last refresh attempt (success or failure).
    last_attempt: HashMap<String, u64>,
    /// Floor on the retry gap after a failure.
    min_cooldown_secs: u64,
}

impl AutoUpdateScheduler {
    pub fn new() -> Self {
        Self {
            last_attempt: HashMap::new(),
            min_cooldown_secs: MIN_COOLDOWN_SECS,
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
}

/// Reload the running core from a freshly refreshed current profile.
/// Shared by the TUI (which sends `ProxiesRefresh` after success) and the
/// daemon (which logs the outcome).
pub async fn reload_current_profile(
    api: &crate::mihomo_api::MihomoApi,
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
}
