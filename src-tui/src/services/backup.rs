//! Backups of profiles and settings, as zip archives laid out like the
//! configuration directory (and like Clash Verge GUI backups):
//!
//! ```text
//! config.yaml  verge.yaml  profiles.yaml  dns_config.yaml  profiles/<file>
//! ```
//!
//! Secrets stay out by default: the controller `secret` in `config.yaml` and
//! the WebDAV credentials in `verge.yaml`. Subscription URLs are part of the
//! profiles, so an archive is still private data; it is written mode 0600.
//! Restoring keeps the local secrets an archive does not carry and first
//! saves the current state as a `pre-restore-*` backup.

use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};

use anyhow::{Context as _, bail};
use serde::Serialize;
use serde_yaml_ng::{Mapping, Value};

/// Top-level files a backup holds, in the configuration directory.
const FILES: [&str; 4] = ["config.yaml", "verge.yaml", "profiles.yaml", "dns_config.yaml"];
const PROFILES_DIR: &str = "profiles";
/// Keys left out without `--include-secrets`, per file.
const SECRETS: [(&str, &[&str]); 2] = [
    ("config.yaml", &["secret"]),
    ("verge.yaml", &["webdav_username", "webdav_password"]),
];
/// Refuse archives that would unpack to more than this.
const MAX_UNPACKED_BYTES: u64 = 256 * 1024 * 1024;

/// `linux-backup-2026-09-23_12-00-00.zip`, as the GUI names them.
pub fn default_name(now: chrono::DateTime<chrono::Local>) -> String {
    format!(
        "{}-backup-{}.zip",
        std::env::consts::OS,
        now.format("%Y-%m-%d_%H-%M-%S")
    )
}

#[derive(Debug, Serialize)]
pub struct Created {
    pub path: PathBuf,
    pub files: usize,
    pub bytes: u64,
}

