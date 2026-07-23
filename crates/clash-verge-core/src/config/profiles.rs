use super::prfitem::PrfItem;
use crate::utils::{dirs, help};
use anyhow::{Context as _, Result, bail};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_yaml_ng::Mapping;
use smartstring::alias::String;
use std::collections::HashSet;
use std::sync::OnceLock;
use tokio::fs;

/// Regex to check profile file names, eg.
/// R12345678.yaml (remote)
/// L12345678.yaml (local)
/// m12345678.yaml (merge)
/// s12345678.js (script)
/// r12345678.yaml (rules)
/// p12345678.yaml (proxies)
/// g12345678.yaml (groups)
static REGEX_PROFILE_FILE: OnceLock<Regex> = OnceLock::new();

fn profile_file_regex() -> &'static Regex {
    REGEX_PROFILE_FILE.get_or_init(|| {
        // Allowed to unwrap here: pattern is a constant literal.
        #[allow(clippy::unwrap_used)]
        Regex::new(r"^(?:[RLmrpg][a-zA-Z0-9]+\.yaml|s[a-zA-Z0-9]+\.js)$").unwrap()
    })
}

/// Define the `profiles.yaml` schema
#[derive(Default, Debug, Clone, Deserialize, Serialize)]
pub struct IProfiles {
    /// same as PrfConfig.current
    pub current: Option<String>,

    /// profile list
    pub items: Option<Vec<PrfItem>>,
}

pub struct IProfilePreview<'a> {
    pub uid: &'a String,
    pub name: &'a String,
    pub is_current: bool,
}

/// Cleanup result
#[derive(Debug, Clone)]
pub struct CleanupResult {
    pub total_files: usize,
    pub deleted_files: usize,
    pub failed_deletions: usize,
}

impl IProfiles {
    pub async fn new() -> Self {
        let path = match dirs::profiles_path() {
            Ok(p) => p,
            Err(_) => return Self::default(),
        };

        match help::read_yaml::<Self>(&path).await {
            Ok(mut profiles) => {
                let items = profiles.items.get_or_insert_with(Vec::new);
                for item in items.iter_mut() {
                    if item.uid.is_none() {
                        item.uid = Some(help::get_uid("d").into());
                    }
                }
                profiles
            }
            Err(_) => Self::default(),
        }
    }

    pub async fn save_file(&self) -> Result<()> {
        help::save_yaml(&dirs::profiles_path()?, self, Some("# Profiles Config for Clash Verge")).await
    }

    /// Only modify current field
    pub fn patch_config(&mut self, patch: &Self) {
        if self.items.is_none() {
            self.items = Some(vec![]);
        }

        if let Some(current) = &patch.current
            && let Some(items) = self.items.as_ref()
        {
            let some_uid = Some(current);
            if items.iter().any(|e| e.uid.as_ref() == some_uid) {
                self.current = some_uid.cloned();
            }
        }
    }

    pub const fn get_current(&self) -> Option<&String> {
        self.current.as_ref()
    }

    /// get items ref
    pub const fn get_items(&self) -> Option<&Vec<PrfItem>> {
        self.items.as_ref()
    }

    /// find the item by the uid
    pub fn get_item(&self, uid: impl AsRef<str>) -> Result<&PrfItem> {
        let uid_str = uid.as_ref();

        if let Some(items) = self.items.as_ref() {
            for each in items.iter() {
                if let Some(uid_val) = &each.uid
                    && uid_val.as_str() == uid_str
                {
                    return Ok(each);
                }
            }
        }

        bail!("failed to get the profile item \"uid:{}\"", uid_str);
    }

    /// append new item
    pub async fn append_item(&mut self, item: &mut PrfItem) -> Result<()> {
        let uid = &item.uid;
        if uid.is_none() {
            bail!("the uid should not be null");
        }

        // save the file data
        // move the field value after save
        if let Some(file_data) = item.file_data.take() {
            if item.file.is_none() {
                bail!("the file should not be null");
            }

            let file = item
                .file
                .clone()
                .ok_or_else(|| anyhow::anyhow!("file field is required when file_data is provided"))?;
            let path = dirs::app_profiles_dir()?.join(file.as_str());

            fs::write(&path, file_data.as_bytes())
                .await
                .with_context(|| format!("failed to write to file \"{file}\""))?;
        }

        if self.current.is_none() && (item.itype == Some("remote".into()) || item.itype == Some("local".into())) {
            self.current = uid.to_owned();
        }

        if self.items.is_none() {
            self.items = Some(vec![]);
        }

        if let Some(items) = self.items.as_mut() {
            items.push(item.to_owned());
        }

        self.save_file().await?;

        Ok(())
    }

