//! Proxy groups and delay tests.

use std::collections::HashMap;
use std::sync::Arc;

use crate::mihomo_api::MihomoApi;
use crate::mihomo_api::types::{ProxyDelayTarget, ProxyGroup, ProxyProvidersResponse};
use serde_yaml_ng::{Mapping, Value};

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
    targets: Vec<ProxyDelayTarget>,
    url: String,
    timeout_ms: u64,
) -> Vec<(ProxyDelayTarget, Result<u64, String>)> {
    delay_many_with_progress(api, targets, url, timeout_ms, |_, _| async {}).await
}

/// At most four task-owned requests; dropping this future aborts all of them.
/// Notify progress in completion order while returning results in input order.
pub async fn delay_many_with_progress<F, Fut>(
    api: Arc<MihomoApi>,
    targets: Vec<ProxyDelayTarget>,
    url: String,
    timeout_ms: u64,
    on_result: F,
) -> Vec<(ProxyDelayTarget, Result<u64, String>)>
where
    F: FnMut(ProxyDelayTarget, Result<u64, String>) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let request_api = api.clone();
    let request_url = url.clone();
    delay_many_with_request(
        targets,
        move |target| {
            let api = request_api.clone();
            let url = request_url.clone();
            async move {
                delay_target(&api, &target, &url, timeout_ms)
                    .await
                    .map_err(|error| error.to_string())
            }
        },
        on_result,
    )
    .await
}

/// Testable scheduler core. The request closure owns one request task per
/// target; dropping this future drops/aborts the JoinSet and therefore all
/// outstanding owned requests.
pub async fn delay_many_with_request<Request, RequestFuture, Progress, ProgressFuture>(
    targets: Vec<ProxyDelayTarget>,
    request: Request,
    mut on_result: Progress,
) -> Vec<(ProxyDelayTarget, Result<u64, String>)>
where
    Request: Fn(ProxyDelayTarget) -> RequestFuture + Clone + Send + Sync + 'static,
    RequestFuture: std::future::Future<Output = Result<u64, String>> + Send + 'static,
    Progress: FnMut(ProxyDelayTarget, Result<u64, String>) -> ProgressFuture,
    ProgressFuture: std::future::Future<Output = ()>,
{
    let count = targets.len();
    let mut pending = targets.into_iter().enumerate();
    let mut requests = tokio::task::JoinSet::new();
    let mut identities = HashMap::new();
    let mut results: Vec<Option<(ProxyDelayTarget, Result<u64, String>)>> = (0..count).map(|_| None).collect();
    loop {
        while requests.len() < MAX_DELAY_CONCURRENCY {
            let Some((index, target)) = pending.next() else {
                break;
            };
            let request = request.clone();
            let fallback = target.clone();
            let task = requests.spawn(async move {
                let result = request(target.clone()).await;
                (index, target, result)
            });
            identities.insert(task.id(), (index, fallback));
        }
        let Some(completed) = requests.join_next_with_id().await else {
            break;
        };
        let (index, target, result) = match completed {
            Ok((id, result)) => {
                identities.remove(&id);
                result
            }
            Err(error) => {
                let Some((index, target)) = identities.remove(&error.id()) else {
                    continue;
                };
                (index, target, Err(format!("delay task failed: {error}")))
            }
        };
        on_result(target.clone(), result.clone()).await;
        results[index] = Some((target, result));
    }
    results.into_iter().flatten().collect()
}

/// A delay target that could not be tied to a unique core-supported identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RejectedDelayTarget {
    pub name: String,
    pub reason: String,
}

/// Resolved identities are kept separate when their provider provenance is
/// known. Rejected names carry a reason so callers can report skipped tests.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DelayTargetResolution {
    pub targets: Vec<ProxyDelayTarget>,
    pub rejected: Vec<RejectedDelayTarget>,
}

