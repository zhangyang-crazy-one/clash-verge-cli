//! Lightweight subscription auto-update scheduler.
//!
//! Scans remote profiles with `allow_auto_update` + `update_interval` (minutes)
//! and returns UIDs that are due for refresh.

use clash_verge_core::config::PrfItem;
use std::time::{SystemTime, UNIX_EPOCH};

/// Return remote profile UIDs whose update interval has elapsed.
pub fn due_remote_uids(items: &[PrfItem]) -> Vec<String> {
    let now = unix_now_secs();
    items
        .iter()
        .filter_map(|item| {
            if item.itype.as_deref() != Some("remote") {
                return None;
            }
            let option = item.option.as_ref()?;
            if !option.allow_auto_update.unwrap_or(true) {
                return None;
            }
            let interval_minutes = option.update_interval.filter(|v| *v > 0)?;
            let updated = item.updated.unwrap_or(0) as u64;
            let due_at = updated.saturating_add(interval_minutes.saturating_mul(60));
            if now >= due_at {
                item.uid.as_ref().map(|uid| uid.to_string())
            } else {
                None
            }
        })
        .collect()
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

    #[test]
    fn due_when_interval_elapsed() {
        let now = unix_now_secs() as usize;
        let items = vec![PrfItem {
            uid: Some("Rdue".into()),
            itype: Some("remote".into()),
            updated: Some(now.saturating_sub(120)),
            option: Some(PrfOption {
                allow_auto_update: Some(true),
                update_interval: Some(1), // 1 minute
                ..Default::default()
            }),
            ..Default::default()
        }];
        assert_eq!(due_remote_uids(&items), vec!["Rdue".to_string()]);
    }

    #[test]
    fn skips_when_auto_update_disabled() {
        let now = unix_now_secs() as usize;
        let items = vec![PrfItem {
            uid: Some("Rskip".into()),
            itype: Some("remote".into()),
            updated: Some(now.saturating_sub(10_000)),
            option: Some(PrfOption {
                allow_auto_update: Some(false),
                update_interval: Some(1),
                ..Default::default()
            }),
            ..Default::default()
        }];
        assert!(due_remote_uids(&items).is_empty());
    }

    #[test]
    fn skips_when_not_yet_due() {
        let now = unix_now_secs() as usize;
        let items = vec![PrfItem {
            uid: Some("Rwait".into()),
            itype: Some("remote".into()),
            updated: Some(now),
            option: Some(PrfOption {
                allow_auto_update: Some(true),
                update_interval: Some(60),
                ..Default::default()
            }),
            ..Default::default()
        }];
        assert!(due_remote_uids(&items).is_empty());
    }
}
