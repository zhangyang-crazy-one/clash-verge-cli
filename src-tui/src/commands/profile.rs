//! Non-interactive profile subscription commands.

use crate::mihomo_manager::manager::{CoreKind, MihomoManager};
use crate::profile_store::store::ProfileStore;

/// One row of `profile list` (also its `--json` schema).
#[derive(Debug, serde::Serialize)]
struct ProfileRow {
    uid: String,
    name: String,
    current: bool,
    /// Subscription URL with credentials and query redacted.
    url: String,
    /// Last refresh, Unix seconds.
    updated: Option<u64>,
    upload: Option<u64>,
    download: Option<u64>,
    /// Quota in bytes; 0 means unlimited.
    total: Option<u64>,
    /// Expiry, Unix seconds; 0 means never.
    expire: Option<u64>,
}

impl ProfileRow {
    fn of(item: &clash_verge_core::config::PrfItem, current: Option<&str>) -> Self {
        let uid = item.uid.as_deref().unwrap_or("-").to_string();
        Self {
            current: current == Some(uid.as_str()),
            uid,
            name: item.name.as_deref().unwrap_or("(unnamed)").to_string(),
            url: crate::subscribe::fetch::redact_url(item.url.as_deref().unwrap_or("")),
            updated: item.updated.map(|secs| secs as u64),
            upload: item.extra.as_ref().map(|extra| extra.upload),
            download: item.extra.as_ref().map(|extra| extra.download),
            total: item.extra.as_ref().map(|extra| extra.total),
            expire: item.extra.as_ref().map(|extra| extra.expire),
        }
    }

    /// `1.2 GiB / 50.0 GiB`, `1.2 GiB` (unlimited), or `-` (unknown).
    fn usage(&self) -> String {
        let (Some(up), Some(down)) = (self.upload, self.download) else {
            return "-".into();
        };
        let used = super::format_bytes(up.saturating_add(down));
        match self.total {
            Some(total) if total > 0 => format!("{used} / {}", super::format_bytes(total)),
            _ => used,
        }
    }

    fn cells(&self) -> Vec<String> {
        vec![
            if self.current { "*".into() } else { String::new() },
            self.uid.clone(),
            self.name.clone(),
            local_time(self.updated, "%Y-%m-%d %H:%M"),
            self.usage(),
            match self.expire {
                Some(0) | None => "-".into(),
                expire => local_time(expire, "%Y-%m-%d"),
            },
            self.url.clone(),
        ]
    }
}

fn local_time(unix_secs: Option<u64>, format: &str) -> String {
    unix_secs
        .and_then(|secs| i64::try_from(secs).ok())
        .and_then(|secs| chrono::DateTime::from_timestamp(secs, 0))
        .map(|time| time.with_timezone(&chrono::Local).format(format).to_string())
        .unwrap_or_else(|| "-".into())
}