/// Resolve leaf names to native tags or provider/name pairs using the
/// selected effective profile's `proxy-groups.use` and
/// `include-all-providers` declarations.
///
/// With `group_scope`, only that runtime group contributes provider scope. With
/// no group scope, each name is resolved within the runtime groups containing
/// it; a direct CLI node absent from group membership may use any provider
/// explicitly scoped by the effective profile.
pub fn resolve_delay_targets(
    groups: &HashMap<String, ProxyGroup>,
    names: &[String],
    effective_config: &Mapping,
    providers: &ProxyProvidersResponse,
    group_scope: Option<&str>,
) -> DelayTargetResolution {
    let scopes = configured_provider_scopes(effective_config);
    let static_proxies = configured_proxy_names(effective_config);
    let all_provider_names = sorted_provider_names(providers);
    let mut resolution = DelayTargetResolution::default();
    let mut seen_targets = std::collections::HashSet::new();
    let mut seen_issues = std::collections::HashSet::new();

    for name in names {
        let source_groups = match group_scope {
            Some(group) => vec![group.to_owned()],
            None => {
                let mut containing: Vec<_> = groups
                    .iter()
                    .filter(|(_, group)| {
                        group
                            .all
                            .as_ref()
                            .is_some_and(|all| all.iter().any(|member| member == name))
                    })
                    .map(|(group_name, _)| group_name.clone())
                    .collect();
                containing.sort();
                if containing.is_empty() {
                    scopes.keys().cloned().collect()
                } else {
                    containing
                }
            }
        };

        let mut allowed_providers = std::collections::BTreeSet::new();
        for source_group in source_groups {
            let Some(scope) = scopes.get(&source_group) else {
                continue;
            };
            allowed_providers.extend(scope.use_providers.iter().cloned());
            if scope.include_all_providers {
                allowed_providers.extend(all_provider_names.iter().cloned());
            }
        }

        let mut found_provider_entry = false;
        for provider_name in allowed_providers {
            let Some(provider) = providers.providers.get(&provider_name) else {
                continue;
            };
            let count = provider.proxies.iter().filter(|node| node.name == *name).count();
            if count == 0 {
                continue;
            }
            found_provider_entry = true;
            if count > 1 {
                let reason = format!(
                    "provider '{provider_name}' contains {count} entries named '{name}'; its scoped API cannot select one"
                );
                push_rejection(&mut resolution, &mut seen_issues, name, reason);
                continue;
            }
            let target = provider_target(&provider_name, name);
            if seen_targets.insert(target.key.clone()) {
                resolution.targets.push(target);
            }
        }

        if !found_provider_entry {
            if static_proxies.contains(name) {
                let target = native_target(name);
                if seen_targets.insert(target.key.clone()) {
                    resolution.targets.push(target);
                }
            } else {
                push_rejection(
                    &mut resolution,
                    &mut seen_issues,
                    name,
                    "no matching provider in the effective profile scope and no native proxy declaration proves this target".into(),
                );
            }
        }
    }
    resolution
}

/// Map a displayed `(group,node)` pair to its group-scoped provider identities.
/// The UI row key is display-oriented; delay/reapply operations must retain
/// each returned provider identity and never infer provenance from the label.
pub fn display_target_keys(
    groups: &HashMap<String, ProxyGroup>,
    effective_config: &Mapping,
    providers: &ProxyProvidersResponse,
) -> HashMap<(String, String), Vec<ProxyDelayTarget>> {
    let mut keys = HashMap::new();
    let mut group_names: Vec<_> = groups.keys().collect();
    group_names.sort();
    for group_name in group_names {
        let Some(group) = groups.get(group_name) else {
            continue;
        };
        let Some(members) = &group.all else {
            continue;
        };
        for name in members {
            if POLICY_PSEUDO_NODES.contains(&name.as_str()) || is_group(groups, name) {
                continue;
            }
            let resolved = resolve_delay_targets(
                groups,
                std::slice::from_ref(name),
                effective_config,
                providers,
                Some(group_name),
            );
            if !resolved.targets.is_empty() {
                keys.insert((group_name.clone(), name.clone()), resolved.targets);
            }
        }
    }
    keys
}

/// Whether the effective profile asks mihomo to include proxy-provider nodes.
pub fn has_proxy_provider_scope(effective_config: &Mapping) -> bool {
    configured_provider_scopes(effective_config)
        .values()
        .any(|scope| scope.include_all_providers || !scope.use_providers.is_empty())
}

