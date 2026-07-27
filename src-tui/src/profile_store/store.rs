// Profile store — thin wrapper around IProfiles for TUI/CLI use.

use anyhow::Context;
use clash_verge_core::config::{IProfiles, PrfItem, PrfOption};
use smartstring::alias::String as SmartString;

use crate::subscribe::from_url;

/// In-memory view of the profile list. Re-reads from disk on each load.
pub struct ProfileStore {
    profiles: IProfiles,
}

impl ProfileStore {
    pub async fn load() -> anyhow::Result<Self> {
        let profiles = IProfiles::new().await;
        Ok(Self { profiles })
    }

    /// Return the profiles a user can select in the TUI.
    ///
    /// The GUI keeps merge/script/rules/proxies/groups fragments alongside remote
    /// subscriptions so it can build the active configuration. Those fragments
    /// are implementation details, not independently switchable profiles.
    pub fn items(&self) -> Vec<PrfItem> {
        self.profiles
            .get_items()
            .into_iter()
            .flatten()
            .filter(|item| item.itype.as_deref() == Some("remote"))
            .cloned()
            .collect()
    }

    /// All items including enhance fragments (for diagnostics / CLI).
    #[allow(dead_code)]
    pub fn all_items(&self) -> Vec<PrfItem> {
        self.profiles.get_items().into_iter().flatten().cloned().collect()
    }

    /// Resolve the GUI's current profile UID into a stable TUI list index.
    pub fn selected_index(&self) -> usize {
        let Some(current) = self.profiles.get_current() else {
            return 0;
        };
        self.items()
            .iter()
            .position(|item| item.uid.as_ref() == Some(current))
            .unwrap_or_default()
    }

    pub fn current_uid(&self) -> Option<SmartString> {
        self.profiles.get_current().cloned()
    }

    /// Append a new profile item and persist `profiles.yaml`.
    #[allow(dead_code)]
    pub async fn append(&mut self, mut item: PrfItem) -> anyhow::Result<()> {
        ensure_profile_storage().await?;
        self.profiles
            .append_item(&mut item)
            .await
            .context("failed to append profile")?;
        self.profiles.save_file().await.context("failed to save profiles.yaml")
    }

    /// Import a subscription URL.
    pub async fn import_url(&mut self, url: &str, name: Option<&str>) -> anyhow::Result<PrfItem> {
        let name = name.unwrap_or(url);
        let item = from_url::from_url(url, name).await?;
        self.append(item.clone()).await?;
        Ok(item)
    }

    /// Update a remote profile by UID. Returns whether it is the current profile.
    pub async fn update_remote(&mut self, uid: &str, _option_override: Option<&PrfOption>) -> anyhow::Result<bool> {
        let uid_key = SmartString::from(uid);
        let existing = self.profiles.get_item(&uid_key).context("profile not found")?.clone();

        if existing.itype.as_deref() != Some("remote") {
            anyhow::bail!("profile {uid} is not a remote subscription");
        }
        let url = existing.url.as_ref().context("remote profile is missing url")?;

        let mut item = from_url::from_url(url, url).await?;
        item.uid = Some(uid_key.clone());

        let is_current = self.profiles.get_current().map(|c| c.as_str()) == Some(uid);
        self.profiles
            .update_item(&uid_key, &mut item)
            .await
            .context("failed to update remote profile")?;
        Ok(is_current)
    }

    /// Update every remote profile. Returns UIDs that are current (0 or 1).
    pub async fn update_all_remote(&mut self) -> anyhow::Result<Vec<SmartString>> {
        let remotes: Vec<(SmartString, Option<SmartString>)> = self
            .items()
            .into_iter()
            .filter_map(|item| {
                let uid = item.uid?;
                Some((uid, item.url))
            })
            .collect();

        let mut refreshed_current = Vec::new();
        let mut errors = Vec::new();
        for (uid, _) in remotes {
            match self.update_remote(uid.as_str(), None).await {
                Ok(true) => refreshed_current.push(uid),
                Ok(false) => {}
                Err(err) => errors.push(format!("{uid}: {err}")),
            }
        }
        if !errors.is_empty() {
            anyhow::bail!("{}", errors.join("; "));
        }
        Ok(refreshed_current)
    }
}

async fn ensure_profile_storage() -> anyhow::Result<()> {
    use clash_verge_core::utils::dirs;
    let home = dirs::app_home_dir().context("app home dir not initialized")?;
    tokio::fs::create_dir_all(&home)
        .await
        .with_context(|| format!("failed to create {}", home.display()))?;
    let profiles = dirs::app_profiles_dir().context("profiles dir unavailable")?;
    tokio::fs::create_dir_all(&profiles)
        .await
        .with_context(|| format!("failed to create {}", profiles.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use clash_verge_core::config::{IProfiles, PrfItem};
    use clash_verge_core::utils::dirs;

    use super::*;

    #[tokio::test]
    #[ignore = "requires a real subscription URL via SUB_TEST_URL env var"]
    async fn import_url_persists_profiles_yaml_and_body() {
        let url = std::env::var("SUB_TEST_URL").expect("SUB_TEST_URL env var not set");
        let root = std::env::temp_dir().join(format!("clash-verge-cli-profile-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join("profiles")).expect("temp profiles dir");
        dirs::set_app_home_dir(root.clone());

        let mut store = ProfileStore {
            profiles: IProfiles::default(),
        };
        let item = store.import_url(&url, Some("test-profile")).await.expect("import_url");
        assert!(item.uid.is_some());
        assert_eq!(item.itype.as_deref(), Some("remote"));

        let profiles_yaml = std::fs::read_to_string(root.join("profiles.yaml")).expect("profiles.yaml");
        assert!(profiles_yaml.contains("test-profile"));
        let file = item.file.as_ref().unwrap();
        let body = std::fs::read_to_string(root.join("profiles").join(file.as_str())).expect("body");
        assert!(body.contains("proxies:"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn selected_index_follows_the_gui_current_uid() {
        let store = ProfileStore {
            profiles: IProfiles {
                current: Some("remote".into()),
                items: Some(vec![
                    PrfItem {
                        uid: Some("merge".into()),
                        itype: Some("merge".into()),
                        ..Default::default()
                    },
                    PrfItem {
                        uid: Some("remote".into()),
                        itype: Some("remote".into()),
                        ..Default::default()
                    },
                ]),
            },
        };

        assert_eq!(store.selected_index(), 0);
    }

    #[test]
    fn items_exclude_gui_internal_profile_fragments() {
        let store = ProfileStore {
            profiles: IProfiles {
                current: Some("remote-b".into()),
                items: Some(vec![
                    PrfItem {
                        uid: Some("merge".into()),
                        itype: Some("merge".into()),
                        ..Default::default()
                    },
                    PrfItem {
                        uid: Some("remote-a".into()),
                        itype: Some("remote".into()),
                        ..Default::default()
                    },
                    PrfItem {
                        uid: Some("rules".into()),
                        itype: Some("rules".into()),
                        ..Default::default()
                    },
                    PrfItem {
                        uid: Some("remote-b".into()),
                        itype: Some("remote".into()),
                        ..Default::default()
                    },
                ]),
            },
        };

        let items = store.items();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].uid.as_deref(), Some("remote-a"));
        assert_eq!(items[1].uid.as_deref(), Some("remote-b"));
        assert_eq!(store.selected_index(), 1);
    }
}
