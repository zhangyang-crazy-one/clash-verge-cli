//! Profiles: resolving a user's profile argument and switching the current
//! profile.

use clash_verge_core::config::PrfItem;

use crate::mihomo_api::MihomoApi;
use crate::profile_store::store::ProfileStore;
use crate::runtime_config::{commit_runtime_config, reload_remote_profile};

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
}
