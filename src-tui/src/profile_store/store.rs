// Profile store — thin wrapper around IProfiles for TUI/CLI use.

use std::sync::LazyLock;

use anyhow::Context;
use clash_verge_core::config::{IProfiles, PrfItem, PrfOption};
use compact_str::CompactString;
use serde_yaml_ng::{Mapping, Value};
use tokio::sync::Mutex;

use crate::subscribe::from_url::{self, RemoteProfileBundle};

/// Serializes all profiles.yaml load-modify-save sequences across TUI/CLI tasks.
static PROFILE_IO: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

/// In-memory view of the profile list. Re-reads from disk on each load.
pub struct ProfileStore {
    profiles: IProfiles,
}

/// Captured inside the same IO transaction as the forced-refresh write.
pub(crate) struct RefreshSnapshot {
    uid: String,
    metadata: Vec<u8>,
    body: Vec<u8>,
    file: String,
    fragments: Vec<RefreshFragment>,
}

struct RefreshFragment {
    item: Value,
    file: String,
    body: Vec<u8>,
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
        let _guard = PROFILE_IO.lock().await;
        let mut store = Self::load_unlocked().await?;
        if store.current_uid().as_deref() != Some(expected) {
            return Ok(());
        }
        match previous {
            Some(previous) => store.set_current(previous).await,
            None => {
                store.profiles.current = None;
                store.profiles.save_file().await
            }
        }
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

    /// CLI import stores a profile without claiming it was applied. TUI
    /// import retains its explicit first-profile application transaction.
    pub async fn import_url_unselected_locked(
        url: &str,
        name: Option<&str>,
        option: Option<&PrfOption>,
    ) -> anyhow::Result<PrfItem> {
        let _guard = PROFILE_IO.lock().await;
        let mut store = Self::load_unlocked().await?;
        let bundle = from_url::import_with_fallback(url, name, option).await?;
        store.append_bundle_with_selection(bundle, false).await
    }

    /// Update one remote profile under the shared IO lock.
    pub async fn update_remote_locked(uid: &str, option_override: Option<&PrfOption>) -> anyhow::Result<bool> {
        let _guard = PROFILE_IO.lock().await;
        let mut store = Self::load_unlocked().await?;
        store.update_remote(uid, option_override).await
    }

