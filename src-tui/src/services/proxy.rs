//! Proxy groups and delay tests.

use std::collections::HashMap;
use std::sync::Arc;

use crate::mihomo_api::MihomoApi;
use crate::mihomo_api::types::ProxyGroup;

/// Policy pseudo-nodes that must never receive a delay test.
pub const POLICY_PSEUDO_NODES: [&str; 5] = ["DIRECT", "REJECT", "REJECT-DROP", "PASS", "COMPATIBLE"];

pub const DELAY_TEST_URL: &str = "http://www.gstatic.com/generate_204";
pub const DELAY_TEST_TIMEOUT_MS: u64 = 5000;

/// Maximum number of concurrent delay requests for one batch.
pub const MAX_DELAY_CONCURRENCY: usize = 4;

/// Whether `name` is a group (a key whose `all` is present).
pub fn is_group(groups: &HashMap<String, ProxyGroup>, name: &str) -> bool {
    groups.get(name).is_some_and(|group| group.all.is_some())
}

/// Deduplicated, sorted real leaf proxies: members of `group` (or of every
/// group when `None`) that are neither policy pseudo-nodes nor groups.
pub fn leaf_targets(groups: &HashMap<String, ProxyGroup>, group: Option<&str>) -> Vec<String> {
    let members = groups
        .iter()
        .filter(|(name, _)| group.is_none_or(|wanted| wanted == name.as_str()))
        .filter_map(|(_, group)| group.all.as_ref().filter(|nodes| !nodes.is_empty()))
        .flatten();
    let mut targets: Vec<String> = members
        .filter(|name| !POLICY_PSEUDO_NODES.contains(&name.as_str()) && !is_group(groups, name))
        .cloned()
        .collect();
    targets.sort_unstable();
    targets.dedup();
    targets
}

/// Most recent recorded delay for `name` (0 means the last test failed).
pub fn last_delay(groups: &HashMap<String, ProxyGroup>, name: &str) -> Option<u64> {
    groups
        .get(name)
        .and_then(|proxy| proxy.history.as_ref())
        .and_then(|history| history.last())
        .map(|entry| entry.delay)
}

/// Delay-test `targets`, at most [`MAX_DELAY_CONCURRENCY`] at a time.
/// Results keep the order of `targets`.
pub async fn delay_many(
    api: Arc<MihomoApi>,
    targets: Vec<String>,
    url: String,
    timeout_ms: u64,
) -> Vec<(String, Result<u64, String>)> {
    let semaphore = Arc::new(tokio::sync::Semaphore::new(MAX_DELAY_CONCURRENCY));
    let mut handles = Vec::with_capacity(targets.len());
    for name in targets {
        let api = api.clone();
        let url = url.clone();
        let semaphore = semaphore.clone();
        handles.push(tokio::spawn(async move {
            let _permit = semaphore.acquire_owned().await;
            let result = api
                .delay_test(&name, &url, timeout_ms)
                .await
                .map(|delay| delay.delay)
                .map_err(|error| error.to_string());
            (name, result)
        }));
    }
    let mut results = Vec::with_capacity(handles.len());
    for handle in handles {
        if let Ok(result) = handle.await {
            results.push(result);
        }
    }
    results
}

#[cfg(test)]
mod tests {
    use super::*;

    fn group(all: Option<&[&str]>) -> ProxyGroup {
        ProxyGroup {
            group_type: "Selector".into(),
            now: None,
            all: all.map(|nodes| nodes.iter().map(|n| n.to_string()).collect()),
            history: None,
        }
    }

    fn groups() -> HashMap<String, ProxyGroup> {
        HashMap::from([
            ("Proxy".into(), group(Some(&["Auto", "Tokyo", "DIRECT", "Tokyo"]))),
            ("Auto".into(), group(Some(&["Singapore", "Tokyo"]))),
            ("Tokyo".into(), group(None)),
            ("Singapore".into(), group(None)),
        ])
    }

    #[test]
    fn leaf_targets_for_one_group_skip_nested_groups_and_pseudo_nodes() {
        assert_eq!(leaf_targets(&groups(), Some("Proxy")), vec!["Tokyo"]);
        assert_eq!(leaf_targets(&groups(), Some("Auto")), vec!["Singapore", "Tokyo"]);
        assert!(leaf_targets(&groups(), Some("Missing")).is_empty());
    }

    #[test]
    fn leaf_targets_for_all_groups_are_deduplicated_and_sorted() {
        assert_eq!(leaf_targets(&groups(), None), vec!["Singapore", "Tokyo"]);
    }

    #[test]
    fn is_group_only_for_entries_with_members() {
        assert!(is_group(&groups(), "Auto"));
        assert!(!is_group(&groups(), "Tokyo"));
        assert!(!is_group(&groups(), "Missing"));
    }
}
