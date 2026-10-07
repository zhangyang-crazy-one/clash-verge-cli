//! Profiles: resolving a user's profile argument and switching the current
//! profile.

use clash_verge_core::config::{DnsOverrideState, IVerge, PrfItem};
use serde_yaml_ng::Mapping;

use crate::mihomo_api::MihomoApi;
use crate::mihomo_manager::{CoreKind, MihomoManager};
use crate::profile_store::store::ProfileStore;
use crate::runtime_config::{commit_runtime_config, reload_remote_profile};

/// Prepare a candidate profile with the shared GUI DNS section applied when
/// this profile's setting is enabled and its provider DNS source is confirmed.
/// The source decision is captured before the overlay, so the global values
/// cannot invalidate their own confirmation.
pub fn prepare_profile_dns(
    profile_uid: &str,
    mut effective_config: Mapping,
    global_dns_config: &Mapping,
    verge: &IVerge,
) -> Result<(Mapping, DnsOverrideState), String> {
    if profile_uid.is_empty() {
        return Err("profile DNS override requires a profile uid".into());
    }
    let state = verge
        .dns_override_for(profile_uid, &effective_config)
        .map_err(|error| format!("failed to identify profile DNS source: {error}"))?;
    if state.enabled {
        crate::chain::apply_dns_override(&mut effective_config, global_dns_config)
            .map_err(|error| format!("failed to apply global DNS settings: {error}"))?;
    }
    Ok((effective_config, state))
}

/// Read the GUI-compatible DNS section file. Its root mapping is the content
/// placed beneath `dns:` in the effective Mihomo config. A missing file means
/// there is no global DNS overlay; malformed or unreadable files are errors.
pub async fn read_global_dns_config() -> Result<Mapping, String> {
    let path = clash_verge_core::utils::dirs::app_home_dir()
        .map_err(|error| error.to_string())?
        .join("dns_config.yaml");
    let raw = match tokio::fs::read_to_string(&path).await {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Mapping::new()),
        Err(error) => return Err(format!("failed to read {}: {error}", path.display())),
    };
    serde_yaml_ng::from_str(&raw).map_err(|error| format!("invalid DNS config in {}: {error}", path.display()))
}

/// Convenience entry point for runtime candidate builders: load the latest
/// Verge preference and the shared GUI DNS section, then return the prepared
/// mapping and the source-bound state that must be persisted after success.
pub async fn prepare_profile_dns_from_settings(
    profile_uid: &str,
    effective_config: Mapping,
) -> Result<(Mapping, DnsOverrideState), String> {
    if profile_uid.is_empty() {
        return Err("profile DNS override requires a profile uid".into());
    }
    let verge = IVerge::try_new()
        .await
        .map_err(|error| format!("failed to load Verge DNS settings: {error}"))?;
    let state = verge
        .dns_override_for(profile_uid, &effective_config)
        .map_err(|error| format!("failed to identify profile DNS source: {error}"))?;
    if !state.enabled {
        return Ok((effective_config, state));
    }
    let global_dns = read_global_dns_config().await?;
    prepare_profile_dns(profile_uid, effective_config, &global_dns, &verge)
}

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

    let applied = apply_profile(api, item, enable_tun, core_running).await;

    if applied.is_err() {
        let _ = ProfileStore::restore_current_if_matches(uid, previous_uid.as_deref()).await;
    }
    applied
}

async fn apply_profile(api: &MihomoApi, item: &PrfItem, enable_tun: bool, core_running: bool) -> Result<(), String> {
    if item
        .option
        .as_ref()
        .and_then(|option| option.script.as_deref())
        .is_some_and(|uid| !uid.is_empty())
    {
        return Err("script profile overrides are unsupported by the standalone TUI".into());
    }
    if item.itype.as_deref() == Some("remote") {
        reload_remote_profile(api, item, enable_tun, core_running)
            .await
            .map_err(|error| format!("profile reload: {error}"))
    } else {
        let profiles_dir = clash_verge_core::utils::dirs::app_profiles_dir().unwrap_or_default();
        match crate::chain::resolve_chain(item, &profiles_dir).await {
            Ok(chain) => commit_runtime_config(api, enable_tun, core_running, Some(item), |mut config| {
                crate::chain::apply_chain_to_config(&mut config, &chain).map_err(|error| error.to_string())?;
                Ok(config)
            })
            .await
            .map(|_| ())
            .map_err(|error| format!("config write: {error}")),
            Err(error) => Err(format!("chain: {error}")),
        }
    }
}