/// Archive the configuration in `home` to `dest`.
pub fn create(home: &Path, dest: &Path, include_secrets: bool) -> anyhow::Result<Created> {
    let mut entries: Vec<(String, Vec<u8>)> = Vec::new();
    for name in FILES {
        let path = home.join(name);
        if !path.is_file() {
            continue;
        }
        let data = std::fs::read(&path).with_context(|| format!("cannot read {}", path.display()))?;
        let data = if include_secrets {
            data
        } else {
            strip_secrets(name, &data).with_context(|| format!("cannot parse {}", path.display()))?
        };
        entries.push((name.to_string(), data));
    }
    if !entries.iter().any(|(name, _)| name == "profiles.yaml") {
        bail!("no profiles.yaml in {}: nothing to back up", home.display());
    }
    let profiles = home.join(PROFILES_DIR);
    if profiles.is_dir() {
        let mut names: Vec<_> = std::fs::read_dir(&profiles)?
            .filter_map(Result::ok)
            // Regular files only: no symlinks out of the directory.
            .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
            .filter_map(|entry| entry.file_name().into_string().ok())
            .collect();
        names.sort();
        for name in names {
            let data = std::fs::read(profiles.join(&name))?;
            entries.push((format!("{PROFILES_DIR}/{name}"), data));
        }
    }

    if let Some(parent) = dest.parent().filter(|parent| !parent.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    let partial = dest.with_extension("zip.partial");
    let written = write_zip(&partial, &entries);
    if let Err(error) = written {
        let _ = std::fs::remove_file(&partial);
        return Err(error);
    }
    std::fs::rename(&partial, dest)?;
    Ok(Created {
        path: dest.to_path_buf(),
        files: entries.len(),
        bytes: std::fs::metadata(dest)?.len(),
    })
}

fn write_zip(path: &Path, entries: &[(String, Vec<u8>)]) -> anyhow::Result<()> {
    let file = private_file(path)?;
    let mut zip = zip::ZipWriter::new(file);
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .last_modified_time(zip_now());
    for (name, data) in entries {
        zip.start_file(name.as_str(), options)?;
        zip.write_all(data)?;
    }
    zip.finish()?.sync_all()?;
    Ok(())
}

/// The local time as a zip timestamp (the format has no time zone).
fn zip_now() -> zip::DateTime {
    use chrono::{Datelike as _, Timelike as _};
    let now = chrono::Local::now();
    zip::DateTime::from_date_and_time(
        u16::try_from(now.year()).unwrap_or(1980),
        u8::try_from(now.month()).unwrap_or(1),
        u8::try_from(now.day()).unwrap_or(1),
        u8::try_from(now.hour()).unwrap_or(0),
        u8::try_from(now.minute()).unwrap_or(0),
        u8::try_from(now.second()).unwrap_or(0),
    )
    .unwrap_or_default()
}

/// Write `data` to `path` readable only by this user (backups hold
/// subscription URLs).
pub fn write_private(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let mut file = private_file(path)?;
    file.write_all(data)?;
    file.sync_all()
}

/// A new file only this user can read.
fn private_file(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt as _;
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
}

/// `data` without the secret keys of `name` (unchanged for other files).
fn strip_secrets(name: &str, data: &[u8]) -> anyhow::Result<Vec<u8>> {
    let Some((_, keys)) = SECRETS.iter().find(|(file, _)| *file == name) else {
        return Ok(data.to_vec());
    };
    let mut mapping: Mapping = serde_yaml_ng::from_slice(data)?;
    let mut changed = false;
    for key in *keys {
        changed |= mapping.remove(*key).is_some();
    }
    Ok(if changed {
        serde_yaml_ng::to_string(&mapping)?.into_bytes()
    } else {
        data.to_vec()
    })
}

#[derive(Debug, Serialize)]
pub struct Entry {
    pub name: String,
    pub bytes: u64,
    /// Unix seconds.
    pub modified: Option<i64>,
}

/// Backups in `dir`, newest first.
pub fn list(dir: &Path) -> anyhow::Result<Vec<Entry>> {
    let mut entries = Vec::new();
    let read = match std::fs::read_dir(dir) {
        Ok(read) => read,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(entries),
        Err(error) => return Err(error.into()),
    };
    for entry in read.filter_map(Result::ok) {
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if !metadata.is_file() || !name.ends_with(".zip") {
            continue;
        }
        let modified = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .and_then(|since| i64::try_from(since.as_secs()).ok());
        entries.push(Entry {
            name,
            bytes: metadata.len(),
            modified,
        });
    }
    entries.sort_by(|left, right| right.modified.cmp(&left.modified).then(right.name.cmp(&left.name)));
    Ok(entries)
}

/// Whether `name` is a plain backup file name (no path).
pub fn is_backup_name(name: &str) -> bool {
    name.ends_with(".zip")
        && !name.starts_with('.')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

#[derive(Debug, Serialize)]
pub struct Restored {
    /// Files written, relative to the configuration directory.
    pub files: Vec<String>,
    /// Archive entries that are not part of a backup and were left out.
    pub skipped: Vec<String>,
    /// The backup of the previous state; `None` when there was none (a
    /// fresh configuration directory).
    pub previous: Option<PathBuf>,
}

/// Restore `archive` into `home`, saving the current state to `backup_dir`
/// first. Nothing is written unless every file in the archive is valid.
pub fn restore(home: &Path, archive: &Path, backup_dir: &Path) -> anyhow::Result<Restored> {
    let file = std::fs::File::open(archive).with_context(|| format!("cannot open {}", archive.display()))?;
    let mut zip = zip::ZipArchive::new(file).with_context(|| format!("{} is not a zip archive", archive.display()))?;

    let mut files: Vec<(String, Vec<u8>)> = Vec::new();
    let mut skipped = Vec::new();
    let mut unpacked = 0_u64;
    for index in 0..zip.len() {
        let mut entry = zip.by_index(index)?;
        let name = entry.name().to_string();
        if entry.is_dir() {
            continue;
        }
        if !is_restorable(&name) {
            skipped.push(name);
            continue;
        }
        unpacked = unpacked.saturating_add(entry.size());
        if unpacked > MAX_UNPACKED_BYTES {
            bail!("{} unpacks to more than {MAX_UNPACKED_BYTES} bytes", archive.display());
        }
        let mut data = Vec::new();
        entry
            .by_ref()
            .take(MAX_UNPACKED_BYTES)
            .read_to_end(&mut data)
            .with_context(|| format!("cannot read {name} from the archive"))?;
        // Profile scripts (`.js`) are not YAML; everything else must parse.
        if name.ends_with(".yaml") || name.ends_with(".yml") {
            serde_yaml_ng::from_slice::<Value>(&data)
                .with_context(|| format!("{name} in the archive is not valid YAML"))?;
        }
        files.push((name, data));
    }
    if !files.iter().any(|(name, _)| name == "profiles.yaml") {
        bail!("{} holds no profiles.yaml: not a backup", archive.display());
    }

    // Keep what the archive left out on purpose.
    for (name, data) in &mut files {
        if let Some(merged) = keep_local_secrets(name, data, &home.join(name.as_str()))? {
            *data = merged;
        }
    }

    let previous = if home.join("profiles.yaml").is_file() {
        let path = backup_dir.join(format!("pre-restore-{}", default_name(chrono::Local::now())));
        create(home, &path, true).context("could not save the current state before restoring")?;
        Some(path)
    } else {
        None
    };

    let targets: Vec<(PathBuf, &[u8])> = files
        .iter()
        .map(|(name, data)| (home.join(name.as_str()), data.as_slice()))
        .collect();
    replace_all(&targets)?;
    Ok(Restored {
        files: files.into_iter().map(|(name, _)| name).collect(),
        skipped,
        previous,
    })
}

/// A top-level backup file or a plain file directly under `profiles/`.
fn is_restorable(name: &str) -> bool {
    if FILES.contains(&name) {
        return true;
    }
    name.strip_prefix("profiles/").is_some_and(|file| {
        !file.is_empty() && !file.contains(['/', '\\']) && file != "." && file != ".." && !file.starts_with('.')
    })
}

/// The archive's `data` for `name` with the local secret keys it lacks put
/// back; `None` when nothing needs merging.
fn keep_local_secrets(name: &str, data: &[u8], local: &Path) -> anyhow::Result<Option<Vec<u8>>> {
    let Some((_, keys)) = SECRETS.iter().find(|(file, _)| *file == name) else {
        return Ok(None);
    };
    let Ok(local) = std::fs::read(local) else {
        return Ok(None);
    };
    let Ok(local) = serde_yaml_ng::from_slice::<Mapping>(&local) else {
        return Ok(None);
    };
    let mut incoming: Mapping = serde_yaml_ng::from_slice(data)?;
    let mut changed = false;
    for key in *keys {
        if !incoming.contains_key(*key)
            && let Some(value) = local.get(*key)
        {
            incoming.insert(Value::from(*key), value.clone());
            changed = true;
        }
    }
    Ok(changed
        .then(|| serde_yaml_ng::to_string(&incoming).map(String::into_bytes))
        .transpose()?)
}

/// Replace every file in `targets` or none: all new contents are staged
/// first (mode 0600: they hold secrets and subscription URLs), then swapped
/// in; a failure part-way puts the originals back.
fn replace_all(targets: &[(PathBuf, &[u8])]) -> anyhow::Result<()> {
    let staged = |path: &Path| path.with_extension("restore.partial");
    let kept = |path: &Path| path.with_extension("restore.orig");

    let staging = targets.iter().try_for_each(|(path, data)| {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        write_private(&staged(path), data).with_context(|| format!("cannot write {}", staged(path).display()))
    });
    if let Err(error) = staging {
        for (path, _) in targets {
            let _ = std::fs::remove_file(staged(path));
        }
        return Err(error);
    }

    // (path, whether an original was moved aside)
    let mut swapped: Vec<(&Path, bool)> = Vec::new();
    let mut swap = || -> anyhow::Result<()> {
        for (path, _) in targets {
            let had_original = path.exists();
            if had_original {
                std::fs::rename(path, kept(path)).with_context(|| format!("cannot move {} aside", path.display()))?;
            }
            swapped.push((path, had_original));
            std::fs::rename(staged(path), path).with_context(|| format!("cannot replace {}", path.display()))?;
        }
        Ok(())
    };
    let result = swap();
    if result.is_err() {
        for (path, had_original) in swapped.iter().rev() {
            if *had_original {
                let _ = std::fs::rename(kept(path), path);
            } else {
                let _ = std::fs::remove_file(path);
            }
        }
    }
    for (path, _) in targets {
        let _ = std::fs::remove_file(staged(path));
        let _ = std::fs::remove_file(kept(path));
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cv-backup-{label}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn read(path: &Path) -> String {
        std::fs::read_to_string(path).unwrap()
    }

    fn sample_home(label: &str) -> PathBuf {
        let home = temp_dir(label);
        write(&home.join("config.yaml"), "mixed-port: 7897\nsecret: s3cret\n");
        write(
            &home.join("verge.yaml"),
            "language: zh\nwebdav_url: https://dav.example\nwebdav_username: me\nwebdav_password: pw\n",
        );
        write(
            &home.join("profiles.yaml"),
            "current: A\nitems:\n- uid: A\n  file: A.yaml\n",
        );
        write(&home.join("profiles/A.yaml"), "proxies: []\n");
        write(&home.join("cache.db"), "not backed up");
        home
    }

    fn archive_names(path: &Path) -> Vec<String> {
        let zip = zip::ZipArchive::new(std::fs::File::open(path).unwrap()).unwrap();
        let mut names: Vec<String> = zip.file_names().map(str::to_string).collect();
        names.sort();
        names
    }

    fn archive_file(path: &Path, name: &str) -> String {
        let mut zip = zip::ZipArchive::new(std::fs::File::open(path).unwrap()).unwrap();
        let mut text = String::new();
        zip.by_name(name).unwrap().read_to_string(&mut text).unwrap();
        text
    }

    #[test]
    fn backups_hold_the_config_files_without_secrets_by_default() {
        let home = sample_home("create");
        let dest = home.join("out/linux-backup-x.zip");
        let created = create(&home, &dest, false).unwrap();
        assert_eq!(created.files, 4);
        assert_eq!(
            archive_names(&dest),
            ["config.yaml", "profiles.yaml", "profiles/A.yaml", "verge.yaml"]
        );
        let config = archive_file(&dest, "config.yaml");
        assert!(config.contains("mixed-port") && !config.contains("s3cret"));
        let verge = archive_file(&dest, "verge.yaml");
        assert!(verge.contains("webdav_url") && !verge.contains("webdav_password") && !verge.contains("me"));
        use std::os::unix::fs::PermissionsExt as _;
        assert_eq!(std::fs::metadata(&dest).unwrap().permissions().mode() & 0o777, 0o600);

        create(&home, &dest, true).unwrap();
        assert!(archive_file(&dest, "config.yaml").contains("s3cret"));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn restore_brings_files_back_keeps_local_secrets_and_saves_the_old_state() {
        let home = sample_home("restore");
        let backups = home.join("backups");
        let archive = backups.join("b.zip");
        create(&home, &archive, false).unwrap();

        // Later changes, then restore.
        write(
            &home.join("verge.yaml"),
            "language: en\nwebdav_username: me\nwebdav_password: newpw\n",
        );
        write(&home.join("profiles/A.yaml"), "proxies: [changed]\n");
        write(&home.join("profiles.yaml"), "items: []\n");

        let restored = restore(&home, &archive, &backups).unwrap();
        assert_eq!(restored.files.len(), 4);
        assert!(read(&home.join("profiles.yaml")).contains("uid: A"));
        assert_eq!(read(&home.join("profiles/A.yaml")), "proxies: []\n");
        let verge = read(&home.join("verge.yaml"));
        assert!(verge.contains("language: zh"), "{verge}");
        assert!(verge.contains("newpw"), "local WebDAV password kept: {verge}");
        assert!(read(&home.join("config.yaml")).contains("s3cret"), "local secret kept");

        // The replaced state is saved, secrets included.
        let previous = restored.previous.unwrap();
        assert!(archive_file(&previous, "profiles/A.yaml").contains("changed"));
        assert!(archive_file(&previous, "verge.yaml").contains("newpw"));
        assert_eq!(list(&backups).unwrap().len(), 2);

        // A fresh directory (nothing to save first) restores too.
        let fresh = temp_dir("fresh");
        let restored = restore(&fresh, &archive, &backups).unwrap();
        assert!(restored.previous.is_none());
        assert!(read(&fresh.join("profiles.yaml")).contains("uid: A"));
        let _ = std::fs::remove_dir_all(&fresh);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn restore_rejects_bad_archives_before_writing_anything() {
        let home = sample_home("reject");
        let backups = home.join("backups");
        let before = read(&home.join("verge.yaml"));
        let make = |name: &str, entries: &[(&str, &str)]| {
            let path = backups.join(name);
            std::fs::create_dir_all(&backups).unwrap();
            let entries: Vec<(String, Vec<u8>)> = entries
                .iter()
                .map(|(name, text)| ((*name).to_string(), text.as_bytes().to_vec()))
                .collect();
            write_zip(&path, &entries).unwrap();
            path
        };

        let no_profiles = make("a.zip", &[("verge.yaml", "language: en\n")]);
        assert!(restore(&home, &no_profiles, &backups).is_err());
        let broken = make(
            "b.zip",
            &[
                ("profiles.yaml", "items: []\n"),
                ("verge.yaml", "language: [unclosed\n"),
            ],
        );
        assert!(restore(&home, &broken, &backups).is_err());
        assert_eq!(read(&home.join("verge.yaml")), before, "nothing written");

        // Paths outside the directory are skipped, never written.
        let sneaky = make(
            "c.zip",
            &[
                ("profiles.yaml", "items: []\n"),
                ("../escape.yaml", "x: 1\n"),
                ("profiles/../../escape2.yaml", "x: 1\n"),
                ("clash-verge.yaml", "x: 1\n"),
            ],
        );
        let restored = restore(&home, &sneaky, &backups).unwrap();
        assert_eq!(restored.files, ["profiles.yaml"]);
        assert_eq!(restored.skipped.len(), 3);
        assert!(!home.parent().unwrap().join("escape.yaml").exists());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn restored_files_are_private_and_profile_yaml_must_parse() {
        use std::os::unix::fs::PermissionsExt as _;
        let home = sample_home("private");
        let backups = home.join("backups");
        let archive = backups.join("b.zip");
        create(&home, &archive, false).unwrap();
        restore(&home, &archive, &backups).unwrap();
        for file in ["config.yaml", "verge.yaml", "profiles.yaml", "profiles/A.yaml"] {
            let mode = std::fs::metadata(home.join(file)).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "{file}");
        }
        assert!(!home.join("config.restore.orig").exists(), "no leftovers");

        let broken = backups.join("broken.zip");
        write_zip(
            &broken,
            &[
                ("profiles.yaml".to_string(), b"items: []\n".to_vec()),
                ("profiles/A.yaml".to_string(), b"proxies: [unclosed\n".to_vec()),
                ("profiles/s.js".to_string(), b"function main(c) { return c }".to_vec()),
            ],
        )
        .unwrap();
        let error = restore(&home, &broken, &backups).unwrap_err();
        assert!(format!("{error:#}").contains("profiles/A.yaml"), "{error:#}");
        assert_eq!(read(&home.join("profiles/A.yaml")), "proxies: []\n", "nothing written");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn a_failed_swap_puts_every_original_back() {
        let dir = temp_dir("swap");
        write(&dir.join("a.yaml"), "old a\n");
        write(&dir.join("b.yaml"), "old b\n");
        // A directory where b's original would be moved aside: that rename
        // fails (for any user), after a has already been swapped in.
        write(&dir.join("b.restore.orig/keep"), "x");
        let targets: Vec<(PathBuf, &[u8])> = vec![(dir.join("a.yaml"), b"new a\n"), (dir.join("b.yaml"), b"new b\n")];

        assert!(replace_all(&targets).is_err());
        assert_eq!(read(&dir.join("a.yaml")), "old a\n", "a rolled back");
        assert_eq!(read(&dir.join("b.yaml")), "old b\n");
        assert!(!dir.join("a.restore.orig").exists());
        assert!(!dir.join("a.restore.partial").exists() && !dir.join("b.restore.partial").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn names_and_listing() {
        let name = default_name(chrono::Local::now());
        assert!(name.starts_with("linux-backup-") && name.ends_with(".zip"));
        assert!(is_backup_name(&name));
        assert!(!is_backup_name("../x.zip"));
        assert!(!is_backup_name("x.tar"));
        assert!(list(Path::new("/nonexistent/backups")).unwrap().is_empty());
    }
}