/// Create the one result key used by the TUI and CLI for a native proxy/tag.
pub fn native_target(name: &str) -> ProxyDelayTarget {
    ProxyDelayTarget {
        key: format!("n{}:{name}", name.len()),
        label: name.to_owned(),
        provider: None,
        name: name.to_owned(),
    }
}

/// Create a collision-safe key for a provider-scoped node identity.
pub fn provider_target(provider: &str, name: &str) -> ProxyDelayTarget {
    ProxyDelayTarget {
        key: format!("p{}:{provider}n{}:{name}", provider.len(), name.len()),
        label: format!("{provider}/{name}"),
        provider: Some(provider.to_owned()),
        name: name.to_owned(),
    }
}

/// Stable identity for a rejected display name, used only to surface a
/// per-target diagnostic without pretending it is a tested native node.
pub fn rejected_target(name: &str) -> ProxyDelayTarget {
    ProxyDelayTarget {
        key: format!("r{}:{name}", name.len()),
        label: name.to_owned(),
        provider: None,
        name: name.to_owned(),
    }
}

/// Verify that a refreshed provider registry still represents the saved
/// delay target. Display names alone are insufficient because the same exit
/// label may move between providers after a subscription refresh.
pub fn contains_target_identity(target: &ProxyDelayTarget, candidates: &[ProxyDelayTarget]) -> bool {
    candidates.iter().any(|candidate| candidate.key == target.key)
}

async fn delay_target(api: &MihomoApi, target: &ProxyDelayTarget, url: &str, timeout_ms: u64) -> anyhow::Result<u64> {
    match target.provider.as_deref() {
        Some(provider) => Ok(api
            .provider_proxy_delay_test(provider, &target.name, url, timeout_ms)
            .await?
            .delay),
        None => Ok(api.delay_test(&target.name, url, timeout_ms).await?.delay),
    }
}

#[derive(Debug, Clone, Default)]
struct ProviderScope {
    use_providers: Vec<String>,
    include_all_providers: bool,
}

fn configured_provider_scopes(config: &Mapping) -> HashMap<String, ProviderScope> {
    let Some(groups) = config.get("proxy-groups").and_then(Value::as_sequence) else {
        return HashMap::new();
    };
    groups
        .iter()
        .filter_map(Value::as_mapping)
        .filter_map(|group| {
            let name = group.get("name")?.as_str()?.to_owned();
            let use_providers = group
                .get("use")
                .and_then(Value::as_sequence)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect();
            let include_all_providers = group
                .get("include-all-providers")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            Some((
                name,
                ProviderScope {
                    use_providers,
                    include_all_providers,
                },
            ))
        })
        .collect()
}

fn configured_proxy_names(config: &Mapping) -> std::collections::HashSet<String> {
    config
        .get("proxies")
        .and_then(Value::as_sequence)
        .into_iter()
        .flatten()
        .filter_map(Value::as_mapping)
        .filter_map(|proxy| proxy.get("name").and_then(Value::as_str))
        .map(str::to_owned)
        .collect()
}

fn sorted_provider_names(providers: &ProxyProvidersResponse) -> Vec<String> {
    let mut names: Vec<_> = providers.providers.keys().cloned().collect();
    names.sort();
    names
}

