use super::prfitem::{PrfItem, merge_unknown_fields};
use crate::utils::{dirs, help};
use anyhow::{Context as _, Result, bail};
use compact_str::CompactString as String;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_yaml_ng::Mapping;
use std::collections::HashSet;
use std::path::{Component, Path};
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

    /// Preserve profile metadata written by newer GUI versions.
    #[serde(flatten)]
    pub extra: Mapping,
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
    pub async fn new() -> Result<Self> {
        let path = dirs::profiles_path()?;
        let mut profiles = match fs::metadata(&path).await {
            Ok(_) => help::read_yaml::<Self>(&path)
                .await
                .with_context(|| format!("cannot load profile config {}", path.display()))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(error) => return Err(error).with_context(|| format!("cannot read {}", path.display())),
        };
        let items = profiles.items.get_or_insert_with(Vec::new);
        for item in items.iter_mut() {
            if item.uid.is_none() {
                item.uid = Some(help::get_uid("d").into());
            }
        }
        Ok(profiles)
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

        if let Some(file) = &item.file {
            Self::validate_profile_file(file)?;
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
        if let Some(file) = &item.file {
            Self::validate_profile_file(file)?;
        }

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
                    let mut updated = extra.clone();
                    if let Some(current) = &each.extra {
                        let mut unknown = current.unknown_fields.clone();
                        merge_unknown_fields(&mut unknown, &updated.unknown_fields);
                        updated.unknown_fields = unknown;
                    }
                    each.extra = Some(updated);
                }
                if let Some(updated) = &item.updated {
                    each.updated = Some(*updated);
                }
                if let Some(option) = &item.option {
                    each.option = PrfOption::merge(each.option.as_ref(), Some(option));
                }
                merge_unknown_fields(&mut each.unknown_fields, &item.unknown_fields);

                self.items = Some(items);
                return self.save_file().await;
            }
        }

        self.items = Some(items);
        bail!("failed to find the profile item \"uid:{uid}\"")
    }

    fn validate_profile_file(file: &str) -> Result<()> {
        let mut components = Path::new(file).components();
        if file.is_empty()
            || file.contains('/')
            || file.contains('\\')
            || !matches!(
                (components.next(), components.next()),
                (Some(Component::Normal(_)), None)
            )
        {
            bail!("profile file must be a single filename");
        }

        Ok(())
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
                    if let Some(mut updated) = item.extra.clone() {
                        if let Some(current) = &each.extra {
                            let mut unknown = current.unknown_fields.clone();
                            merge_unknown_fields(&mut unknown, &updated.unknown_fields);
                            updated.unknown_fields = unknown;
                        }
                        each.extra = Some(updated);
                    } else {
                        each.extra = None;
                    }
                    each.updated = item.updated;
                    each.home = item.home.to_owned();
                    each.option = PrfOption::merge(each.option.as_ref(), item.option.as_ref());
                    // save the file data
                    if let Some(file_data) = item.file_data.take() {
                        let file = each.file.take();
                        let file =
                            file.unwrap_or_else(|| item.file.take().unwrap_or_else(|| format!("{}.yaml", uid).into()));

                        Self::validate_profile_file(file.as_str())?;
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

#[cfg(test)]
mod lossless_config_tests {
    use super::IProfiles;
    use serde_yaml_ng::Value;

    #[test]
    fn typed_profile_edits_keep_unknown_root_and_nested_yaml() {
        let source = r#"
future_root:
  mode: strict
  sequence: [one, two]
current: R1
items:
  - uid: R1
    type: remote
    name: old
    future_item:
      nested: [kept]
    selected:
      - name: Auto
        now: Tokyo
        future_selection: true
    extra:
      upload: 1
      download: 2
      total: 3
      expire: 4
      future_usage: [kept]
    option:
      user_agent: old-agent
      future_option:
        retry: 5
"#;
        let mut profiles: IProfiles = serde_yaml_ng::from_str(source).unwrap();
        let item = profiles.items.as_mut().unwrap().first_mut().unwrap();
        item.name = Some("edited".into());
        item.option.as_mut().unwrap().user_agent = Some("new-agent".into());

        let output = serde_yaml_ng::to_string(&profiles).unwrap();
        let actual: Value = serde_yaml_ng::from_str(&output).unwrap();
        let original: Value = serde_yaml_ng::from_str(source).unwrap();
        assert_eq!(actual["future_root"], original["future_root"]);
        assert_eq!(actual["items"][0]["future_item"], original["items"][0]["future_item"]);
        assert_eq!(
            actual["items"][0]["selected"][0]["future_selection"],
            original["items"][0]["selected"][0]["future_selection"]
        );
        assert_eq!(
            actual["items"][0]["extra"]["future_usage"],
            original["items"][0]["extra"]["future_usage"]
        );
        assert_eq!(
            actual["items"][0]["option"]["future_option"],
            original["items"][0]["option"]["future_option"]
        );
        assert_eq!(actual["items"][0]["name"], Value::from("edited"));
        assert_eq!(actual["items"][0]["option"]["user_agent"], Value::from("new-agent"));
    }

    #[test]
    fn profile_option_merge_keeps_base_unknowns_and_applies_override_unknowns() {
        let base: super::PrfOption =
            serde_yaml_ng::from_str("user_agent: base\nfuture: {base: true, shared: base}\n").unwrap();
        let overlay: super::PrfOption =
            serde_yaml_ng::from_str("timeout_seconds: 12\nfuture: {shared: overlay, added: true}\n").unwrap();
        let merged = super::PrfOption::merge(Some(&base), Some(&overlay)).unwrap();
        let value: Value = serde_yaml_ng::to_value(merged).unwrap();
        assert_eq!(value["user_agent"], Value::from("base"));
        assert_eq!(value["timeout_seconds"], Value::from(12));
        assert_eq!(value["future"]["base"], Value::from(true));
        assert_eq!(value["future"]["shared"], Value::from("overlay"));
        assert_eq!(value["future"]["added"], Value::from(true));
    }
}
