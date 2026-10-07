//! Injectable core operations used by subscription scheduling.
//!
//! Production code adapts the existing manager; tests can provide a fake
//! lifecycle without opening a controller or starting a core.

use async_trait::async_trait;

use crate::mihomo_api::error::MihomoError;
use crate::mihomo_api::types::ProxyDelay;
use crate::mihomo_manager::{CoreKind, MihomoManager};

#[async_trait]
pub(crate) trait CoreLifecycle: Send + Sync {
    fn core_kind(&self) -> CoreKind;
    fn is_running(&self) -> bool;
    fn owns_core(&self) -> bool;
    async fn current_exit_node(&self, items: &[clash_verge_core::config::PrfItem], uid: &str) -> Option<String>;
    async fn delay_test(&self, node: &str, url: &str, timeout_ms: u64) -> Result<ProxyDelay, MihomoError>;
    async fn reload_profile(&self, uid: &str, enable_tun: bool) -> Result<(), String>;
    async fn reapply_selected(&self, node: &str) -> Result<(), String>;
}

pub(crate) struct ManagerLifecycle<'a> {
    manager: &'a MihomoManager,
    selection_path: parking_lot::Mutex<Vec<(String, String)>>,
    delay_target: parking_lot::Mutex<Option<crate::mihomo_api::types::ProxyDelayTarget>>,
}

pub(crate) fn validate_apply_target(kind: CoreKind, running: bool, owns_core: bool) -> Result<(), String> {
    if !running {
        return Ok(());
    }
    if kind == CoreKind::SingBox && !owns_core {
        return Err("selected sing-box core is externally managed; refusing an implicit restart".into());
    }
    Ok(())
}

pub(crate) fn should_rollback_selected_exit(selected_node: &str, refreshed_nodes: &[&str]) -> bool {
    !refreshed_nodes.iter().any(|node| *node == selected_node)
}

/// Shared guard and dispatch seam for refresh callers. A stopped core is
/// allowed to receive persisted profile state, but this function never starts
/// it; an attached external sing-box is likewise never restarted implicitly.
pub(crate) async fn reload_with_lifecycle<C: CoreLifecycle>(
    lifecycle: &C,
    uid: &str,
    enable_tun: bool,
) -> Result<(), String> {
    validate_apply_target(lifecycle.core_kind(), lifecycle.is_running(), lifecycle.owns_core())?;
    if !lifecycle.is_running() {
        return Ok(());
    }
    lifecycle.reload_profile(uid, enable_tun).await
}

/// Readiness is part of reload; selection is applied only after it succeeds.
pub(crate) async fn reload_selected_with_lifecycle<C: CoreLifecycle>(
    lifecycle: &C,
    uid: &str,
    node: &str,
    tun: bool,
) -> Result<(), String> {
    reload_with_lifecycle(lifecycle, uid, tun).await?;
    if lifecycle.is_running() {
        lifecycle.reapply_selected(node).await?;
    }
    Ok(())
}

/// Follow real selectors; sing-box's synthetic GLOBAL fallback is never written.
fn exit_selection_path(
    groups: &std::collections::HashMap<String, crate::mihomo_api::types::ProxyGroup>,
) -> Option<(String, Vec<(String, String)>)> {
    let global = groups.get("GLOBAL")?;
    let mut node = global.now.clone()?;
    let mut path = Vec::new();
    if global.group_type == "Selector" {
        path.push(("GLOBAL".into(), node.clone()));
    }
    let mut visited = std::collections::HashSet::new();
    loop {
        if matches!(
            node.as_str(),
            "DIRECT" | "REJECT" | "REJECT-DROP" | "PASS" | "COMPATIBLE"
        ) {
            return None;
        }
        if !visited.insert(node.clone()) {
            return None;
        }
        let Some(group) = groups.get(&node) else {
            return Some((node, path));
        };
        if group.group_type != "Selector" {
            return Some((node, path));
        }
        let selected = group.now.clone()?;
        path.push((node, selected.clone()));
        node = selected;
    }
}

impl<'a> ManagerLifecycle<'a> {
    pub(crate) fn new(manager: &'a MihomoManager) -> Self {
        Self {
            manager,
            selection_path: parking_lot::Mutex::new(Vec::new()),
            delay_target: parking_lot::Mutex::new(None),
        }
    }
}

