// Profile store — thin wrapper around IProfiles for TUI/CLI use.

use std::sync::LazyLock;

use anyhow::Context;
use clash_verge_core::config::{IProfiles, PrfItem, PrfOption};
use smartstring::alias::String as SmartString;
use tokio::sync::Mutex;

use crate::subscribe::from_url::{self, RemoteProfileBundle};

/// Serializes all profiles.yaml load-modify-save sequences across TUI/CLI tasks.
static PROFILE_IO: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

/// In-memory view of the profile list. Re-reads from disk on each load.
pub struct ProfileStore {
    profiles: IProfiles,
}

impl ProfileStore {
    /// Read-only snapshot under the shared IO lock (avoids torn reads during a save).
    pub async fn snapshot() -> anyhow::Result<Self> {
        let _guard = PROFILE_IO.lock().await;
        Self::load_unlocked().await
    }

    /// Atomically capture the previous `current` UID and install `uid`.
    ///
    /// Returns the previous UID (if any) from the same critical section that writes
    /// the new one, so concurrent switches cannot both snapshot the same predecessor.
    pub async fn replace_current_locked(uid: &str) -> anyhow::Result<Option<String>> {
        let _guard = PROFILE_IO.lock().await;
        let mut store = Self::load_unlocked().await?;
        let previous = store.current_uid().map(|current| current.to_string());
        store.set_current(uid).await?;
        Ok(previous)
    }

    /// Restore `previous` only when `current` is still `expected` (failed switch owned it).
    ///
    /// Prevents a late-failing concurrent switch from overwriting a successful one.
    pub async fn restore_current_if_matches(expected: &str, previous: Option<&str>) -> anyhow::Result<()> {
        let Some(previous) = previous else {
            return Ok(());
        };
        let _guard = PROFILE_IO.lock().await;
        let mut store = Self::load_unlocked().await?;
        if store.current_uid().as_deref() != Some(expected) {
            return Ok(());
        }
        store.set_current(previous).await
    }

    /// Import a subscription URL under the shared IO lock.
    pub async fn import_url_locked(
        url: &str,
        name: Option<&str>,
        option: Option<&PrfOption>,
    ) -> anyhow::Result<PrfItem> {
        let _guard = PROFILE_IO.lock().await;
        let mut store = Self::load_unlocked().await?;
        store.import_url(url, name, option).await
    }

    /// Update one remote profile under the shared IO lock.
    pub async fn update_remote_locked(uid: &str, option_override: Option<&PrfOption>) -> anyhow::Result<bool> {
        let _guard = PROFILE_IO.lock().await;
        let mut store = Self::load_unlocked().await?;
        store.update_remote(uid, option_override).await
    }

    /// Update every remote profile under the shared IO lock.
    pub async fn update_all_remote_locked() -> anyhow::Result<Vec<SmartString>> {
        let _guard = PROFILE_IO.lock().await;
        let mut store = Self::load_unlocked().await?;
        store.update_all_remote().await
    }

    /// Update a batch of remote UIDs under one lock (auto-update).
    /// Continues after per-UID failures so one bad subscription does not block the rest.
    pub async fn update_remotes_locked(
        uids: &[String],
    ) -> anyhow::Result<(Vec<(String, bool)>, Vec<(String, String)>)> {
        let _guard = PROFILE_IO.lock().await;
        let mut store = Self::load_unlocked().await?;
        let mut updated = Vec::new();
        let mut failed = Vec::new();
        for uid in uids {
            match store.update_remote(uid, None).await {
                Ok(is_current) => updated.push((uid.clone(), is_current)),
                Err(error) => failed.push((uid.clone(), error.to_string())),
            }
        }
        Ok((updated, failed))
    }

    /// Delete a profile by UID under the shared IO lock.
    /// Also removes associated chain fragment files.
    pub async fn delete_locked(uid: &str) -> anyhow::Result<()> {
        let _guard = PROFILE_IO.lock().await;
        let mut store = Self::load_unlocked().await?;
        let uid_key = smartstring::alias::String::from(uid);
        let was_current = store.profiles.delete_item(&uid_key).await?;
        let _ = was_current;
        Ok(())
    }

    /// Rename a profile by UID under the shared IO lock.
    pub async fn rename_locked(uid: &str, new_name: &str) -> anyhow::Result<()> {
        let _guard = PROFILE_IO.lock().await;
        let mut store = Self::load_unlocked().await?;
        let uid_key = smartstring::alias::String::from(uid);
        let patch = PrfItem {
            name: Some(smartstring::alias::String::from(new_name)),
            ..Default::default()
        };
        store.profiles.patch_item(&uid_key, &patch).await?;
        Ok(())
    }

