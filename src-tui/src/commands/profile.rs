//! Non-interactive profile subscription commands.

use crate::profile_store::store::ProfileStore;

pub async fn list() -> anyhow::Result<()> {
    let store = ProfileStore::load().await?;
    let current = store.current_uid();
    let items = store.items();
    if items.is_empty() {
        println!("(no remote profiles)");
        return Ok(());
    }
    for item in items {
        let uid = item.uid.as_deref().unwrap_or("-");
        let name = item.name.as_deref().unwrap_or("(unnamed)");
        let marker = if current.as_deref() == Some(uid) {
            "*"
        } else {
            " "
        };
        let url = item.url.as_deref().unwrap_or("");
        println!("{marker} {uid}\t{name}\t{url}");
    }
    Ok(())
}

pub async fn import(url: &str, name: Option<&str>) -> anyhow::Result<()> {
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        anyhow::bail!("subscription URL must start with http:// or https://");
    }
    let mut store = ProfileStore::load().await?;
    let item = store.import_url(url, name).await?;
    let uid = item.uid.as_deref().unwrap_or("?");
    let name = item.name.as_deref().unwrap_or("(unnamed)");
    println!("imported {uid} ({name})");
    Ok(())
}

pub async fn update(uid: Option<&str>, all: bool) -> anyhow::Result<()> {
    let mut store = ProfileStore::load().await?;
    if all {
        let currents = store.update_all_remote().await?;
        println!("updated all remote profiles");
        if let Some(uid) = currents.first() {
            println!("current profile refreshed: {uid}");
        }
        return Ok(());
    }

    let uid = uid.ok_or_else(|| anyhow::anyhow!("provide a profile uid or pass --all"))?;
    let is_current = store.update_remote(uid, None).await?;
    println!("updated {uid}");
    if is_current {
        println!("profile is current — restart or switch to reload the core config");
    }
    Ok(())
}