#[async_trait]
impl CoreLifecycle for ManagerLifecycle<'_> {
    fn core_kind(&self) -> CoreKind {
        self.manager.core_kind()
    }

    fn is_running(&self) -> bool {
        self.manager.state() == crate::app::CoreState::Running
    }

    fn owns_core(&self) -> bool {
        self.manager.owns_child()
    }

    async fn delay_test(&self, node: &str, url: &str, timeout_ms: u64) -> Result<ProxyDelay, MihomoError> {
        let target = self.delay_target.lock().clone();
        let api = self.manager.api();
        match target.and_then(|target| target.provider) {
            Some(provider) => api.provider_proxy_delay_test(&provider, node, url, timeout_ms).await,
            None => api.delay_test(node, url, timeout_ms).await,
        }
    }

    async fn current_exit_node(&self, items: &[clash_verge_core::config::PrfItem], uid: &str) -> Option<String> {
        let api = self.manager.api();
        let data = api.get_proxies().await.ok()?;
        let (node, path) = exit_selection_path(&data.proxies)?;
        let mut target = crate::services::proxy::native_target(&node);
        if self.core_kind() == CoreKind::Mihomo && !crate::services::proxy::is_group(&data.proxies, &node) {
            let effective = clash_verge_core::config::IClashTemp::try_read().await.ok()?.0;
            if crate::services::proxy::has_proxy_provider_scope(&effective) {
                let providers = api.get_proxy_providers().await.ok()?;
                let group = path.last().map(|(group, _)| group.as_str());
                let resolved = crate::services::proxy::resolve_delay_targets(
                    &data.proxies,
                    std::slice::from_ref(&node),
                    &effective,
                    &providers,
                    group,
                );
                if resolved.targets.len() != 1 || !resolved.rejected.is_empty() {
                    return None;
                }
                target = resolved.targets.into_iter().next()?;
            }
        }
        *self.selection_path.lock() = path;
        *self.delay_target.lock() = Some(target);
        let _ = (items, uid);
        Some(node)
    }

    async fn reload_profile(&self, uid: &str, enable_tun: bool) -> Result<(), String> {
        super::scheduler::reload_current_profile_for_manager(self.manager, uid, enable_tun, true).await
    }

    async fn reapply_selected(&self, node: &str) -> Result<(), String> {
        let path = self.selection_path.lock().clone();
        let saved_target = self.delay_target.lock().clone();
        let api = self.manager.api();
        let data = api.get_proxies().await.map_err(|error| error.to_string())?;
        if let Some(saved_target) = saved_target.filter(|target| target.provider.is_some()) {
            let effective = clash_verge_core::config::IClashTemp::try_read()
                .await
                .map_err(|error| error.to_string())?
                .0;
            let providers = api.get_proxy_providers().await.map_err(|error| error.to_string())?;
            let group = path.last().map(|(group, _)| group.as_str());
            let resolved = crate::services::proxy::resolve_delay_targets(
                &data.proxies,
                &[node.to_owned()],
                &effective,
                &providers,
                group,
            );
            if !crate::services::proxy::contains_target_identity(&saved_target, &resolved.targets) {
                return Err(format!("selected exit {node} changed provider identity after reload"));
            }
        }
        for (group, selected) in path {
            let members = data
                .proxies
                .get(&group)
                .and_then(|group| group.all.as_ref())
                .ok_or_else(|| format!("selected group {group} disappeared after reload"))?;
            let names: Vec<_> = members.iter().map(String::as_str).collect();
            if should_rollback_selected_exit(&selected, &names) {
                return Err(format!("selected exit {node} disappeared from group {group}"));
            }
            api.select_proxy(&group, &selected)
                .await
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::{Arc, Mutex};

    struct FakeLifecycle {
        kind: CoreKind,
        running: bool,
        owns: bool,
        reloads: Arc<Mutex<Vec<String>>>,
        events: Arc<Mutex<Vec<&'static str>>>,
        fail_reload: bool,
        fail_reapply: bool,
    }

    #[async_trait]
    impl CoreLifecycle for FakeLifecycle {
        fn core_kind(&self) -> CoreKind {
            self.kind
        }
        fn is_running(&self) -> bool {
            self.running
        }
        fn owns_core(&self) -> bool {
            self.owns
        }
        async fn current_exit_node(&self, _items: &[clash_verge_core::config::PrfItem], _uid: &str) -> Option<String> {
            Some("JP-1".into())
        }
        async fn delay_test(&self, _node: &str, _url: &str, _timeout_ms: u64) -> Result<ProxyDelay, MihomoError> {
            Ok(ProxyDelay { delay: 1 })
        }
        async fn reload_profile(&self, uid: &str, _enable_tun: bool) -> Result<(), String> {
            self.events.lock().unwrap().push("reload");
            if self.fail_reload {
                return Err("readiness failed".into());
            }
            self.reloads.lock().unwrap().push(uid.into());
            Ok(())
        }
        async fn reapply_selected(&self, _node: &str) -> Result<(), String> {
            self.events.lock().unwrap().push("reapply");
            if self.fail_reapply {
                return Err("selection failed".into());
            }
            Ok(())
        }
    }

    #[test]
    fn stopped_core_never_requires_start_or_restart() {
        assert!(validate_apply_target(CoreKind::Mihomo, false, false).is_ok());
        assert!(validate_apply_target(CoreKind::SingBox, false, false).is_ok());
    }

    #[test]
    fn external_singbox_cannot_be_restarted_by_refresh() {
        let error = validate_apply_target(CoreKind::SingBox, true, false).expect_err("external core must be guarded");
        assert!(error.contains("externally managed"));
        assert!(validate_apply_target(CoreKind::Mihomo, true, false).is_ok());
        assert!(validate_apply_target(CoreKind::SingBox, true, true).is_ok());
    }

    #[test]
    fn selected_exit_rolls_back_when_refresh_loses_it() {
        assert!(should_rollback_selected_exit("JP-1", &["DIRECT", "US-1"]));
        assert!(!should_rollback_selected_exit("JP-1", &["JP-1", "US-1"]));
    }

    #[tokio::test]
    async fn injected_reload_dispatches_both_cores_without_starting_stopped_core() {
        for kind in [CoreKind::Mihomo, CoreKind::SingBox] {
            let reloads = Arc::new(Mutex::new(Vec::new()));
            let fake = FakeLifecycle {
                kind,
                running: true,
                owns: true,
                reloads: reloads.clone(),
                events: Arc::new(Mutex::new(Vec::new())),
                fail_reload: false,
                fail_reapply: false,
            };
            reload_with_lifecycle(&fake, "profile-a", false)
                .await
                .expect("owned core reload");
            assert_eq!(reloads.lock().unwrap().as_slice(), ["profile-a"]);

            let stopped = FakeLifecycle {
                kind,
                running: false,
                owns: false,
                reloads: reloads.clone(),
                events: Arc::new(Mutex::new(Vec::new())),
                fail_reload: false,
                fail_reapply: false,
            };
            reload_with_lifecycle(&stopped, "profile-b", false)
                .await
                .expect("stopped persists only");
            assert_eq!(reloads.lock().unwrap().as_slice(), ["profile-a"]);
        }
    }

    #[tokio::test]
    async fn injected_reload_rejects_external_singbox_before_operation() {
        let reloads = Arc::new(Mutex::new(Vec::new()));
        let fake = FakeLifecycle {
            kind: CoreKind::SingBox,
            running: true,
            owns: false,
            reloads: reloads.clone(),
            events: Arc::new(Mutex::new(Vec::new())),
            fail_reload: false,
            fail_reapply: false,
        };
        let error = reload_with_lifecycle(&fake, "profile-a", false)
            .await
            .expect_err("external guard");
        assert!(error.contains("externally managed"));
        assert!(reloads.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn reload_selected_runs_readiness_before_selection_and_surfaces_failures() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let fake = FakeLifecycle {
            kind: CoreKind::Mihomo,
            running: true,
            owns: true,
            reloads: Arc::new(Mutex::new(Vec::new())),
            events: events.clone(),
            fail_reload: false,
            fail_reapply: false,
        };
        reload_selected_with_lifecycle(&fake, "profile-a", "JP-1", false)
            .await
            .expect("reload and reapply");
        assert_eq!(*events.lock().unwrap(), vec!["reload", "reapply"]);

        let failed_reload = FakeLifecycle {
            kind: CoreKind::SingBox,
            running: true,
            owns: true,
            reloads: Arc::new(Mutex::new(Vec::new())),
            events: Arc::new(Mutex::new(Vec::new())),
            fail_reload: true,
            fail_reapply: false,
        };
        assert!(
            reload_selected_with_lifecycle(&failed_reload, "profile-a", "JP-1", false)
                .await
                .is_err()
        );
        assert_eq!(*failed_reload.events.lock().unwrap(), vec!["reload"]);

        let failed_selection = FakeLifecycle {
            kind: CoreKind::SingBox,
            running: true,
            owns: true,
            reloads: Arc::new(Mutex::new(Vec::new())),
            events: Arc::new(Mutex::new(Vec::new())),
            fail_reload: false,
            fail_reapply: true,
        };
        assert!(
            reload_selected_with_lifecycle(&failed_selection, "profile-a", "JP-1", false)
                .await
                .is_err()
        );
        assert_eq!(*failed_selection.events.lock().unwrap(), vec!["reload", "reapply"]);
    }

    #[tokio::test]
    async fn reload_selected_does_nothing_for_stopped_or_external_core() {
        for (kind, owns) in [(CoreKind::Mihomo, false), (CoreKind::SingBox, false)] {
            let events = Arc::new(Mutex::new(Vec::new()));
            let fake = FakeLifecycle {
                kind,
                running: false,
                owns,
                reloads: Arc::new(Mutex::new(Vec::new())),
                events: events.clone(),
                fail_reload: true,
                fail_reapply: true,
            };
            reload_selected_with_lifecycle(&fake, "profile-a", "JP-1", false)
                .await
                .expect("stopped persist only");
            assert!(events.lock().unwrap().is_empty());
        }
        let events = Arc::new(Mutex::new(Vec::new()));
        let fake = FakeLifecycle {
            kind: CoreKind::SingBox,
            running: true,
            owns: false,
            reloads: Arc::new(Mutex::new(Vec::new())),
            events: events.clone(),
            fail_reload: false,
            fail_reapply: false,
        };
        assert!(
            reload_selected_with_lifecycle(&fake, "profile-a", "JP-1", false)
                .await
                .is_err()
        );
        assert!(events.lock().unwrap().is_empty());
    }
}
