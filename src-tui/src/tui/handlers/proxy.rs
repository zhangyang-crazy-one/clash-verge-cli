//! Proxies view: groups and node selection, delay tests, proxy chains, and
//! the clash mode.

use serde_yaml_ng::Value;

use crate::app::{Action, App, CoreState, ProxyDisplayRow, first_selectable_proxy_group, proxy_display_rows};
use crate::runtime_config::commit_runtime_config;
use crate::services::mode::{apply_clash_mode, next_clash_mode};
use crate::services::proxy::{DELAY_TEST_TIMEOUT_MS, DELAY_TEST_URL, MAX_DELAY_CONCURRENCY};

use super::Ctx;

pub(super) fn refresh(app: &mut App, ctx: &Ctx) {
    app.runtime_loading.proxies = true;
    app.runtime_errors.proxies = None;
    let api = ctx.manager.api();
    ctx.spawn_result(
        async move { api.get_proxies().await },
        |data| Action::ProxiesFetched(data.proxies),
        |error| Action::ProxiesFailed(error.to_string()),
    );
}

pub(super) fn note_fetched(
    app: &mut App,
    groups: std::collections::HashMap<String, crate::mihomo_api::types::ProxyGroup>,
) {
    app.runtime_loading.proxies = false;
    app.runtime_errors.proxies = None;
    app.proxy_groups = groups;
    let expanded_is_available = app
        .expanded_proxy_group
        .as_deref()
        .and_then(|name| app.proxy_groups.get(name))
        .and_then(|group| group.all.as_ref())
        .is_some_and(|nodes| !nodes.is_empty());
    if !expanded_is_available {
        app.expanded_proxy_group = first_selectable_proxy_group(&app.proxy_groups);
        app.node_selected_index = 0;
    }
    let rows = proxy_display_rows(&app.proxy_groups, app.expanded_proxy_group.as_deref());
    app.node_selected_index = app.node_selected_index.min(rows.len().saturating_sub(1));
    let group_count = rows
        .iter()
        .filter(|row| matches!(row, ProxyDisplayRow::Group { .. }))
        .count();
    let choice_count: usize = app
        .proxy_groups
        .values()
        .filter_map(|group| group.all.as_ref().filter(|nodes| !nodes.is_empty()))
        .map(Vec::len)
        .sum();
    app.status_msg = Some(format!(
        "{group_count} selectable groups, {choice_count} choices loaded"
    ));
}

/// `Enter` on Proxies: expand a group, add a node to the chain being edited,
/// or select the node in its group.
pub(super) fn activate_selected(app: &mut App, ctx: &Ctx) {
    let selected_row = proxy_display_rows(&app.proxy_groups, app.expanded_proxy_group.as_deref())
        .get(app.node_selected_index)
        .cloned();
    if let Some(ProxyDisplayRow::Group { name, node_count, .. }) = selected_row {
        app.node_selected_index = proxy_display_rows(&app.proxy_groups, Some(&name))
            .iter()
            .position(|row| matches!(row, ProxyDisplayRow::Group { name: row_name, .. } if row_name == &name))
            .unwrap_or_default();
        app.status_msg = Some(format!("Browsing {name}: {node_count} choices"));
        app.expanded_proxy_group = Some(name);
        return;
    }
    let Some((group, name)) = find_node_at_index(
        &app.proxy_groups,
        app.expanded_proxy_group.as_deref(),
        app.node_selected_index,
    ) else {
        return;
    };
    if app.chain_mode {
        if !app.chain_nodes.contains(&name) {
            app.chain_nodes.push(name);
            app.status_msg = Some(format!("Chain: {}", app.chain_nodes.join(" → ")));
        }
        return;
    }
    app.status_msg = Some(format!("Switching to {name}..."));
    let api = ctx.manager.api();
    ctx.spawn_result(
        async move { api.select_proxy(&group, &name).await },
        |()| Action::ProxiesRefresh,
        |error| Action::ProxiesFailed(error.to_string()),
    );
}