    /// update the item value
    pub async fn patch_item(&mut self, uid: &String, item: &PrfItem) -> Result<()> {
        let mut items = self.items.take().unwrap_or_default();

        for each in items.iter_mut() {
            if each.uid.as_ref() == Some(uid) {
                if let Some(itype) = &item.itype {
                    each.itype = Some(itype.clone());
                }
                if let Some(name) = &item.name {
                    each.name = Some(name.clone());
                }
                if let Some(desc) = &item.desc {
                    each.desc = Some(desc.clone());
                }
                if let Some(file) = &item.file {
                    each.file = Some(file.clone());
                }
                if let Some(url) = &item.url {
                    each.url = Some(url.clone());
                }
                if let Some(selected) = &item.selected {
                    each.selected = Some(selected.clone());
                }
                if let Some(extra) = &item.extra {
                    each.extra = Some(*extra);
                }
                if let Some(updated) = &item.updated {
                    each.updated = Some(*updated);
                }
                if let Some(option) = &item.option {
                    each.option = Some(option.clone());
                }

                self.items = Some(items);
                return self.save_file().await;
            }
        }

        self.items = Some(items);
        bail!("failed to find the profile item \"uid:{uid}\"")
    }

    /// be used to update the remote item
    /// only patch `updated` `extra` `file_data`
    pub async fn update_item(&mut self, uid: &String, item: &mut PrfItem) -> Result<()> {
        if self.items.is_none() {
            self.items = Some(vec![]);
        }

        // find the item
        let _ = self.get_item(uid)?;

        if let Some(items) = self.items.as_mut() {
            let some_uid = Some(uid.clone());

            for each in items.iter_mut() {
                if each.uid == some_uid {
                    each.extra = item.extra;
                    each.updated = item.updated;
                    each.home = item.home.to_owned();
                    each.option = PrfOption::merge(each.option.as_ref(), item.option.as_ref());
                    // save the file data
                    if let Some(file_data) = item.file_data.take() {
                        let file = each.file.take();
                        let file =
                            file.unwrap_or_else(|| item.file.take().unwrap_or_else(|| format!("{}.yaml", uid).into()));

                        each.file = Some(file.clone());

                        let path = dirs::app_profiles_dir()?.join(file.as_str());

                        fs::write(&path, file_data.as_bytes())
                            .await
                            .with_context(|| format!("failed to write to file \"{file}\""))?;
                    }

                    break;
                }
            }
        }

        self.save_file().await
    }

    /// delete item
    pub async fn delete_item(&mut self, uid: &String) -> Result<bool> {
        let current = self.current.as_ref().unwrap_or(uid);
        let current = current.clone();
        let delete_uids = {
            let item = self.get_item(uid)?;
            let option = item.option.as_ref();
            option.map_or(Vec::new(), |op| {
                [
                    op.merge.clone(),
                    op.script.clone(),
                    op.rules.clone(),
                    op.proxies.clone(),
                    op.groups.clone(),
                ]
                .into_iter()
                .collect::<Vec<_>>()
            })
        };
        let mut items = self.items.take().unwrap_or_default();

        // remove the main item (if exists) and delete its file
        if let Some(file) = Self::take_item_file_by_uid(&mut items, Some(uid.as_str())) {
            let _ = dirs::app_profiles_dir()?.join(file.as_str());
            let _ = tokio::fs::remove_file(dirs::app_profiles_dir()?.join(file.as_str())).await;
        }

        for delete_uid in delete_uids {
            if let Some(file) = Self::take_item_file_by_uid(&mut items, delete_uid.as_deref()) {
                let _ = tokio::fs::remove_file(dirs::app_profiles_dir()?.join(file.as_str())).await;
            }
        }

        // delete the original uid
        if current == *uid {
            self.current = None;
            for item in items.iter() {
                if item.itype == Some("remote".into()) || item.itype == Some("local".into()) {
                    self.current = item.uid.clone();
                    break;
                }
            }
        }

        self.items = Some(items);
        self.save_file().await?;
        Ok(current == *uid)
    }