/// Make `item` the current profile and apply it, dispatching by
/// [`CoreKind`]. Mirrors [`switch_profile`] but additionally handles
/// sing-box — whose controller does not honour `PUT /configs` — by
/// reading the profile YAML from disk and routing through
/// [`crate::runtime_config::apply_singbox_restart`], which regenerates
/// the JSON config (with prevalidation) and restarts the process.
///
/// Used by the TUI and CLI selected-core profile flows.
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

    let applied = async {
        match decide_switch_path(manager.core_kind()) {
            // Current UID was captured and replaced above, so do not call
            // `switch_profile` here and replace it a second time.
            SwitchPath::HotReload => apply_profile(&manager.api(), item, enable_tun, core_running).await,
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
                crate::runtime_config::apply_singbox_restart_for_profile(
                    manager,
                    Some(yaml.as_str()),
                    enable_tun,
                    Some(uid),
                )
                .await
                .map(|_report| ())
                .map_err(|error| format!("profile reload: {error}"))
            }
        }
    }
    .await;

    if applied.is_err() {
        let _ = ProfileStore::restore_current_if_matches(uid, previous_uid.as_deref()).await;
    }
    applied
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_yaml_ng::Value;

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
    fn prepare_profile_dns_overlays_only_after_matching_confirmation() {
        let original = serde_yaml_ng::from_str::<Mapping>(
            "port: 7890\ndns:\n  proxy-server-nameserver: [https://provider.example/dns]\n  nameserver: [profile-dns]\n  future: {keep: true}\n",
        )
        .expect("profile fixture");
        let global =
            serde_yaml_ng::from_str::<Mapping>("nameserver: []\nfallback: [8.8.8.8]\nfuture: {global: true}\n")
                .expect("global DNS fixture");
        let source = clash_verge_core::config::dns_override_source("R1", &original).expect("valid source identity");
        let mut verge = IVerge::default();
        verge.enable_dns_settings = Some(true);
        verge.profile_dns_settings.insert(
            "R1".into(),
            clash_verge_core::config::ProfileDnsSettings {
                enabled: true,
                confirmation: Some(source.clone().expect("provider DNS source")),
                ..Default::default()
            },
        );

        let (prepared, state) =
            prepare_profile_dns("R1", original.clone(), &global, &verge).expect("confirmed profile prepares");
        assert!(state.enabled);
        assert_eq!(state.source, source, "source is captured before applying global DNS");
        assert_eq!(prepared["port"], original["port"]);
        assert_eq!(prepared["dns"]["nameserver"], Value::Sequence(Vec::new()));
        assert_eq!(
            prepared["dns"]["proxy-server-nameserver"],
            original["dns"]["proxy-server-nameserver"]
        );
        assert_eq!(prepared["dns"]["future"]["keep"], Value::from(true));
        assert_eq!(prepared["dns"]["future"]["global"], Value::from(true));
        assert_eq!(prepared["dns"]["fallback"][0], Value::from("8.8.8.8"));
    }

    #[test]
    fn prepare_profile_dns_keeps_candidate_when_provider_source_is_unconfirmed() {
        let original = serde_yaml_ng::from_str::<Mapping>(
            "dns: {proxy-server-nameserver: [https://provider.example/dns], nameserver: [profile-dns]}\n",
        )
        .expect("profile fixture");
        let global = serde_yaml_ng::from_str::<Mapping>("nameserver: [global-dns]\n").expect("global DNS fixture");
        let mut verge = IVerge::default();
        verge.enable_dns_settings = Some(true);

        let (prepared, state) = prepare_profile_dns("R1", original.clone(), &global, &verge)
            .expect("unconfirmed profile is still a valid candidate");
        assert!(!state.enabled);
        assert_eq!(prepared, original, "unconfirmed provider DNS stays intact");
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
