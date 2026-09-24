//! Profiles: resolving a user's profile argument and switching the current
//! profile.

use clash_verge_core::config::PrfItem;

use crate::mihomo_api::MihomoApi;
use crate::mihomo_manager::{CoreKind, MihomoManager};
use crate::profile_store::store::ProfileStore;
use crate::runtime_config::{commit_runtime_config, reload_remote_profile};

/// Strategy `switch_profile_for_core` uses to push `item` to the running
/// core. Mirrors the scheduler's `RefreshPath`: kept here so the
/// switch/profile flow has its own dispatch surface (no shared enum to
/// avoid coupling two unrelated modules). Inlined into the helper.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SwitchPath {
    HotReload,
    SingboxRestart,
}

fn decide_switch_path(kind: CoreKind) -> SwitchPath {
    match kind {
        CoreKind::Mihomo => SwitchPath::HotReload,
        CoreKind::SingBox => SwitchPath::SingboxRestart,
    }
}

/// Find the profile `query` names: an exact uid, then an exact name, then a
/// case-insensitive name. A name matching several profiles is an error
/// listing their uids.
pub fn find_profile<'a>(items: &'a [PrfItem], query: &str) -> anyhow::Result<&'a PrfItem> {
    if let Some(item) = items.iter().find(|item| item.uid.as_deref() == Some(query)) {
        return Ok(item);
    }
    for exact in [true, false] {
        let matches: Vec<&PrfItem> = items
            .iter()
            .filter(|item| {
                item.name.as_deref().is_some_and(|name| {
                    if exact {
                        name == query
                    } else {
                        name.eq_ignore_ascii_case(query)
                    }
                })
            })
            .collect();
        match matches.as_slice() {
            [] => continue,
            [item] => return Ok(item),
            several => {
                let uids: Vec<&str> = several.iter().filter_map(|item| item.uid.as_deref()).collect();
                anyhow::bail!(
                    "profile name '{query}' is ambiguous; use one of the uids: {}",
                    uids.join(", ")
                );
            }
        }
    }
    anyhow::bail!("no profile with uid or name '{query}' (see `clash-verge-cli profile list`)")
}

/// Make `item` the current profile and apply it: remote subscriptions are
/// reloaded as-is, local profiles get their chain fragments merged into the
/// runtime config. On failure the previous current profile is restored (if
/// nothing else changed it meanwhile) and a user-facing error is returned.
pub async fn switch_profile(
    api: &MihomoApi,
    item: &PrfItem,
    enable_tun: bool,
    core_running: bool,
) -> Result<(), String> {
    let uid = item.uid.as_deref().ok_or("profile switch: profile has no uid")?;
    let previous_uid = ProfileStore::replace_current_locked(uid)
        .await
        .map_err(|error| format!("profile switch: {error}"))?;

    let applied = if item.itype.as_deref() == Some("remote") {
        reload_remote_profile(api, item, enable_tun, core_running)
            .await
            .map_err(|error| format!("profile reload: {error}"))
    } else {
        let profiles_dir = clash_verge_core::utils::dirs::app_profiles_dir().unwrap_or_default();
        match crate::chain::resolve_chain(item, &profiles_dir).await {
            Ok(chain) => commit_runtime_config(api, enable_tun, core_running, Some(item), |mut config| {
                crate::chain::apply_chain_to_config(&mut config, &chain);
                Ok(config)
            })
            .await
            .map(|_| ())
            .map_err(|error| format!("config write: {error}")),
            Err(error) => Err(format!("chain: {error}")),
        }
    };

    if applied.is_err() {
        let _ = ProfileStore::restore_current_if_matches(uid, previous_uid.as_deref()).await;
    }
    applied
}