    /// Update every remote profile under the shared IO lock.
    pub async fn update_all_remote_locked() -> anyhow::Result<Vec<CompactString>> {
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
        let uid_key = CompactString::from(uid);
        let item = store.profiles.get_item(&uid_key).context("profile not found")?.clone();
        let was_current = store.profiles.delete_item(&uid_key).await?;
        let _ = was_current;
        let profiles = clash_verge_core::utils::dirs::app_profiles_dir()?;
        let overlay = crate::rule_overlay::path(&profiles, &item).map_err(anyhow::Error::msg)?;
        match tokio::fs::remove_file(&overlay).await {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| format!("failed to remove rule overlay {}", overlay.display()));
            }
        }
        Ok(())
    }

    /// Identify the refresh's writes before any queued profile edit can run.
    /// The lock is released before runtime reload/readiness begins.
    pub(crate) async fn force_refresh_locked(uid: &str) -> anyhow::Result<(RefreshSnapshot, RefreshSnapshot)> {
        let _guard = PROFILE_IO.lock().await;
        let mut store = Self::load_unlocked().await?;
        let previous_uids: Vec<_> = store
            .profiles
            .get_items()
            .into_iter()
            .flatten()
            .filter_map(|item| item.uid.clone())
            .collect();
        let old = store.refresh_snapshot(uid).await?;
        store.update_remote(uid, None).await?;
        let mut expected = store.refresh_snapshot(uid).await?;
        let expected_metadata: Value = serde_yaml_ng::from_slice(&expected.metadata)?;
        for item in profile_items(&expected_metadata)? {
            if previous_uids
                .iter()
                .any(|uid| item["uid"].as_str() == Some(uid.as_str()))
            {
                continue;
            }
            let file = item["file"]
                .as_str()
                .context("new refresh fragment has no file")?
                .to_owned();
            let body = tokio::fs::read(clash_verge_core::utils::dirs::app_profiles_dir()?.join(&file)).await?;
            expected.fragments.push(RefreshFragment {
                item: item.clone(),
                file,
                body,
            });
        }
        Ok((old, expected))
    }

    async fn refresh_snapshot(&self, uid: &str) -> anyhow::Result<RefreshSnapshot> {
        let item = self.profiles.get_item(CompactString::from(uid))?;
        let file = item.file.as_deref().context("profile has no file")?.to_owned();
        let metadata = tokio::fs::read(clash_verge_core::utils::dirs::profiles_path()?).await?;
        let body = tokio::fs::read(clash_verge_core::utils::dirs::app_profiles_dir()?.join(&file)).await?;
        Ok(RefreshSnapshot {
            uid: uid.to_owned(),
            metadata,
            body,
            file,
            fragments: Vec::new(),
        })
    }

    pub(crate) async fn restore_refresh_if_matches(
        old: &RefreshSnapshot,
        expected: &RefreshSnapshot,
    ) -> anyhow::Result<()> {
        let _guard = PROFILE_IO.lock().await;
        let metadata = clash_verge_core::utils::dirs::profiles_path()?;
        let profiles_dir = clash_verge_core::utils::dirs::app_profiles_dir()?;
        let source = profiles_dir.join(&expected.file);
        let current_bytes = tokio::fs::read(&metadata).await?;
        let mut current: Value = serde_yaml_ng::from_slice(&current_bytes)?;
        let old_metadata: Value = serde_yaml_ng::from_slice(&old.metadata)?;
        let expected_metadata: Value = serde_yaml_ng::from_slice(&expected.metadata)?;
        let old_item = profile_item(&old_metadata, &old.uid)?;
        let expected_item = profile_item(&expected_metadata, &expected.uid)?;
        let current_item = profile_items_mut(&mut current)?
            .iter_mut()
            .find(|item| item["uid"].as_str() == Some(old.uid.as_str()))
            .context("refreshed profile was removed; refusing rollback")?;
        let current_file_matches = current_item["file"].as_str() == Some(expected.file.as_str());
        let mut conflict = !current_file_matches
            || old.file != expected.file
            || current_item["url"] != expected_item["url"]
            || current_item["type"] != expected_item["type"];
        // These are the only fields update_item owns. Desc/name/current,
        // unknown fields and other profiles are never rollback targets.
        let mapping = current_item.as_mapping_mut().context("profile item is not a mapping")?;
        for field in ["updated", "extra", "home", "option", "file"] {
            let key = Value::from(field);
            conflict |= restore_field_delta(mapping, &key, old_item.get(field), expected_item.get(field));
        }
        let body_matches = tokio::fs::read(&source).await? == expected.body;
        // Keep A's source and metadata as one transaction: a newer owned
        // field or body rejects the entire rollback, including fragment cleanup.
        anyhow::ensure!(
            !conflict && body_matches,
            "profile changed after forced refresh; refusing to overwrite newer source or metadata"
        );

        // Remove only this refresh's newly generated fragments. A later
        // reference from any surviving profile makes the fragment shared.
        let mut protected_fragment = false;
        let mut removed_files = Vec::new();
        for fragment in &expected.fragments {
            let uid = fragment.item["uid"].as_str().context("fragment has no uid")?;
            if profile_items(&current)?
                .iter()
                .any(|item| references_fragment(item, uid))
            {
                protected_fragment = true;
                continue;
            }
            let items = profile_items_mut(&mut current)?;
            let Some(index) = items.iter().position(|item| item["uid"].as_str() == Some(uid)) else {
                continue;
            };
            let path = profiles_dir.join(&fragment.file);
            let bytes = match tokio::fs::read(&path).await {
                Ok(bytes) => Some(bytes),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => return Err(error.into()),
            };
            if items[index] != fragment.item || bytes.as_deref() != Some(fragment.body.as_slice()) {
                protected_fragment = true;
                continue;
            }
            items.remove(index);
            removed_files.push(path);
        }

        tokio::fs::write(source, &old.body).await?;
        // Preserve exact old bytes when the document has no other changes;
        // otherwise serialize the merged current document, including unknowns.
        if current_bytes == expected.metadata && !protected_fragment {
            tokio::fs::write(metadata, &old.metadata).await?;
        } else {
            tokio::fs::write(metadata, serde_yaml_ng::to_string(&current)?).await?;
        }
        // Persist removal of chain references before deleting their files.
        for path in removed_files {
            tokio::fs::remove_file(path).await?;
        }
        Ok(())
    }

    /// Persist a trusted host into a remote profile's stored `option.trusted_hosts`.
    ///
    /// Merges with any entries the profile already trusts (deduplicated via
    /// `from_url::merge_trusted_host`), then saves `profiles.yaml`. The saved
    /// option is what the next `update_remote` fetch re-reads into its SSRF
    /// allowlist, so a manual refresh that was blocked becomes refreshable
    /// without an explicit option override.
    pub async fn add_trusted_host_locked(uid: &str, host: &str) -> anyhow::Result<()> {
        let _guard = PROFILE_IO.lock().await;
        let mut store = Self::load_unlocked().await?;
        let uid_key = CompactString::from(uid);
        let existing = store.profiles.get_item(&uid_key).context("profile not found")?.clone();
        if existing.itype.as_deref() != Some("remote") {
            anyhow::bail!("profile {uid} is not a remote subscription");
        }
        let mut option = existing.option.clone().unwrap_or_default();
        let merged = from_url::merge_trusted_host(option.trusted_hosts.as_deref(), host)
            .ok_or_else(|| anyhow::anyhow!("trusted host does not normalize: {host}"))?;
        option.trusted_hosts = Some(merged.into_iter().map(CompactString::from).collect());
        let patch = PrfItem {
            option: Some(option),
            ..Default::default()
        };
        store.profiles.patch_item(&uid_key, &patch).await?;
        Ok(())
    }

    /// Rename a profile by UID under the shared IO lock.
    pub async fn rename_locked(uid: &str, new_name: &str) -> anyhow::Result<()> {
        let _guard = PROFILE_IO.lock().await;
        let mut store = Self::load_unlocked().await?;
        let uid_key = CompactString::from(uid);
        let patch = PrfItem {
            name: Some(CompactString::from(new_name)),
            ..Default::default()
        };
        store.profiles.patch_item(&uid_key, &patch).await?;
        Ok(())
    }

    /// Unlocked load — callers that mutate must use the `*_locked` helpers.
    async fn load_unlocked() -> anyhow::Result<Self> {
        let profiles = IProfiles::new().await?;
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
            .filter(|item| matches!(item.itype.as_deref(), Some("remote" | "local")))
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

    pub fn current_uid(&self) -> Option<CompactString> {
        self.profiles.get_current().cloned()
    }

    /// Persist the GUI `current` profile UID (used when switching in the TUI).
    pub async fn set_current(&mut self, uid: &str) -> anyhow::Result<()> {
        let uid = CompactString::from(uid);
        self.profiles.patch_config(&IProfiles {
            current: Some(uid),
            items: None,
            ..Default::default()
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
        self.append_bundle_with_selection(bundle, true).await
    }

    async fn append_bundle_with_selection(
        &mut self,
        bundle: RemoteProfileBundle,
        select_first: bool,
    ) -> anyhow::Result<PrfItem> {
        ensure_profile_storage().await?;
        for mut fragment in bundle.fragments {
            self.profiles
                .append_item_with_selection(&mut fragment, select_first)
                .await
                .context("failed to append profile fragment")?;
        }
        let mut item = bundle.item;
        let saved = item.clone();
        self.profiles
            .append_item_with_selection(&mut item, select_first)
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
        let uid_key = CompactString::from(uid);
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
    pub async fn update_all_remote(&mut self) -> anyhow::Result<Vec<CompactString>> {
        let remotes: Vec<(CompactString, Option<CompactString>)> = self
            .items()
            .into_iter()
            .filter(|item| item.itype.as_deref() == Some("remote"))
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

fn profile_items(metadata: &Value) -> anyhow::Result<&Vec<Value>> {
    metadata["items"]
        .as_sequence()
        .context("profile metadata has no item list")
}

fn profile_items_mut(metadata: &mut Value) -> anyhow::Result<&mut Vec<Value>> {
    metadata["items"]
        .as_sequence_mut()
        .context("profile metadata has no item list")
}

fn profile_item<'a>(metadata: &'a Value, uid: &str) -> anyhow::Result<&'a Value> {
    profile_items(metadata)?
        .iter()
        .find(|item| item["uid"].as_str() == Some(uid))
        .context("profile missing from refresh snapshot")
}

fn references_fragment(item: &Value, uid: &str) -> bool {
    ["merge", "script", "rules", "proxies", "groups"]
        .iter()
        .any(|key| item["option"][key].as_str() == Some(uid))
}

fn null_or_missing(value: Option<&Value>) -> bool {
    value.is_none_or(Value::is_null)
}

/// Apply only the writer's leaf delta to a candidate document. The caller
/// discards that document if any changed leaf no longer matches expected.
fn restore_field_delta(current: &mut Mapping, key: &Value, old: Option<&Value>, expected: Option<&Value>) -> bool {
    if old == expected || (null_or_missing(old) && null_or_missing(expected)) {
        return false;
    }
    let old_map = old.and_then(Value::as_mapping);
    let expected_map = expected.and_then(Value::as_mapping);
    if (old_map.is_some() || expected_map.is_some())
        && (old_map.is_some() || null_or_missing(old))
        && (expected_map.is_some() || null_or_missing(expected))
        && let Some(current_map) = current.get_mut(key).and_then(Value::as_mapping_mut)
    {
        let mut keys = Vec::new();
        for map in [old_map, expected_map].into_iter().flatten() {
            for key in map.keys() {
                if !keys.contains(key) {
                    keys.push(key.clone());
                }
            }
        }
        let mut conflict = false;
        for nested in keys {
            conflict |= restore_field_delta(
                current_map,
                &nested,
                old_map.and_then(|map| map.get(&nested)),
                expected_map.and_then(|map| map.get(&nested)),
            );
        }
        if current_map.is_empty() && null_or_missing(old) {
            match old {
                Some(old) => {
                    current.insert(key.clone(), old.clone());
                }
                None => {
                    current.remove(key);
                }
            }
        }
        return conflict;
    }
    let actual = current.get(key);
    if actual != expected && !(null_or_missing(actual) && null_or_missing(expected)) {
        return true;
    }
    match old {
        Some(old) => {
            current.insert(key.clone(), old.clone());
        }
        None => {
            current.remove(key);
        }
    }
    false
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
pub(crate) mod tests {
    use clash_verge_core::config::{IProfiles, PrfItem};
    use clash_verge_core::utils::dirs;

    use super::*;
    use crate::subscribe::from_url::RemoteProfileBundle;

    /// Serializes tests that claim the process-global `APP_HOME_DIR` OnceLock.
    /// Disk-backed tests run one at a time and share one root so the OnceLock
    /// can only ever be claimed once per process. Async-aware because the guard
    /// is held across `await` points for the whole test body.
    pub(crate) static TEST_APP_HOME_DIR_LOCK: std::sync::LazyLock<tokio::sync::Mutex<()>> =
        std::sync::LazyLock::new(|| tokio::sync::Mutex::new(()));

    /// The single test app-home root shared by every disk-backed test.
    pub(crate) fn test_app_home_root() -> std::path::PathBuf {
        static ROOT: std::sync::LazyLock<tempfile::TempDir> =
            std::sync::LazyLock::new(|| tempfile::tempdir().expect("isolated test app home"));
        ROOT.path().to_path_buf()
    }

    /// Overrides the process-global app home under `TEST_APP_HOME_DIR_LOCK` via
    /// the core crate's re-settable test seam so the canonical test root wins
    /// regardless of which test ran first. A test that bypasses this lock (e.g.
    /// the settings row-5 test calling `set_app_home_dir` directly with its own
    /// tempdir) can otherwise leave `app_home_dir()` pointing at a path the
    /// claiming test never wrote to, surfacing as "profile X not found after
    /// refresh" inside the dispatcher tests that go through
    /// `reload_current_profile_for_core` / `switch_profile_for_core`.
    pub(crate) async fn claim_test_app_home(root: std::path::PathBuf) -> tokio::sync::MutexGuard<'static, ()> {
        let guard = TEST_APP_HOME_DIR_LOCK.lock().await;
        let _ = std::fs::create_dir_all(root.join("profiles"));
        dirs::set_app_home_dir_for_tests(root);
        guard
    }

    /// In-memory store for disk-backed tests that must seed a profile without
    /// hitting the network (the `profiles` field is private to this module).
    pub(crate) fn empty_store() -> ProfileStore {
        ProfileStore {
            profiles: IProfiles::default(),
        }
    }

    #[tokio::test]
    async fn failed_first_switch_restores_unselected_state_without_overwriting_a_later_switch() {
        let root = test_app_home_root();
        let _guard = claim_test_app_home(root.clone()).await;
        let profiles = IProfiles {
            current: Some("failed".into()),
            items: Some(vec![
                PrfItem {
                    uid: Some("failed".into()),
                    ..Default::default()
                },
                PrfItem {
                    uid: Some("later".into()),
                    ..Default::default()
                },
            ]),
            ..Default::default()
        };
        profiles.save_file().await.unwrap();
        ProfileStore::restore_current_if_matches("failed", None).await.unwrap();
        assert!(ProfileStore::snapshot().await.unwrap().current_uid().is_none());
        ProfileStore::replace_current_locked("later").await.unwrap();
        ProfileStore::restore_current_if_matches("failed", None).await.unwrap();
        assert_eq!(
            ProfileStore::snapshot().await.unwrap().current_uid().as_deref(),
            Some("later")
        );
    }

    #[tokio::test]
    async fn append_bundle_persists_profiles_yaml_and_body() {
        let root = crate::profile_store::store::tests::test_app_home_root();
        let _dir_guard = claim_test_app_home(root.clone()).await;

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
            fragments: vec![match PrfItem::from_merge(None) {
                Ok(item) => item,
                Err(error) => panic!("merge fragment: {error}"),
            }],
        };

        match store.append_bundle(bundle).await {
            Ok(_) => {}
            Err(error) => panic!("append_bundle: {error}"),
        }

        let profiles_yaml = match std::fs::read_to_string(root.join("profiles.yaml")) {
            Ok(text) => text,
            Err(error) => panic!("read profiles.yaml: {error}"),
        };
        assert!(profiles_yaml.contains(uid));
        assert!(profiles_yaml.contains("persist-demo"));
        let body = match std::fs::read_to_string(root.join("profiles").join(&file_name)) {
            Ok(text) => text,
            Err(error) => panic!("read profile body: {error}"),
        };
        assert!(body.contains("proxies:"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn issue83_unselected_import_preserves_selection_and_unknown_fields_even_on_save_failure() {
        let home = tempfile::tempdir().unwrap();
        let _home = claim_test_app_home(home.path().to_path_buf()).await;
        let bundle = |uid: &str| RemoteProfileBundle {
            item: PrfItem {
                uid: Some(uid.into()),
                itype: Some("remote".into()),
                file: Some(format!("{uid}.yaml").into()),
                file_data: Some("proxies: []\n".into()),
                ..Default::default()
            },
            fragments: vec![PrfItem::from_merge(None).unwrap()],
        };
        let mut store = ProfileStore {
            profiles: serde_yaml_ng::from_str("future: {keep: true}\n").unwrap(),
        };
        store
            .append_bundle_with_selection(bundle("first"), false)
            .await
            .unwrap();
        assert!(store.current_uid().is_none());
        assert!(ProfileStore::snapshot().await.unwrap().current_uid().is_none());
        let saved: serde_yaml_ng::Value =
            serde_yaml_ng::from_slice(&std::fs::read(home.path().join("profiles.yaml")).unwrap()).unwrap();
        assert_eq!(saved["future"]["keep"], serde_yaml_ng::Value::Bool(true));
        store.set_current("first").await.unwrap();
        store
            .append_bundle_with_selection(bundle("second"), false)
            .await
            .unwrap();
        assert_eq!(
            ProfileStore::snapshot().await.unwrap().current_uid().as_deref(),
            Some("first")
        );
        std::fs::remove_file(home.path().join("profiles.yaml")).unwrap();
        std::fs::create_dir(home.path().join("profiles.yaml")).unwrap();
        for previous in [None, Some("first".into())] {
            let mut profiles = IProfiles {
                current: previous.clone(),
                ..Default::default()
            };
            let mut item = bundle("failed").item;
            assert!(profiles.append_item_with_selection(&mut item, false).await.is_err());
            assert_eq!(
                profiles.current, previous,
                "no transient current mutation before failed metadata write"
            );
        }
    }

    #[tokio::test]
    async fn add_trusted_host_persists_merged_allowlist_into_the_profiles_option() {
        let root = crate::profile_store::store::tests::test_app_home_root();
        let _dir_guard = claim_test_app_home(root.clone()).await;

        let mut store = ProfileStore {
            profiles: IProfiles::default(),
        };
        let uid = "Rtrusted01ab";
        let bundle = RemoteProfileBundle {
            item: PrfItem {
                uid: Some(uid.into()),
                itype: Some("remote".into()),
                name: Some("trust-demo".into()),
                file: Some(format!("{uid}.yaml").into()),
                url: Some("https://8ry1xfih.doggygosubs.com/sub".into()),
                option: Some(clash_verge_core::config::PrfOption {
                    trusted_hosts: Some(vec!["existing.example.org".into()]),
                    ..Default::default()
                }),
                file_data: Some("proxies: []\n".into()),
                ..Default::default()
            },
            fragments: vec![match PrfItem::from_merge(None) {
                Ok(item) => item,
                Err(error) => panic!("merge fragment: {error}"),
            }],
        };
        match store.append_bundle(bundle).await {
            Ok(_) => {}
            Err(error) => panic!("append_bundle: {error}"),
        }

        match ProfileStore::add_trusted_host_locked(uid, "8ry1xfih.doggygosubs.com").await {
            Ok(()) => {}
            Err(error) => panic!("add_trusted_host_locked: {error}"),
        }

        // The stored option now merges the new host WITHOUT dropping the one
        // the profile already trusted (the update trust flow must merge).
        let snapshot = match ProfileStore::snapshot().await {
            Ok(store) => store,
            Err(error) => panic!("snapshot: {error}"),
        };
        let item = snapshot
            .items()
            .into_iter()
            .find(|item| item.uid.as_deref() == Some(uid))
            .unwrap_or_else(|| panic!("profile {uid} missing after persist"));
        let hosts = item
            .option
            .and_then(|option| option.trusted_hosts)
            .expect("trusted_hosts persisted");
        let expected: Vec<CompactString> = vec!["existing.example.org".into(), "8ry1xfih.doggygosubs.com".into()];
        assert_eq!(hosts, expected);

        // A repeated confirm for the same host is idempotent: no duplicate entry.
        match ProfileStore::add_trusted_host_locked(uid, "8ry1xfih.doggygosubs.com").await {
            Ok(()) => {}
            Err(error) => panic!("second add_trusted_host_locked: {error}"),
        }
        let snapshot = match ProfileStore::snapshot().await {
            Ok(store) => store,
            Err(error) => panic!("snapshot: {error}"),
        };
        let item = snapshot
            .items()
            .into_iter()
            .find(|item| item.uid.as_deref() == Some(uid))
            .unwrap_or_else(|| panic!("profile {uid} missing after second persist"));
        let hosts = item
            .option
            .and_then(|option| option.trusted_hosts)
            .expect("trusted_hosts persisted");
        assert_eq!(hosts.len(), 2, "duplicate confirm must not grow the allowlist");

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
                ..Default::default()
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
                    PrfItem {
                        uid: Some("local-base".into()),
                        itype: Some("local".into()),
                        ..Default::default()
                    },
                ]),
                ..Default::default()
            },
        };

        let items = store.items();
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].uid.as_deref(), Some("remote-a"));
        assert_eq!(items[1].uid.as_deref(), Some("remote-b"));
        assert_eq!(items[2].uid.as_deref(), Some("local-base"));
        assert_eq!(store.selected_index(), 1);
    }
}