/// `t`: delay-test the selected node.
pub(super) fn test_selected_delay(app: &mut App, ctx: &Ctx) {
    let Some((_, name)) = find_node_at_index(
        &app.proxy_groups,
        app.expanded_proxy_group.as_deref(),
        app.node_selected_index,
    ) else {
        return;
    };
    app.status_msg = Some(format!("Testing delay for {name}..."));
    let api = ctx.manager.api();
    ctx.spawn(|tx| async move {
        let _ = tx.send(
            match api.delay_test(&name, DELAY_TEST_URL, DELAY_TEST_TIMEOUT_MS).await {
                Ok(d) => Action::DelayResult(name, Some(d.delay)),
                Err(error) => Action::DelayFailed(name, error.to_string()),
            },
        );
    });
}

/// `T`: delay-test every real node, at most [`MAX_DELAY_CONCURRENCY`] at a
/// time.
pub(super) fn test_all_delays(app: &mut App, ctx: &Ctx) {
    match begin_batch_delay(app) {
        BatchDelayOutcome::Started { targets } => {
            app.status_msg = Some(format!("{}: 0/{}", app.tr("proxies.batch_delay"), targets.len()));
            let api = std::sync::Arc::new(ctx.manager.api());
            ctx.spawn(|tx| async move {
                let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(MAX_DELAY_CONCURRENCY));
                let mut handles = Vec::new();
                for name in targets {
                    let permit = match semaphore.clone().acquire_owned().await {
                        Ok(permit) => permit,
                        // Semaphore closed: stop scheduling further requests.
                        Err(_) => break,
                    };
                    let api = api.clone();
                    let tx = tx.clone();
                    handles.push(tokio::spawn(async move {
                        let _permit = permit;
                        let _ = tx.send(
                            match api.delay_test(&name, DELAY_TEST_URL, DELAY_TEST_TIMEOUT_MS).await {
                                Ok(d) => Action::BatchDelayResult(name, Some(d.delay)),
                                Err(error) => Action::BatchDelayFailed(name, error.to_string()),
                            },
                        );
                    }));
                }
                for handle in handles {
                    let _ = handle.await;
                }
            });
        }
        BatchDelayOutcome::InProgress { done, total } => {
            app.status_msg = Some(format!("{}: {done}/{total}", app.tr("proxies.batch_delay")));
        }
        BatchDelayOutcome::NoTargets => {
            app.status_msg = Some(app.tr("proxies.no_testable").into());
        }
    }
}

/// `c`: toggle chain editing (always starting from an empty chain).
pub(super) fn toggle_chain_mode(app: &mut App) {
    app.chain_mode = !app.chain_mode;
    app.chain_nodes.clear();
    app.status_msg = Some(if app.chain_mode {
        "Chain edit ON: Enter add, a apply, x clear".into()
    } else {
        "Chain OFF".into()
    });
}

/// `a`: apply the edited chain to the runtime config.
pub(super) fn apply_chain(app: &mut App, ctx: &Ctx) {
    if app.chain_nodes.len() < 2 {
        app.status_msg = Some("Need >=2 nodes for chain".into());
        return;
    }
    let nodes = app.chain_nodes.clone();
    let enable_tun = app.gui_config.enable_tun_mode.unwrap_or(false);
    let api = ctx.manager.api();
    app.status_msg = Some("Applying chain...".into());
    ctx.spawn(|tx| async move {
        match apply_chain_config(&api, &nodes, enable_tun).await {
            Ok(_) => {
                let _ = tx.send(Action::ChainApplied(nodes));
                let _ = tx.send(Action::ProxiesRefresh);
            }
            Err(error) => {
                let _ = tx.send(Action::ChainFailed(error));
            }
        }
    });
}

/// `x`: drop the edited chain.
pub(super) fn clear_chain(app: &mut App) {
    app.chain_nodes.clear();
    app.status_msg = Some("Chain cleared".into());
}

pub(super) fn note_chain_applied(app: &mut App, nodes: Vec<String>) {
    app.chain_mode = false;
    app.chain_nodes.clear();
    app.status_msg = Some(format!("Chain applied: {}", nodes.join(" -> ")));
}