/// Make `item` the current profile and apply it, dispatching by
/// [`CoreKind`]. Mirrors [`switch_profile`] but additionally handles
/// sing-box — whose controller does not honour `PUT /configs` — by
/// reading the profile YAML from disk and routing through
/// [`crate::runtime_config::apply_singbox_restart`], which regenerates
/// the JSON config (with prevalidation) and restarts the process.
///
/// Used by the TUI's `Enter`-on-Profiles flow. The CLI `profile use`
/// command still calls [`switch_profile`] and remains sing-box-broken
/// (out of scope here).
///
/// On a failed apply the previous `current` UID is restored, matching
/// the mihomo branch's semantics; the sing-box branch's underlying
/// restart already handles config-file rollback + one retry inside
/// `apply_singbox_restart`.
pub async fn switch_profile_for_core(
    manager: &MihomoManager,
    item: &PrfItem,
    enable_tun: bool,
    core_running: bool,
) -> Result<(), String> {
    let uid = item.uid.as_deref().ok_or("profile switch: profile has no uid")?;
    let previous_uid = ProfileStore::replace_current_locked(uid)
        .await
        .map_err(|error| format!("profile switch: {error}"))?;

    let applied = match decide_switch_path(manager.core_kind()) {
        SwitchPath::HotReload => switch_profile(&manager.api(), item, enable_tun, core_running).await,
        SwitchPath::SingboxRestart => {
            // sing-box: read the profile YAML from disk (chain resolution
            // is a clash-only concept; the sing-box pipeline converts
            // whatever proxies/proxy-groups are present and skips the
            // rest). For an unresolvable file we error early so the
            // current-uid rollback below can run.
            let file = item.file.as_deref().ok_or_else(|| {
                format!(
                    "profile switch: profile {} has no file",
                    item.uid.as_deref().unwrap_or("?")
                )
            })?;
            let profiles_dir = clash_verge_core::utils::dirs::app_profiles_dir()
                .map_err(|error| format!("profile switch: {error}"))?;
            let path = profiles_dir.join(file);
            let yaml = tokio::fs::read_to_string(&path)
                .await
                .map_err(|error| format!("profile switch: failed to read {}: {error}", path.display()))?;
            crate::runtime_config::apply_singbox_restart(manager, Some(yaml.as_str()), enable_tun)
                .await
                .map(|_report| ())
                .map_err(|error| format!("profile reload: {error}"))
        }
    };

    if applied.is_err() {
        let _ = ProfileStore::restore_current_if_matches(uid, previous_uid.as_deref()).await;
    }
    applied
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(uid: &str, name: &str) -> PrfItem {
        PrfItem {
            uid: Some(uid.into()),
            name: Some(name.into()),
            ..PrfItem::default()
        }
    }

    #[test]
    fn find_profile_prefers_uid_then_exact_then_case_insensitive_name() {
        let items = vec![
            item("R1", "Home"),
            item("R2", "home"),
            item("R3", "Work"),
            item("Work", "Other"),
        ];
        assert_eq!(find_profile(&items, "R2").unwrap().uid.as_deref(), Some("R2"));
        // A uid wins over a same-named profile.
        assert_eq!(find_profile(&items, "Work").unwrap().uid.as_deref(), Some("Work"));
        assert_eq!(find_profile(&items, "Home").unwrap().uid.as_deref(), Some("R1"));
        assert_eq!(find_profile(&items, "WORK").unwrap().uid.as_deref(), Some("R3"));
    }

    #[test]
    fn find_profile_rejects_ambiguous_and_unknown_names() {
        let items = vec![item("R1", "Home"), item("R2", "home")];
        let error = find_profile(&items, "HOME").unwrap_err().to_string();
        assert!(
            error.contains("ambiguous") && error.contains("R1") && error.contains("R2"),
            "{error}"
        );
        assert!(find_profile(&items, "Nope").is_err());
    }

    #[test]
    fn decide_switch_path_picks_hot_reload_for_mihomo() {
        // Mihomo's switch keeps going through the chain-resolved
        // `commit_runtime_config` path; the dispatcher does not interfere
        // with the rule-fragment composition that lives there.
        assert_eq!(decide_switch_path(CoreKind::Mihomo), SwitchPath::HotReload);
    }

    #[test]
    fn decide_switch_path_picks_singbox_restart_for_singbox() {
        // sing-box's switch must read the profile YAML and restart the
        // core via `apply_singbox_restart`; `commit_runtime_config`'s
        // `PUT /configs` is a no-op against sing-box's controller.
        assert_eq!(decide_switch_path(CoreKind::SingBox), SwitchPath::SingboxRestart);
    }

    #[test]
    fn manager_core_kind_round_trip_for_switch_profile() {
        // Same shape as the scheduler's `manager_core_kind_propagates_to
        // _dispatch_decision`: the switch helper reads `manager.core_kind`
        // so a manager built with `with_core_kind(SingBox)` must reach
        // the dispatcher as `SingboxRestart`.
        let mgr = MihomoManager::new(std::path::PathBuf::from("/tmp/cfg"));
        assert_eq!(decide_switch_path(mgr.core_kind()), SwitchPath::HotReload);
        let mgr = mgr.with_core_kind(CoreKind::SingBox);
        assert_eq!(decide_switch_path(mgr.core_kind()), SwitchPath::SingboxRestart);
    }

    #[tokio::test]
    async fn switch_profile_for_core_singbox_errors_when_profile_file_missing() {
        // End-to-end of the sing-box branch's error surface without
        // starting a real sing-box: a profile that points at a file we
        // never wrote must fail with a path-naming error BEFORE
        // `apply_singbox_restart` ever runs (otherwise the user sees a
        // confusing "sing-box binary not found" downstream).
        let root = crate::profile_store::store::tests::test_app_home_root();
        let _dir_guard = crate::profile_store::store::tests::claim_test_app_home(root.clone()).await;

        // Seed the store with a profile whose `file` does NOT exist on
        // disk. `replace_current_locked` writes to profiles.yaml but
        // does not touch the profile body file.
        let mut store = crate::profile_store::store::tests::empty_store();
        let uid = "Rsw-missing-file";
        let bundle = crate::subscribe::from_url::RemoteProfileBundle {
            item: clash_verge_core::config::PrfItem {
                uid: Some(uid.into()),
                itype: Some("remote".into()),
                name: Some("missing-file-demo".into()),
                file: Some(format!("{uid}.yaml").into()),
                ..Default::default()
            },
            fragments: vec![match clash_verge_core::config::PrfItem::from_merge(None) {
                Ok(item) => item,
                Err(error) => panic!("merge fragment: {error}"),
            }],
        };
        store.append_bundle(bundle).await.expect("append");

        let mgr = MihomoManager::new(root.clone()).with_core_kind(CoreKind::SingBox);
        let snapshot = crate::profile_store::store::ProfileStore::snapshot()
            .await
            .expect("snapshot");
        let item = snapshot
            .items()
            .into_iter()
            .find(|item| item.uid.as_deref() == Some(uid))
            .expect("seeded profile present")
            .clone();

        let error = switch_profile_for_core(&mgr, &item, false, false)
            .await
            .expect_err("missing profile file must error before sing-box is touched");
        assert!(
            error.contains(&format!("{uid}.yaml")) && error.contains("failed to read"),
            "error names the missing profile file and the read failure: {error}"
        );

        // With no previous current uid (the seeded profile became current
        // on import), `restore_current_if_matches` is a no-op — current
        // remains the seeded uid. That matches the mihomo branch's
        // documented semantics; what matters here is that the error
        // surfaces BEFORE any sing-box binary is touched.

        let _ = std::fs::remove_dir_all(&root);
    }
}