fn push_rejection(
    resolution: &mut DelayTargetResolution,
    seen: &mut std::collections::HashSet<(String, String)>,
    name: &str,
    reason: String,
) {
    if seen.insert((name.to_owned(), reason.clone())) {
        resolution.rejected.push(RejectedDelayTarget {
            name: name.to_owned(),
            reason,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

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

    fn profile_config(yaml: &str) -> Mapping {
        serde_yaml_ng::from_str(yaml).expect("valid effective-profile fixture")
    }

    fn provider(names: &[&str]) -> crate::mihomo_api::types::ProxyProvider {
        crate::mihomo_api::types::ProxyProvider {
            proxies: names
                .iter()
                .map(|name| crate::mihomo_api::types::ProviderProxyNode { name: (*name).into() })
                .collect(),
        }
    }

    fn providers() -> ProxyProvidersResponse {
        ProxyProvidersResponse {
            providers: HashMap::from([
                ("Alpha".into(), provider(&["Tokyo", "Osaka"])),
                ("Beta".into(), provider(&["Tokyo"])),
                ("Broken".into(), provider(&["Duplicate", "Duplicate"])),
            ]),
        }
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

    #[test]
    fn scoped_provider_membership_qualifies_names_and_keeps_other_sources_out() {
        let runtime = HashMap::from([("Auto".into(), group(Some(&["Tokyo", "Osaka"])))]);
        let config =
            profile_config("proxies: [{name: Osaka, type: direct}]\nproxy-groups:\n  - name: Auto\n    use: [Alpha]\n");
        let requested = vec!["Tokyo".into(), "Osaka".into()];
        let resolved = resolve_delay_targets(&runtime, &requested, &config, &providers(), None);
        assert!(resolved.rejected.is_empty());
        assert_eq!(
            resolved.targets,
            [provider_target("Alpha", "Tokyo"), provider_target("Alpha", "Osaka")]
        );
        let just_alpha = resolve_delay_targets(&runtime, &requested, &config, &providers(), Some("Auto"));
        assert_eq!(just_alpha.targets, resolved.targets);
    }

    #[test]
    fn same_display_name_from_distinct_scoped_providers_keeps_provenance() {
        let runtime = HashMap::from([
            ("From alpha".into(), group(Some(&["Tokyo"]))),
            ("From beta".into(), group(Some(&["Tokyo"]))),
        ]);
        let config = profile_config(
            "proxy-groups:\n  - name: From alpha\n    use: [Alpha]\n  - name: From beta\n    use: [Beta]\n",
        );
        let resolved = resolve_delay_targets(&runtime, &["Tokyo".into()], &config, &providers(), None);
        assert!(resolved.rejected.is_empty());
        assert_eq!(
            resolved.targets,
            [provider_target("Alpha", "Tokyo"), provider_target("Beta", "Tokyo")]
        );
    }

    #[test]
    fn duplicate_entries_inside_one_provider_are_rejected_without_plain_name_fallback() {
        let runtime = HashMap::from([("Auto".into(), group(Some(&["Duplicate"])))]);
        let config = profile_config(
            "proxies: [{name: Duplicate, type: direct}]\nproxy-groups:\n  - name: Auto\n    use: [Broken]\n",
        );
        let resolved = resolve_delay_targets(&runtime, &["Duplicate".into()], &config, &providers(), None);
        assert!(resolved.targets.is_empty());
        assert_eq!(resolved.rejected.len(), 1);
        assert!(resolved.rejected[0].reason.contains("provider 'Broken'"));
    }

    #[test]
    fn include_all_providers_is_limited_to_runtime_group_members() {
        let runtime = HashMap::from([("Auto".into(), group(Some(&["Tokyo", "not-static"])))]);
        let config = profile_config("proxy-groups:\n  - name: Auto\n    include-all-providers: true\n");
        let names = vec!["Tokyo".into(), "not-static".into()];
        let resolved = resolve_delay_targets(&runtime, &names, &config, &providers(), Some("Auto"));
        assert_eq!(
            resolved.targets,
            [provider_target("Alpha", "Tokyo"), provider_target("Beta", "Tokyo")]
        );
        assert_eq!(resolved.rejected.len(), 1);
        assert_eq!(resolved.rejected[0].name, "not-static");
    }

    #[test]
    fn result_keys_are_unambiguous_for_delimiters_and_unicode() {
        let first = provider_target("a::b", "c");
        let second = provider_target("a", "b::c");
        assert_ne!(first.key, second.key);
        assert_eq!(native_target("东京").key, "n6:东京");
    }

    #[test]
    fn saved_provider_identity_must_survive_refresh() {
        let saved = provider_target("Alpha", "Tokyo");
        assert!(contains_target_identity(&saved, &[provider_target("Alpha", "Tokyo")]));
        assert!(!contains_target_identity(&saved, &[provider_target("Beta", "Tokyo")]));
    }

    fn target(name: &str) -> ProxyDelayTarget {
        native_target(name)
    }

    #[tokio::test]
    async fn delay_request_scheduler_caps_concurrency_and_preserves_input_order() {
        let targets = (0..9).map(|index| target(&format!("node-{index}"))).collect::<Vec<_>>();
        let current = Arc::new(AtomicUsize::new(0));
        let maximum = Arc::new(AtomicUsize::new(0));
        let (started_tx, mut started_rx) = tokio::sync::mpsc::channel(9);
        let (progress_tx, mut progress_rx) = tokio::sync::mpsc::channel(9);
        let mut releases = Vec::new();
        let mut gates = HashMap::new();
        for target in &targets {
            let (release, gate) = tokio::sync::oneshot::channel();
            releases.push(Some(release));
            gates.insert(target.name.clone(), gate);
        }
        let gates = Arc::new(std::sync::Mutex::new(gates));
        let task = tokio::spawn({
            let current = current.clone();
            let maximum = maximum.clone();
            let targets = targets.clone();
            async move {
                delay_many_with_request(
                    targets,
                    move |target| {
                        let gate = gates.lock().unwrap().remove(&target.name).unwrap();
                        let current = current.clone();
                        let maximum = maximum.clone();
                        let started = started_tx.clone();
                        async move {
                            let active = current.fetch_add(1, Ordering::SeqCst) + 1;
                            maximum.fetch_max(active, Ordering::SeqCst);
                            started.send(target.name.clone()).await.unwrap();
                            gate.await.unwrap();
                            current.fetch_sub(1, Ordering::SeqCst);
                            Ok(1)
                        }
                    },
                    move |target, result| {
                        let progress = progress_tx.clone();
                        async move {
                            progress.send((target.name, result)).await.unwrap();
                        }
                    },
                )
                .await
            }
        });
        let mut initial = Vec::new();
        for _ in 0..MAX_DELAY_CONCURRENCY {
            initial.push(started_rx.recv().await.unwrap());
        }
        initial.sort();
        assert_eq!(initial, ["node-0", "node-1", "node-2", "node-3"]);
        assert!(started_rx.try_recv().is_err());
        for (step, index) in [3, 2, 1, 0, 4, 5, 6, 7, 8].into_iter().enumerate() {
            releases[index].take().unwrap().send(()).unwrap();
            let (name, result) = progress_rx.recv().await.unwrap();
            assert_eq!(name, format!("node-{index}"));
            assert_eq!(result, Ok(1));
            if step < 5 {
                assert_eq!(started_rx.recv().await.unwrap(), format!("node-{}", step + 4));
            }
        }
        let results = task.await.unwrap();
        assert_eq!(maximum.load(Ordering::SeqCst), MAX_DELAY_CONCURRENCY);
        assert_eq!(current.load(Ordering::SeqCst), 0);
        assert_eq!(
            results.iter().map(|(target, _)| &target.key).collect::<Vec<_>>(),
            targets.iter().map(|target| &target.key).collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn dropping_scheduler_aborts_owned_requests() {
        struct DropProbe(tokio::sync::mpsc::UnboundedSender<()>);
        impl Drop for DropProbe {
            fn drop(&mut self) {
                let _ = self.0.send(());
            }
        }
        let (dropped_tx, mut dropped_rx) = tokio::sync::mpsc::unbounded_channel();
        let (started_tx, mut started_rx) = tokio::sync::mpsc::channel(MAX_DELAY_CONCURRENCY);
        let targets = (0..MAX_DELAY_CONCURRENCY)
            .map(|index| target(&format!("node-{index}")))
            .collect();
        let task = tokio::spawn(async move {
            delay_many_with_request(
                targets,
                move |_target| {
                    let guard = DropProbe(dropped_tx.clone());
                    let started = started_tx.clone();
                    async move {
                        let _guard = guard;
                        started.send(()).await.unwrap();
                        std::future::pending::<()>().await;
                        Ok(0)
                    }
                },
                |_, _| async {},
            )
            .await;
        });
        for _ in 0..MAX_DELAY_CONCURRENCY {
            started_rx.recv().await.unwrap();
        }
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        for _ in 0..MAX_DELAY_CONCURRENCY {
            dropped_rx.recv().await.unwrap();
        }
        assert!(dropped_rx.recv().await.is_none());
    }
}