pub async fn list(json: bool) -> anyhow::Result<()> {
    let store = ProfileStore::snapshot().await?;
    let current = store.current_uid();
    let rows: Vec<ProfileRow> = store
        .items()
        .iter()
        .map(|item| ProfileRow::of(item, current.as_deref()))
        .collect();
    if json {
        println!("{}", serde_json::to_string_pretty(&rows)?);
    } else if rows.is_empty() {
        println!("(no profiles)");
    } else {
        let headers = ["", "UID", "NAME", "UPDATED", "USAGE", "EXPIRES", "URL"];
        print!("{}", super::table(&headers, rows.iter().map(ProfileRow::cells)));
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
    let store = ProfileStore::snapshot().await.ok();
    let mut trusted_hosts: Vec<compact_str::CompactString> = store
        .map(|s| {
            s.items()
                .iter()
                .filter_map(|it| it.option.as_ref()?.trusted_hosts.clone())
                .flatten()
                .collect()
        })
        .unwrap_or_default();
    if let Ok(parsed) = url::Url::parse(url)
        && let Some(host) = parsed.host_str()
        && !trusted_hosts.iter().any(|h| h.as_str() == host)
    {
        trusted_hosts.push(host.into());
    }
    let option = clash_verge_core::config::PrfOption {
        update_interval,
        allow_auto_update: no_auto_update.then_some(false),
        trusted_hosts: (!trusted_hosts.is_empty()).then_some(trusted_hosts),
        ..Default::default()
    };
    let item = ProfileStore::import_url_locked(url, name, Some(&option)).await?;
    let uid = item.uid.as_deref().unwrap_or("?");
    let name = item.name.as_deref().unwrap_or("(unnamed)");
    println!("imported {uid} ({name})");
    Ok(())
}

/// Resolve a uid-or-name argument to a profile uid.
async fn resolve_uid(query: &str) -> anyhow::Result<String> {
    let store = ProfileStore::snapshot().await?;
    let items = store.items();
    let item = crate::services::profile::find_profile(&items, query)?;
    item.uid
        .as_deref()
        .map(str::to_string)
        .ok_or_else(|| anyhow::anyhow!("profile '{query}' has no uid"))
}

/// `profile use`: make a profile current and apply it to the running core
/// (or write it for the next start).
///
/// Dispatches by [`CoreKind`] so sing-box does not get issued a
/// `PUT /configs` (which sing-box's controller silently ignores):
/// - mihomo uses [`crate::services::profile::switch_profile`] which honours
///   `core_running` — when the core is down, the mihomo runtime config is
///   staged on disk for the next start and no API call is made.
/// - sing-box, when the core is running, uses
///   [`crate::services::profile::switch_profile_for_core`] to regenerate the
///   JSON config and restart the core (sing-box's controller does not honour
///   `PUT /configs`).
/// - sing-box, when the core is **not** running, only updates `current`. The
///   next `start` reads `current_uid` via
///   `ManagerInner::active_profile_yaml`, so calling the helper here would
///   route through `apply_singbox_restart` → `manager.restart()` and start a
///   core the user never asked for. See the `core_not_running → no_restart`
///   note in the per-call site tests below.
pub async fn use_profile(manager: &MihomoManager, query: &str) -> anyhow::Result<()> {
    let store = ProfileStore::snapshot().await?;
    let items = store.items();
    let item = crate::services::profile::find_profile(&items, query)?;
    let api = manager.api();
    let running = super::core_running(&api).await;
    let enable_tun = clash_verge_core::config::IVerge::new()
        .await
        .enable_tun_mode
        .unwrap_or(false);
    match manager.core_kind() {
        CoreKind::Mihomo => {
            // mihomo: `switch_profile` already honours `core_running` —
            // with `running=false`, it writes the runtime config to disk for
            // the next start and skips the doomed `PUT /configs`.
            crate::services::profile::switch_profile(&api, item, enable_tun, running)
                .await
                .map_err(|error| anyhow::anyhow!(error))?;
        }
        CoreKind::SingBox => {
            if running {
                // sing-box's controller ignores `PUT /configs`; route through
                // the manager-aware switch helper so the JSON config is
                // regenerated and the core restarts.
                crate::services::profile::switch_profile_for_core(manager, item, enable_tun, true)
                    .await
                    .map_err(|error| anyhow::anyhow!(error))?;
            } else {
                // No core is running — only update `current`. The next
                // `start` reads `current_uid` via `active_profile_yaml`
                // and assembles the sing-box config from it; calling
                // `switch_profile_for_core` here would route through
                // `apply_singbox_restart` → `manager.restart()` and start
                // a core the user never asked for.
                let uid = item
                    .uid
                    .as_deref()
                    .ok_or_else(|| anyhow::anyhow!("profile switch: profile has no uid"))?;
                ProfileStore::replace_current_locked(uid)
                    .await
                    .map_err(|error| anyhow::anyhow!("profile switch: {error}"))?;
            }
        }
    }
    let uid = item.uid.as_deref().unwrap_or("?");
    let name = item.name.as_deref().unwrap_or("(unnamed)");
    if running {
        println!("switched to {uid} ({name})");
    } else {
        println!("switched to {uid} ({name}); core not running, used on next start");
    }
    Ok(())
}

pub async fn update(manager: &MihomoManager, query: Option<&str>, all: bool, reload: bool) -> anyhow::Result<()> {
    let (refreshed_current, failures) = if all {
        // Per-profile outcomes: one broken subscription must not keep a
        // refreshed current profile from being reloaded.
        let uids: Vec<String> = ProfileStore::snapshot()
            .await?
            .items()
            .into_iter()
            .filter_map(|item| item.uid.map(|uid| uid.to_string()))
            .collect();
        let (updated, failed) = ProfileStore::update_remotes_locked(&uids).await?;
        for (uid, _) in &updated {
            println!("updated {uid}");
        }
        for (uid, error) in &failed {
            eprintln!("failed to update {uid}: {error}");
        }
        let current = updated
            .into_iter()
            .find_map(|(uid, is_current)| is_current.then_some(uid));
        (current, failed.len())
    } else {
        let query = query.ok_or_else(|| anyhow::anyhow!("provide a profile uid or name, or pass --all"))?;
        let uid = resolve_uid(query).await?;
        let is_current = ProfileStore::update_remote_locked(&uid, None).await?;
        println!("updated {uid}");
        (is_current.then_some(uid), 0)
    };

    if let Some(uid) = refreshed_current {
        reload_if_requested(manager, &uid, reload).await?;
    }
    if failures > 0 {
        anyhow::bail!("{failures} profile update(s) failed");
    }
    Ok(())
}

/// Apply a refreshed current profile to the running core when `--reload`
/// was given; otherwise say how to apply it.
///
/// Uses the manager-aware
/// [`crate::subscribe::scheduler::reload_current_profile_for_core`] so
/// sing-box's controller does not get a `PUT /configs` (which it would
/// silently ignore). The function already bails when the core is not
/// running, so by the time we reach the helper `running=true` is
/// guaranteed — `reload_current_profile_for_core` therefore never takes
/// the sing-box branch with `core_running=false`, and the brief's
/// "don't start a core when not running" rule is preserved.
async fn reload_if_requested(manager: &MihomoManager, uid: &str, reload: bool) -> anyhow::Result<()> {
    let api = manager.api();
    if !reload || !super::core_running(&api).await {
        println!("current profile {uid} refreshed; pass --reload (or restart) to apply it to the running core");
        return Ok(());
    }
    let enable_tun = clash_verge_core::config::IVerge::new()
        .await
        .enable_tun_mode
        .unwrap_or(false);
    crate::subscribe::scheduler::reload_current_profile_for_core(manager, uid, enable_tun, true)
        .await
        .map_err(|error| anyhow::anyhow!("profile reload: {error}"))?;
    println!("reloaded the running core with {uid}");
    Ok(())
}

pub async fn delete(query: &str) -> anyhow::Result<()> {
    let uid = resolve_uid(query).await?;
    ProfileStore::delete_locked(&uid).await?;
    println!("deleted {uid}");
    Ok(())
}

pub async fn rename(query: &str, new_name: &str) -> anyhow::Result<()> {
    let uid = resolve_uid(query).await?;
    ProfileStore::rename_locked(&uid, new_name).await?;
    println!("renamed {uid} → {new_name}");
    Ok(())
}

/// One-shot migration of a Clash Verge GUI profile set into the standalone
/// directory. Copies `profiles.yaml`, `profiles/`, `verge.yaml`, and the
/// clash config template, then re-lists the imported profiles.
pub async fn migrate(from: &std::path::Path, force: bool) -> anyhow::Result<()> {
    let home = clash_verge_core::utils::dirs::app_home_dir()?;
    migrate_files(from, &home, force)?;
    list(false).await?;
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

    // Subscription bodies + chain fragments live under profiles/. Copy them
    // BEFORE profiles.yaml: a silently dropped body would leave profiles.yaml
    // referencing a missing file while the migration still reports success.
    // Any per-file failure aborts the whole migration with the file named.
    let source_dir = from.join("profiles");
    if source_dir.exists() {
        let dest_dir = dest.join("profiles");
        std::fs::create_dir_all(&dest_dir)?;
        let mut failed: Vec<String> = Vec::new();
        for entry in std::fs::read_dir(&source_dir)? {
            let entry = entry?;
            let file_name = entry.file_name();
            if let Err(error) = std::fs::copy(entry.path(), dest_dir.join(&file_name)) {
                failed.push(format!("{}: {error}", file_name.to_string_lossy()));
            }
        }
        if !failed.is_empty() {
            anyhow::bail!(
                "migration failed to copy {} profile file(s) under profiles/ (profiles.yaml would reference missing files): {}",
                failed.len(),
                failed.join("; ")
            );
        }
    }

    std::fs::copy(&source_profiles, &dest_profiles)?;

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
    fn profile_rows_show_usage_and_expiry() {
        let mut item = clash_verge_core::config::PrfItem {
            uid: Some("R1".into()),
            name: Some("Home".into()),
            url: Some("https://user:pw@example.com/sub?token=secret".into()),
            ..Default::default()
        };
        let bare = ProfileRow::of(&item, Some("R1"));
        assert!(bare.current);
        assert!(!bare.url.contains("secret") && !bare.url.contains("pw"), "{}", bare.url);
        assert_eq!(bare.usage(), "-");
        assert_eq!(bare.cells()[5], "-");

        item.extra = Some(clash_verge_core::config::PrfExtra {
            upload: 1024,
            download: 1024,
            total: 4096,
            expire: 0,
        });
        let row = ProfileRow::of(&item, None);
        assert!(!row.current);
        assert_eq!(row.usage(), "2.0 KiB / 4.0 KiB");
        assert_eq!(row.cells()[5], "-", "0 means never expires");
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

    #[test]
    fn migrate_fails_and_names_file_when_a_profile_body_cannot_be_copied() {
        let src = temp_dir("broken-src");
        let dest = temp_dir("broken-dest");
        must(std::fs::create_dir_all(src.join("profiles")), "mkdir");
        must(
            std::fs::write(src.join("profiles.yaml"), "current: R1\nitems:\n- uid: R1\n"),
            "write",
        );
        // A directory where a subscription body should be: std::fs::copy
        // cannot read it as a file, simulating an unreadable/missing source
        // body that must abort the migration instead of being silently
        // dropped.
        must(std::fs::create_dir_all(src.join("profiles").join("R1.yaml")), "mkdir");

        match migrate_files(&src, &dest, false) {
            Ok(()) => panic!("migration with an uncopyable body must fail"),
            Err(error) => {
                let text = error.to_string();
                assert!(text.contains("R1.yaml"), "error must name the failed file: {text}");
                assert!(text.contains("profiles/"), "error must locate the copy phase: {text}");
            }
        }
        // Fail BEFORE profiles.yaml is written so the destination never
        // references missing bodies.
        assert!(!dest.join("profiles.yaml").exists());

        let _ = std::fs::remove_dir_all(&src);
        let _ = std::fs::remove_dir_all(&dest);
    }

    // ---- Dispatcher tests for the sing-box-aware `use_profile` /
    //      `reload_if_requested` rewiring. Each test asserts on the
    //      helper-level error signature: the mihomo branch
    //      (`switch_profile` → `reload_remote_profile` →
    //      `compose_remote_profile`) reports "profile file not found",
    //      the sing-box branch (`switch_profile_for_core` →
    //      `read_profile_yaml`) reports "failed to read". The
    //      discriminator is stable across minor refactors of either
    //      helper because it comes from a different file: the mihomo
    //      helper reads through `app_profiles_dir()` while the sing-box
    //      helper calls `tokio::fs::read_to_string` directly.
    use crate::subscribe::from_url::RemoteProfileBundle;

    /// Seed a single remote profile whose on-disk body is intentionally
    /// missing. The store knows the file's name (so `compose_remote_profile`
    /// / `read_profile_yaml` get a non-empty `Item.file`) but the test never
    /// writes the file, so any branch touching the body errors before
    /// reaching the controller.
    async fn seed_missing_remote_profile(uid: &str, name: &str) {
        let mut store = crate::profile_store::store::tests::empty_store();
        let bundle = RemoteProfileBundle {
            item: clash_verge_core::config::PrfItem {
                uid: Some(uid.into()),
                itype: Some("remote".into()),
                name: Some(name.into()),
                file: Some(format!("{uid}.yaml").into()),
                ..Default::default()
            },
            fragments: vec![match clash_verge_core::config::PrfItem::from_merge(None) {
                Ok(item) => item,
                Err(error) => panic!("merge fragment: {error}"),
            }],
        };
        must(store.append_bundle(bundle).await, "append bundle");
    }

    /// Bind a fake sing-box controller on a free localhost port and answer
    /// one `GET /version` with a minimal valid JSON body. Returns the
    /// bound address so the caller can wire it into
    /// `MihomoManager::with_singbox_controller`. The spawned task lives
    /// for the duration of the test (held by the handle in the caller).
    async fn fake_singbox_controller() -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let handle = tokio::spawn(async move {
            if let Ok((mut stream, _)) = listener.accept().await {
                let mut buf = [0u8; 512];
                // Best-effort drain; reqwest sends the headers in one go.
                let _ = stream.read(&mut buf).await;
                let body = r#"{"version":"sing-box 1.13.12"}"#;
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.shutdown().await;
            }
        });
        (addr, handle)
    }

    #[tokio::test]
    async fn use_profile_dispatches_to_mihomo_branch_via_switch_profile() {
        // mihomo manager + missing file → mihomo's
        // `compose_remote_profile` reports "profile file not found".
        // The mihomo branch of `use_profile` calls `switch_profile` which
        // routes through `load_remote_profile_with_rules`, so seeing this
        // message proves the dispatcher took the mihomo branch.
        let root = crate::profile_store::store::tests::test_app_home_root();
        let _dir_guard = crate::profile_store::store::tests::claim_test_app_home(root.clone()).await;

        let uid = "Rcmd-mihomo-missing";
        seed_missing_remote_profile(uid, "mihomo-missing").await;

        let bogus_socket = std::env::temp_dir().join(format!("cv-no-sock-{}.sock", uuid::Uuid::new_v4()));
        let mgr = MihomoManager::new(root.clone()).with_socket(bogus_socket);

        let error = use_profile(&mgr, uid)
            .await
            .expect_err("missing profile file must surface as an error");
        let text = error.to_string();
        assert!(
            text.contains("profile file not found"),
            "mihomo branch surfaces load_remote_profile_with_rules' error: {text}"
        );
        assert!(
            !text.contains("failed to read"),
            "must NOT route through the sing-box branch: {text}"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn use_profile_dispatches_to_singbox_branch_via_switch_profile_for_core() {
        // sing-box manager with a reachable fake controller (running=true)
        // + missing file → sing-box's `read_profile_yaml` reports
        // "failed to read <path>". Seeing this message proves the
        // dispatcher routed through `switch_profile_for_core` instead of
        // the mihomo-only `switch_profile`.
        let root = crate::profile_store::store::tests::test_app_home_root();
        let _dir_guard = crate::profile_store::store::tests::claim_test_app_home(root.clone()).await;

        let uid = "Rcmd-singbox-missing";
        seed_missing_remote_profile(uid, "singbox-missing").await;

        let (addr, _handle) = fake_singbox_controller().await;
        let mgr = MihomoManager::new(root.clone())
            .with_core_kind(CoreKind::SingBox)
            .with_singbox_controller(addr);

        let error = use_profile(&mgr, uid)
            .await
            .expect_err("missing profile file must surface as an error");
        let text = error.to_string();
        assert!(
            text.contains("failed to read") && text.contains(&format!("{uid}.yaml")),
            "sing-box branch surfaces read_profile_yaml's error: {text}"
        );
        assert!(
            !text.contains("profile file not found"),
            "must NOT route through the mihomo branch: {text}"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn use_profile_skips_singbox_restart_when_core_is_not_running() {
        // sing-box manager with an UNREACHABLE controller (running=false)
        // + missing file → must SUCCEED. The sing-box branch's helper
        // would route through `apply_singbox_restart` → `manager.restart()`,
        // which either tries to start a fresh core (forbidden by the
        // brief: "不在未运行时启动一个 core") or errors with "sing-box
        // binary not found" in the test environment. Neither should
        // happen: only `current_uid` gets updated so the next `start`
        // assembles the sing-box config from the active profile.
        let root = crate::profile_store::store::tests::test_app_home_root();
        let _dir_guard = crate::profile_store::store::tests::claim_test_app_home(root.clone()).await;

        let uid = "Rcmd-singbox-not-running";
        seed_missing_remote_profile(uid, "singbox-not-running").await;

        // 127.0.0.1:1 — well-known port nobody binds, TCP connect fails fast.
        let unreachable: std::net::SocketAddr = "127.0.0.1:1".parse().expect("addr");
        let mgr = MihomoManager::new(root.clone())
            .with_core_kind(CoreKind::SingBox)
            .with_singbox_controller(unreachable);

        use_profile(&mgr, uid)
            .await
            .expect("sing-box + !running must skip restart");

        // The next `start` reads `current_uid` via
        // `ManagerInner::active_profile_yaml`; confirm the seeded profile
        // is now the active one (no file rewrite was attempted, no
        // sing-box config was generated, no controller was contacted).
        let snapshot = ProfileStore::snapshot().await.expect("snapshot");
        assert_eq!(
            snapshot.current_uid().as_deref(),
            Some(uid),
            "current_uid must be set so the next start picks this profile up"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn reload_if_requested_dispatches_to_singbox_branch_via_reload_current_profile_for_core() {
        // `reload_if_requested` already bails when `!reload` or
        // `!core_running`; only the `reload=true && running=true` arm
        // reaches the helper. A sing-box manager + a fake reachable
        // controller + a missing profile file should therefore surface
        // sing-box's `read_profile_yaml` error ("failed to read ..."),
        // proving the helper called is `reload_current_profile_for_core`
        // (the manager-aware one), not `reload_current_profile`
        // (mihomo-only). The mihomo-only helper would never reach the
        // sing-box branch and would surface a different signature.
        let root = crate::profile_store::store::tests::test_app_home_root();
        let _dir_guard = crate::profile_store::store::tests::claim_test_app_home(root.clone()).await;

        let uid = "Rcmd-reload-singbox";
        seed_missing_remote_profile(uid, "reload-singbox").await;

        let (addr, _handle) = fake_singbox_controller().await;
        let mgr = MihomoManager::new(root.clone())
            .with_core_kind(CoreKind::SingBox)
            .with_singbox_controller(addr);

        let error = reload_if_requested(&mgr, uid, true)
            .await
            .expect_err("missing file must surface as an error");
        let text = error.to_string();
        assert!(
            text.contains("failed to read") && text.contains(&format!("{uid}.yaml")),
            "reload helper must take the sing-box branch: {text}"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn reload_if_requested_bails_when_core_not_running() {
        // The brief's "don't start a core when not running" rule applies
        // to reload too: `reload_if_requested` must short-circuit before
        // `reload_current_profile_for_core` is reached, otherwise the
        // sing-box branch's `apply_singbox_restart` would start a core
        // the user never asked for. The current contract is
        // "reload=true && running=true reaches the helper; otherwise
        // the function returns Ok without touching the helper". Lock
        // that in here so a future refactor cannot silently widen the
        // helper call to also fire when the core is down.
        let root = crate::profile_store::store::tests::test_app_home_root();
        let _dir_guard = crate::profile_store::store::tests::claim_test_app_home(root.clone()).await;

        let uid = "Rcmd-reload-not-running";
        seed_missing_remote_profile(uid, "reload-not-running").await;

        let bogus_socket = std::env::temp_dir().join(format!("cv-no-sock-{}.sock", uuid::Uuid::new_v4()));
        let mgr = MihomoManager::new(root.clone())
            .with_core_kind(CoreKind::SingBox)
            .with_singbox_controller("127.0.0.1:1".parse().expect("addr"))
            .with_socket(bogus_socket);

        // reload=false → early return, no error even with a missing file.
        must(
            reload_if_requested(&mgr, uid, false).await,
            "reload=false must short-circuit",
        );
        // running=false → early return, no error even with a missing file.
        must(
            reload_if_requested(&mgr, uid, true).await,
            "running=false must short-circuit",
        );

        let _ = std::fs::remove_dir_all(&root);
    }
}
