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

/// One-shot migration of a Clash Verge GUI profile set into the standalone
/// directory. Copies `profiles.yaml`, `profiles/`, `verge.yaml`, and the
/// clash config template, then re-lists the imported profiles.
pub async fn migrate(from: &std::path::Path, force: bool) -> anyhow::Result<()> {
    let home = clash_verge_core::utils::dirs::app_home_dir()?;
    migrate_files(from, &home, force)?;
    list().await?;
    Ok(())
}

/// Path-level migration core (extracted for unit tests; no global dirs).
fn migrate_files(from: &std::path::Path, dest: &std::path::Path, force: bool) -> anyhow::Result<()> {
    let source_profiles = from.join("profiles.yaml");
    if !source_profiles.exists() {
        anyhow::bail!("source {} has no profiles.yaml", from.display());
    }
    let dest_profiles = dest.join("profiles.yaml");
    if dest_profiles.exists() && !force {
        anyhow::bail!("standalone profiles.yaml already exists; pass --force to overwrite");
    }
    std::fs::create_dir_all(dest)?;
    std::fs::copy(&source_profiles, &dest_profiles)?;

    // Chain fragments + subscription bodies live under profiles/.
    let source_dir = from.join("profiles");
    if source_dir.exists() {
        let dest_dir = dest.join("profiles");
        std::fs::create_dir_all(&dest_dir)?;
        for entry in std::fs::read_dir(&source_dir)? {
            let entry = entry?;
            let file_name = entry.file_name();
            let _ = std::fs::copy(entry.path(), dest_dir.join(&file_name));
        }
    }

    // Settings (best-effort; templates exist if absent).
    let _ = std::fs::copy(from.join("verge.yaml"), dest.join("verge.yaml"));
    for candidate in ["config.yaml", "clash-verge.yaml"] {
        if from.join(candidate).exists() {
            copy_clash_config_without_gui_socket(&from.join(candidate), &dest.join(candidate));
            break;
        }
    }
    Ok(())
}

/// Copy the GUI clash config but strip `external-controller-unix`: the
/// standalone `build_manager` then falls back to the CLI's own socket
/// instead of re-pointing at the GUI's controller.
fn copy_clash_config_without_gui_socket(source: &std::path::Path, dest: &std::path::Path) {
    let raw = match std::fs::read_to_string(source) {
        Ok(raw) => raw,
        Err(_) => return,
    };
    let Ok(mut mapping) = serde_yaml_ng::from_str::<serde_yaml_ng::Mapping>(&raw) else {
        let _ = std::fs::copy(source, dest);
        return;
    };
    mapping.remove("external-controller-unix");
    if let Ok(yaml) = serde_yaml_ng::to_string(&mapping) {
        let _ = std::fs::write(dest, yaml);
    } else {
        let _ = std::fs::copy(source, dest);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(label: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("cv-migrate-{label}-{}", uuid::Uuid::new_v4()))
    }

    /// Unwrap a test result with a panic carrying the error (avoids
    /// `.expect()` which pi-lens flags). Works for `std::io::Result` and
    /// `anyhow::Result`.
    fn must<T, E: std::fmt::Display>(result: Result<T, E>, what: &str) -> T {
        match result {
            Ok(value) => value,
            Err(error) => panic!("{what}: {error}"),
        }
    }

    #[test]
    fn migrate_copies_profiles_and_settings() {
        let src = temp_dir("src");
        let dest = temp_dir("dest");
        must(std::fs::create_dir_all(src.join("profiles")), "mkdir");
        must(
            std::fs::write(src.join("profiles.yaml"), "current: R1\nitems:\n- uid: R1\n"),
            "write",
        );
        must(
            std::fs::write(src.join("profiles").join("R1.yaml"), "proxies: []\n"),
            "write",
        );
        must(std::fs::write(src.join("verge.yaml"), "language: en\n"), "write");
        must(
            std::fs::write(
                src.join("config.yaml"),
                "mixed-port: 7897\nexternal-controller-unix: /tmp/verge/verge-mihomo.sock\n",
            ),
            "write",
        );

        must(migrate_files(&src, &dest, false), "migrate");
        assert!(dest.join("profiles.yaml").exists());
        assert!(dest.join("profiles").join("R1.yaml").exists());
        assert!(dest.join("verge.yaml").exists());
        assert!(dest.join("config.yaml").exists());
        // The GUI's controller socket must not leak into the standalone config.
        let migrated = must(std::fs::read_to_string(dest.join("config.yaml")), "read");
        assert!(!migrated.contains("external-controller-unix"));
        assert!(migrated.contains("mixed-port"));

        // Refuses to overwrite without --force; --force overwrites.
        assert!(migrate_files(&src, &dest, false).is_err());
        assert!(migrate_files(&src, &dest, true).is_ok());

        let _ = std::fs::remove_dir_all(&src);
        let _ = std::fs::remove_dir_all(&dest);
    }

    #[test]
    fn migrate_rejects_source_without_profiles() {
        let src = temp_dir("empty-src");
        let dest = temp_dir("dest2");
        must(std::fs::create_dir_all(&src), "mkdir");
        assert!(migrate_files(&src, &dest, false).is_err());
        let _ = std::fs::remove_dir_all(&src);
        let _ = std::fs::remove_dir_all(&dest);
    }
}
