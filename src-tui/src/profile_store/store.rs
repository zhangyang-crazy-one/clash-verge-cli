// Profile store — thin wrapper around IProfiles for TUI use.

use anyhow::Context;
use clash_verge_core::config::{IProfiles, PrfItem};

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

    /// Append a new profile item and persist to disk.
    pub async fn append(&mut self, mut item: PrfItem) -> anyhow::Result<()> {
        self.profiles
            .append_item(&mut item)
            .await
            .context("failed to append profile")
    }
}

#[cfg(test)]
mod tests {
    use clash_verge_core::config::{IProfiles, PrfItem};

    use super::*;

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
