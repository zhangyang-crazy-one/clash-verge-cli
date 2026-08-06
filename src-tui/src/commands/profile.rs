//! Non-interactive profile subscription commands.

use crate::profile_store::store::ProfileStore;

pub async fn list() -> anyhow::Result<()> {
    let store = ProfileStore::snapshot().await?;
    let current = store.current_uid();
    let items = store.items();
    if items.is_empty() {
        println!("(no profiles)");
        return Ok(());
    }
    for item in items {
        let uid = item.uid.as_deref().unwrap_or("-");
        let name = item.name.as_deref().unwrap_or("(unnamed)");
        let marker = if current.as_deref() == Some(uid) { "*" } else { " " };
        let url = item.url.as_deref().unwrap_or("");
        // Redact query strings and credentials from the URL in list output.
        let redacted = crate::subscribe::fetch::redact_url(url);
        println!("{marker} {uid}\t{name}\t{redacted}");
    }
    Ok(())
}

pub async fn import(
    url: &str,
    name: Option<&str>,
    update_interval: Option<u64>,
    no_auto_update: bool,
) -> anyhow::Result<()> {
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        anyhow::bail!("subscription URL must start with http:// or https://");
    }
    let option = clash_verge_core::config::PrfOption {
        update_interval,
        allow_auto_update: no_auto_update.then_some(false),
        ..Default::default()
    };
    let item = ProfileStore::import_url_locked(url, name, Some(&option)).await?;
    let uid = item.uid.as_deref().unwrap_or("?");
    let name = item.name.as_deref().unwrap_or("(unnamed)");
    println!("imported {uid} ({name})");
    Ok(())
}

pub async fn update(uid: Option<&str>, all: bool) -> anyhow::Result<()> {
    if all {
        let currents = ProfileStore::update_all_remote_locked().await?;
        println!("updated all remote profiles");
        if let Some(uid) = currents.first() {
            println!("current profile refreshed: {uid}");
        }
        return Ok(());
    }

    let uid = uid.ok_or_else(|| anyhow::anyhow!("provide a profile uid or pass --all"))?;
    let is_current = ProfileStore::update_remote_locked(uid, None).await?;
    println!("updated {uid}");
    if is_current {
        println!("profile is current — restart or switch to reload the core config");
    }
    Ok(())
}

pub async fn delete(uid: &str) -> anyhow::Result<()> {
    ProfileStore::delete_locked(uid).await?;
    println!("deleted {uid}");
    Ok(())
}

pub async fn rename(uid: &str, new_name: &str) -> anyhow::Result<()> {
    ProfileStore::rename_locked(uid, new_name).await?;
    println!("renamed {uid} → {new_name}");
    Ok(())
}
