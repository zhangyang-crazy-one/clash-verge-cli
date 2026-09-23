//! `backup create|list|restore`, locally and over WebDAV.

use std::path::{Path, PathBuf};

use anyhow::Context as _;
use clash_verge_core::utils::dirs;

use crate::services::backup;
use crate::services::webdav::WebDav;

async fn webdav() -> anyhow::Result<WebDav> {
    WebDav::from_settings(&clash_verge_core::config::IVerge::new().await)
}

pub async fn create(output: Option<&Path>, include_secrets: bool, upload: bool, json: bool) -> anyhow::Result<()> {
    let home = dirs::app_home_dir()?;
    let dest = match output {
        Some(path) => path.to_path_buf(),
        None => dirs::local_backup_dir()?.join(backup::default_name(chrono::Local::now())),
    };
    let created = backup::create(&home, &dest, include_secrets)?;
    let name = dest
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_string();
    if upload {
        webdav().await?.upload(&created.path, &name).await?;
    }
    if json {
        println!(
            "{}",
            serde_json::json!({ "path": created.path, "files": created.files, "bytes": created.bytes, "uploaded": upload })
        );
    } else {
        println!(
            "backup written: {} ({} files, {})",
            created.path.display(),
            created.files,
            super::format_bytes(created.bytes)
        );
        if upload {
            println!("uploaded to WebDAV as {name}");
        }
        if !include_secrets {
            println!(
                "(secret and WebDAV credentials left out; subscription URLs are included — keep the file private)"
            );
        }
    }
    Ok(())
}

pub async fn list(remote: bool, json: bool) -> anyhow::Result<()> {
    let entries = if remote {
        webdav().await?.list().await?
    } else {
        backup::list(&dirs::local_backup_dir()?)?
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&entries)?);
    } else if entries.is_empty() {
        println!("(no backups)");
    } else {
        let rows = entries.iter().map(|entry| {
            vec![
                entry.name.clone(),
                super::format_bytes(entry.bytes),
                entry
                    .modified
                    .and_then(|secs| chrono::DateTime::from_timestamp(secs, 0))
                    .map(|time| time.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M").to_string())
                    .unwrap_or_else(|| "-".into()),
            ]
        });
        print!("{}", super::table(&["NAME", "SIZE", "MODIFIED"], rows));
    }
    Ok(())
}

pub async fn restore(backup_arg: &str, remote: bool, core_running: bool, json: bool) -> anyhow::Result<()> {
    let home = dirs::app_home_dir()?;
    let backup_dir = dirs::local_backup_dir()?;
    let archive: PathBuf = if remote {
        // Downloaded next to the local backups, under its own name.
        let path = backup_dir.join(backup_arg);
        webdav().await?.download(backup_arg, &path).await?;
        path
    } else if backup::is_backup_name(backup_arg) && backup_dir.join(backup_arg).is_file() {
        backup_dir.join(backup_arg)
    } else {
        PathBuf::from(backup_arg)
    };
    let restored = backup::restore(&home, &archive, &backup_dir)
        .with_context(|| format!("restore from {} failed; nothing was changed", archive.display()))?;
    if json {
        println!("{}", serde_json::to_string_pretty(&restored)?);
        return Ok(());
    }
    println!("restored {} files from {}", restored.files.len(), archive.display());
    for name in &restored.skipped {
        println!("  skipped {name} (not part of a backup)");
    }
    if let Some(previous) = &restored.previous {
        println!("previous state saved to {}", previous.display());
    }
    if core_running {
        println!("run `clash-verge-cli restart` to apply the restored profile and settings");
    }
    Ok(())
}