/// `m`: rule → global → direct → rule.
pub(super) fn cycle_clash_mode(app: &App, ctx: &Ctx) {
    let next = next_clash_mode(&app.clash_mode);
    let api = ctx.manager.api();
    let core_running = app.core_state == CoreState::Running;
    ctx.spawn_result(
        async move { apply_clash_mode(&api, next, core_running).await },
        |mode| Action::ModeChanged { mode, announce: true },
        Action::ModeChangeFailed,
    );
}

pub(super) async fn note_mode_changed(app: &mut App, mode: String, announce: bool) {
    app.core_config = clash_verge_core::config::IClashTemp::new().await;
    if announce {
        app.status_msg = Some(format!("{}: {mode}", app.tr("settings.mode_set")));
    }
    app.clash_mode = mode;
}

pub(super) async fn apply_chain_config(
    api: &crate::mihomo_api::MihomoApi,
    chain_nodes: &[String],
    enable_tun: bool,
) -> Result<std::path::PathBuf, String> {
    // Sidecar backup for diagnostics; the commit path also keeps an in-memory rollback copy.
    let path = clash_verge_core::utils::dirs::clash_path().map_err(|error| error.to_string())?;
    if path.exists() {
        let original = tokio::fs::read(&path)
            .await
            .map_err(|error| format!("failed to back up {}: {error}", path.display()))?;
        let backup_path = path.with_extension("yaml.tui-chain-backup");
        tokio::fs::write(&backup_path, &original)
            .await
            .map_err(|error| format!("failed to write {}: {error}", backup_path.display()))?;
    }

    commit_runtime_config(api, enable_tun, true, None, |mut config| {
        let entries = config
            .get("proxies")
            .and_then(Value::as_sequence)
            .ok_or_else(|| "active config has no proxies list".to_string())?;
        let mut proxies = entries
            .iter()
            .enumerate()
            .map(|(index, entry)| {
                entry
                    .as_mapping()
                    .cloned()
                    .ok_or_else(|| format!("proxies[{index}] is not a mapping"))
            })
            .collect::<Result<Vec<_>, _>>()?;

        crate::chain::build_chain_config(chain_nodes, &mut proxies).map_err(|error| error.to_string())?;
        config.insert(
            "proxies".into(),
            Value::Sequence(proxies.into_iter().map(Value::Mapping).collect()),
        );
        Ok(config)
    })
    .await
}

/// Find the (group_name, node_name) at a flat index in proxy groups.
pub(super) fn find_node_at_index(
    groups: &std::collections::HashMap<String, crate::mihomo_api::types::ProxyGroup>,
    expanded_group: Option<&str>,
    target: usize,
) -> Option<(String, String)> {
    proxy_display_rows(groups, expanded_group)
        .get(target)
        .and_then(|row| row.node_identity())
        .map(|(group, node)| (group.to_string(), node.to_string()))
}

/// Collect the deduplicated set of real leaf proxy targets for a batch delay
/// test. A name is a real leaf only if it is not a policy pseudo-node and it
/// is not itself a proxy group (a group is a key whose `all` is present).
/// The result is sorted for a stable, deterministic test order.
pub(super) fn batch_delay_targets(
    groups: &std::collections::HashMap<String, crate::mihomo_api::types::ProxyGroup>,
) -> Vec<String> {
    crate::services::proxy::leaf_targets(groups, None)
}

/// Outcome of deciding what to do when the user presses the batch-delay shortcut.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum BatchDelayOutcome {
    /// No batch is running; `targets` are the freshly computed leaf targets.
    Started { targets: Vec<String> },
    /// A batch is already running; report its current progress instead of
    /// scheduling another one.
    InProgress { done: usize, total: usize },
    /// The testable set is empty; do not create any delay request.
    NoTargets,
}