    /// Unlocked load — callers that mutate must use the `*_locked` helpers.
    async fn load_unlocked() -> anyhow::Result<Self> {
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

    /// Persist the GUI `current` profile UID (used when switching in the TUI).
    pub async fn set_current(&mut self, uid: &str) -> anyhow::Result<()> {
        let uid = SmartString::from(uid);
        self.profiles.patch_config(&IProfiles {
            current: Some(uid),
            items: None,
        });
        self.profiles
            .save_file()
            .await
            .context("failed to save profiles.yaml after set_current")
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

    /// Append enhance fragments then the remote item, and persist.
    pub async fn append_bundle(&mut self, bundle: RemoteProfileBundle) -> anyhow::Result<PrfItem> {
        ensure_profile_storage().await?;
        for mut fragment in bundle.fragments {
            self.profiles
                .append_item(&mut fragment)
                .await
                .context("failed to append profile fragment")?;
        }
        let mut item = bundle.item;
        let saved = item.clone();
        self.profiles
            .append_item(&mut item)
            .await
            .context("failed to append remote profile")?;
        self.profiles
            .save_file()
            .await
            .context("failed to save profiles.yaml")?;
        Ok(saved)
    }

    /// Import a subscription URL with GUI-style proxy fallbacks.
    pub async fn import_url(
        &mut self,
        url: &str,
        name: Option<&str>,
        option: Option<&PrfOption>,
    ) -> anyhow::Result<PrfItem> {
        let bundle = from_url::import_with_fallback(url, name, option).await?;
        self.append_bundle(bundle).await
    }

    /// Update a remote profile by UID. Returns whether it is the current profile.
    pub async fn update_remote(&mut self, uid: &str, option_override: Option<&PrfOption>) -> anyhow::Result<bool> {
        let uid_key = SmartString::from(uid);
        let existing = self.profiles.get_item(&uid_key).context("profile not found")?.clone();

        if existing.itype.as_deref() != Some("remote") {
            anyhow::bail!("profile {uid} is not a remote subscription");
        }
        let url = existing.url.as_ref().context("remote profile is missing url")?;

        let merged = PrfOption::merge(existing.option.as_ref(), option_override);
        let mut bundle = from_url::update_with_fallback(url, merged.as_ref()).await?;

        // Rare path: an older remote without chain UIDs gets fragments on refresh.
        ensure_profile_storage().await?;
        for mut fragment in bundle.fragments {
            self.profiles
                .append_item(&mut fragment)
                .await
                .context("failed to append profile fragment during update")?;
        }

        let is_current = self.profiles.get_current().map(|c| c.as_str()) == Some(uid);
        self.profiles
            .update_item(&uid_key, &mut bundle.item)
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
    use crate::subscribe::from_url::RemoteProfileBundle;

    #[tokio::test]
    async fn append_bundle_persists_profiles_yaml_and_body() {
        let root = std::env::temp_dir().join(format!("clash-verge-cli-profile-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join("profiles")).expect("temp profiles dir");
        dirs::set_app_home_dir(root.clone());
        assert_eq!(
            dirs::app_home_dir().expect("home"),
            root,
            "test requires exclusive app home dir; another test may have claimed OnceLock"
        );

        let mut store = ProfileStore {
            profiles: IProfiles::default(),
        };
        let uid = "Rpersist01ab";
        let file_name = format!("{uid}.yaml");
        let bundle = RemoteProfileBundle {
            item: PrfItem {
                uid: Some(uid.into()),
                itype: Some("remote".into()),
                name: Some("persist-demo".into()),
                file: Some(file_name.clone().into()),
                url: Some("https://example.com/sub.yaml".into()),
                file_data: Some("proxies: []\n".into()),
                ..Default::default()
            },
            fragments: vec![PrfItem::from_merge(None).expect("merge fragment")],
        };

        store.append_bundle(bundle).await.expect("append_bundle");

        let profiles_yaml = std::fs::read_to_string(root.join("profiles.yaml")).expect("profiles.yaml");
        assert!(profiles_yaml.contains(uid));
        assert!(profiles_yaml.contains("persist-demo"));
        let body = std::fs::read_to_string(root.join("profiles").join(&file_name)).expect("body");
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