    // Helper to find and remove an item by uid from the items vec, returning its file name (if any).
    fn take_item_file_by_uid(items: &mut Vec<PrfItem>, target_uid: Option<&str>) -> Option<String> {
        let index = items.iter().position(|item| item.uid.as_deref() == target_uid)?;
        items.remove(index).file
    }

    /// 获取current指向的订阅内容
    pub async fn current_mapping(&self) -> Result<Mapping> {
        match (self.current.as_ref(), self.items.as_ref()) {
            (Some(current), Some(items)) => {
                if let Some(item) = items.iter().find(|e| e.uid.as_ref() == Some(current)) {
                    let file_path = match item.file.as_ref() {
                        Some(file) => dirs::app_profiles_dir()?.join(file.as_str()),
                        None => bail!("failed to get the file field"),
                    };
                    return help::read_mapping(&file_path).await;
                }
                bail!("failed to find the current profile \"uid:{current}\"");
            }
            _ => Ok(Mapping::new()),
        }
    }

    /// 判断profile是否是current指向的
    pub fn is_current_profile_index(&self, index: &String) -> bool {
        self.current.as_ref() == Some(index)
    }

    /// 获取所有的profiles(uid，名称, 是否为 current)
    pub fn profiles_preview(&self) -> Option<Vec<IProfilePreview<'_>>> {
        self.items.as_ref().map(|items| {
            items
                .iter()
                .filter_map(|e| {
                    if let (Some(uid), Some(name)) = (e.uid.as_ref(), e.name.as_ref()) {
                        let is_current = self.is_current_profile_index(uid);
                        let preview = IProfilePreview { uid, name, is_current };
                        Some(preview)
                    } else {
                        None
                    }
                })
                .collect()
        })
    }

    /// 通过 uid 获取名称
    pub fn get_name_by_uid(&self, uid: &String) -> Option<&String> {
        if let Some(items) = &self.items {
            for item in items {
                if item.uid.as_ref() == Some(uid) {
                    return item.name.as_ref();
                }
            }
        }
        None
    }

    /// 以 app 中的 profile 列表为准，删除不再需要的文件
    pub async fn cleanup_orphaned_files(&self) -> Result<()> {
        let profiles_dir = dirs::app_profiles_dir()?;

        if !profiles_dir.exists() {
            return Ok(());
        }

        let active_files = self.get_all_active_files();
        let protected_files = self.get_protected_global_files();

        let mut total_files = 0;
        let mut deleted_files = 0;
        let mut failed_deletions = 0;

        let mut dir_entries = tokio::fs::read_dir(&profiles_dir).await?;
        while let Some(entry) = dir_entries.next_entry().await? {
            let path = entry.path();

            if !path.is_file() {
                continue;
            }

            total_files += 1;

            if let Some(file_name) = path.file_name().and_then(|n| n.to_str())
                && Self::is_profile_file(file_name)
            {
                if protected_files.contains(file_name) {
                    continue;
                }

                if !active_files.contains(file_name) {
                    match tokio::fs::remove_file(&path).await {
                        Ok(_) => {
                            deleted_files += 1;
                        }
                        Err(_) => {
                            failed_deletions += 1;
                        }
                    }
                }
            }
        }

        let result = CleanupResult {
            total_files,
            deleted_files,
            failed_deletions,
        };

        let _ = result;

        Ok(())
    }

    fn get_protected_global_files(&self) -> HashSet<String> {
        let mut protected_files = HashSet::new();
        protected_files.insert("Merge.yaml".into());
        protected_files.insert("Script.js".into());
        protected_files
    }

    fn get_all_active_files(&self) -> HashSet<&str> {
        let mut active_files: HashSet<&str> = HashSet::new();

        if let Some(items) = &self.items {
            for item in items {
                if let Some(file) = &item.file {
                    active_files.insert(file);
                }
            }
        }

        active_files
    }

    fn is_profile_file(filename: &str) -> bool {
        profile_file_regex().is_match(filename)
    }
}

// Re-export PrfOption for callers that referenced it via `super::PrfOption`.
pub use super::prfitem::PrfOption;