/// Decide what the batch-delay shortcut does with the current app state.
/// Guarded so a second invocation never schedules a second batch.
pub(super) fn begin_batch_delay(app: &mut App) -> BatchDelayOutcome {
    if let Some((done, total)) = app.batch_delay {
        return BatchDelayOutcome::InProgress { done, total };
    }
    let targets = batch_delay_targets(&app.proxy_groups);
    if targets.is_empty() {
        return BatchDelayOutcome::NoTargets;
    }
    app.batch_delay = Some((0, targets.len()));
    BatchDelayOutcome::Started { targets }
}

/// Count one finished batch result. Only batch result events call this, so the
/// batch never blocks on any individual node; when the last result arrives the
/// in-progress marker (and duplicate-start guard) is cleared.
pub(super) fn advance_batch(app: &mut App) {
    let Some((done, total)) = app.batch_delay else {
        return;
    };
    let next = done + 1;
    if next >= total {
        app.batch_delay = None;
    } else {
        app.batch_delay = Some((next, total));
    }
    app.status_msg = Some(format!("{}: {next}/{total}", app.tr("proxies.batch_delay")));
}

/// Record one single-node delay result in the shared delay map and status bar.
/// Never touches batch progress: a single-node `t` result must not advance or
/// clear the active batch.
pub(super) fn note_delay_result(app: &mut App, name: String, delay: Option<u64>) {
    app.delay_map.insert(name, delay);
    if let Some(delay) = delay {
        app.status_msg = Some(format!("Delay: {delay}ms"));
    }
}

/// Record one single-node delay failure. Same contract as [`note_delay_result`].
pub(super) fn note_delay_failed(app: &mut App, name: String, error: String) {
    app.delay_map.insert(name.clone(), None);
    app.status_msg = Some(format!("Delay failed for {name}: {error}"));
}

/// Record one batch delay result: identical per-node rendering to the
/// single-node path, then advance the active batch progress.
pub(super) fn note_batch_delay_result(app: &mut App, name: String, delay: Option<u64>) {
    note_delay_result(app, name, delay);
    advance_batch(app);
}

/// Record one batch delay failure: identical per-node rendering to the
/// single-node path, then advance the active batch progress.
pub(super) fn note_batch_delay_failed(app: &mut App, name: String, error: String) {
    note_delay_failed(app, name, error);
    advance_batch(app);
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn proxy_group(all: Option<Vec<String>>) -> crate::mihomo_api::types::ProxyGroup {
        crate::mihomo_api::types::ProxyGroup {
            group_type: "Selector".to_string(),
            now: None,
            all,
            history: None,
        }
    }

    #[test]
    fn batch_delay_targets_deduplicates_leaf_nodes_in_stable_order() {
        let mut groups = HashMap::new();
        groups.insert(
            "first".to_string(),
            proxy_group(Some(vec!["Tokyo".to_string(), "Singapore".to_string()])),
        );
        groups.insert(
            "second".to_string(),
            proxy_group(Some(vec!["Tokyo".to_string(), "Los Angeles".to_string()])),
        );

        assert_eq!(batch_delay_targets(&groups), vec!["Los Angeles", "Singapore", "Tokyo"]);
    }

    #[test]
    fn batch_delay_targets_excludes_pseudo_nodes_and_nested_group_names() {
        let mut groups = HashMap::new();
        // "nested" is itself a group key, so it must never be a target.
        groups.insert("nested".to_string(), proxy_group(Some(vec!["Tokyo".to_string()])));
        groups.insert(
            "root".to_string(),
            proxy_group(Some(vec![
                "Tokyo".to_string(),
                "nested".to_string(),
                "DIRECT".to_string(),
                "REJECT".to_string(),
                "REJECT-DROP".to_string(),
                "PASS".to_string(),
                "COMPATIBLE".to_string(),
            ])),
        );
        // A leaf node that is also a key with `all: None` stays testable.
        groups.insert("Tokyo".to_string(), proxy_group(None));

        assert_eq!(batch_delay_targets(&groups), vec!["Tokyo"]);
    }

    #[test]
    fn batch_delay_targets_is_empty_when_nothing_is_testable() {
        let mut groups = HashMap::new();
        groups.insert("DIRECT".to_string(), proxy_group(Some(Vec::new())));
        groups.insert("only-group".to_string(), proxy_group(Some(vec!["DIRECT".to_string()])));

        assert!(batch_delay_targets(&groups).is_empty());
    }

    #[test]
    fn begin_batch_delay_rejects_duplicate_starts_and_reports_progress() {
        let mut app = App::new();
        app.batch_delay = Some((2, 7));

        match begin_batch_delay(&mut app) {
            BatchDelayOutcome::InProgress { done, total } => {
                assert_eq!((done, total), (2, 7));
            }
            outcome => panic!("expected InProgress, got {outcome:?}"),
        }
        assert_eq!(app.batch_delay, Some((2, 7)), "in-flight state must stay untouched");
    }

    #[test]
    fn begin_batch_delay_reports_no_targets_without_creating_a_task() {
        let mut app = App::new();

        assert_eq!(begin_batch_delay(&mut app), BatchDelayOutcome::NoTargets);
        assert_eq!(app.batch_delay, None);
    }

    #[test]
    fn begin_batch_delay_starts_a_batch_with_filtered_targets() {
        let mut app = App::new();
        app.proxy_groups.insert(
            "root".to_string(),
            proxy_group(Some(vec!["DIRECT".to_string(), "Tokyo".to_string()])),
        );
        app.proxy_groups.insert("Tokyo".to_string(), proxy_group(None));

        match begin_batch_delay(&mut app) {
            BatchDelayOutcome::Started { targets } => {
                assert_eq!(targets, vec!["Tokyo"]);
            }
            outcome => panic!("expected Started, got {outcome:?}"),
        }
        assert_eq!(app.batch_delay, Some((0, 1)));
    }

    #[test]
    fn advance_batch_counts_results_and_clears_on_completion() {
        let mut app = App::new();
        app.batch_delay = Some((0, 3));

        advance_batch(&mut app);
        assert_eq!(app.batch_delay, Some((1, 3)));

        advance_batch(&mut app);
        assert_eq!(app.batch_delay, Some((2, 3)));

        advance_batch(&mut app);
        assert_eq!(app.batch_delay, None, "last result clears the in-progress marker");
    }

    #[test]
    fn single_node_result_during_batch_does_not_advance_or_clear_the_guard() {
        let mut app = App::new();
        app.batch_delay = Some((1, 5));
        app.proxy_groups
            .insert("root".to_string(), proxy_group(Some(vec!["Tokyo".to_string()])));
        app.proxy_groups.insert("Tokyo".to_string(), proxy_group(None));

        // A single-node `t` result lands while the batch is still running.
        note_delay_result(&mut app, "Tokyo".to_string(), Some(42));
        assert_eq!(
            app.delay_map.get("Tokyo"),
            Some(&Some(42)),
            "single-node result still renders"
        );
        assert_eq!(
            app.batch_delay,
            Some((1, 5)),
            "single-node result must not advance batch progress"
        );
        assert_eq!(
            begin_batch_delay(&mut app),
            BatchDelayOutcome::InProgress { done: 1, total: 5 },
            "the batch guard must stay armed so a second batch cannot start early"
        );

        // The batch's own result still advances progress.
        note_batch_delay_result(&mut app, "Tokyo".to_string(), Some(43));
        assert_eq!(app.batch_delay, Some((2, 5)));
    }

    #[test]
    fn single_node_failure_during_batch_does_not_advance_or_clear_the_guard() {
        let mut app = App::new();
        app.batch_delay = Some((3, 4));

        note_delay_failed(&mut app, "Tokyo".to_string(), "timeout".to_string());
        assert_eq!(
            app.delay_map.get("Tokyo"),
            Some(&None),
            "failure state still renders as failed"
        );
        assert_eq!(
            app.batch_delay,
            Some((3, 4)),
            "single-node failure must not advance batch progress"
        );

        // The batch's own failure completes the batch and clears the guard.
        note_batch_delay_failed(&mut app, "Singapore".to_string(), "timeout".to_string());
        assert_eq!(app.batch_delay, None, "batch failure on the last node clears the guard");
    }
}
