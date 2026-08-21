//! Resolve and auto-install the sing-box core binary.
//!
//! Mirrors [`super::binary`] (mihomo) with sing-box-specific differences:
//! release assets are `.tar.gz` archives containing a versioned directory
//! (not bare gzip binaries), the version probe is `sing-box version`
//! (not `-v`), and the system fallback also scans `$PATH` for a plain
//! `sing-box` install.

use std::io::Read as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::Context;
use tokio::process::Command;
use tokio::sync::Mutex;

/// Managed sing-box stable version — compile-time fallback when the GitHub
/// API is unreachable. Anchored to the 1.13.x stable line: DNS configuration
/// switched to the new typed-server format in 1.12 and legacy `address`
/// syntax is removed in 1.14, so 1.13.x is the supported target window
/// (see add-singbox-dual-core design.md, F4).
pub const SINGBOX_FALLBACK_VERSION: &str = "v1.13.15";

const SINGBOX_REPO: &str = "SagerNet/sing-box";

/// Serialise concurrent downloads — two starts racing on the same
/// `$dest.download` temporary would race on `rename(2)` and on archive
/// extraction. Unlike the mihomo download lock this is held across the
/// whole async section (tokio mutex guards are `Send`).
static DOWNLOAD_LOCK: Mutex<()> = Mutex::const_new(());

/// Resolve the latest sing-box stable tag from GitHub (`/releases/latest`
/// excludes pre-releases such as the 1.14 beta line), falling back to the
/// compile-time constant when unreachable.
pub async fn latest_singbox_version() -> &'static str {
    // Same OnceCell-per-call-site caching pattern as mihomo's resolver;
    // a process-lifetime cache is fine because upgrades are handled by
    // re-resolving at next start.
    static LATEST: tokio::sync::OnceCell<String> = tokio::sync::OnceCell::const_new();
    LATEST
        .get_or_init(|| async {
            if let Some(tag) = crate::subscribe::client_meta::fetch_latest_release_tag(SINGBOX_REPO).await {
                tracing::info!(target: "singbox", "latest sing-box release from GitHub: {tag}");
                return tag;
            }
            tracing::warn!(
                target: "singbox",
                "GitHub API unreachable, falling back to {SINGBOX_FALLBACK_VERSION}"
            );
            SINGBOX_FALLBACK_VERSION.to_string()
        })
        .await
        .as_str()
}

/// Managed binary path, parallel to mihomo:
/// `$XDG_DATA_HOME/clash-verge-cli/sing-box`.
pub fn singbox_binary_path() -> PathBuf {
    if let Some(data_dir) = std::env::var_os("XDG_DATA_HOME") {
        return PathBuf::from(data_dir).join("clash-verge-cli").join("sing-box");
    }
    let home = std::env::var_os("HOME").unwrap_or_default();
    PathBuf::from(home)
        .join(".local")
        .join("share")
        .join("clash-verge-cli")
        .join("sing-box")
}

/// Best-effort system sing-box fallback: `verge-sing-box` in standard bin
/// dirs first, then a plain `sing-box` found on `$PATH`.
fn system_singbox() -> Option<PathBuf> {
    let mut seen = std::collections::HashSet::new();

    let named_dirs = [
        dirs::executable_dir(),
        Some(PathBuf::from("/usr/bin")),
        Some(PathBuf::from("/usr/local/bin")),
    ];
    for dir in named_dirs.into_iter().flatten() {
        let path = dir.join("verge-sing-box");
        if seen.insert(path.clone()) && is_executable_file(&path) {
            return Some(path);
        }
    }

    if let Some(path_var) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path_var) {
            let path = dir.join("sing-box");
            if seen.insert(path.clone()) && is_executable_file(&path) {
                return Some(path);
            }
        }
    }
    None
}

fn is_executable_file(path: &Path) -> bool {
    path.is_file()
        && std::fs::metadata(path)
            .map(|m| m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
}

/// Resolve the binary that WOULD be used without downloading anything.
/// Used by read-only preflights that must not trigger a network install.
pub fn candidate_without_install() -> Option<PathBuf> {
    system_singbox().or_else(|| {
        let managed = singbox_binary_path();
        managed.is_file().then_some(managed)
    })
}

/// Where the runnable sing-box binary came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SingboxBinarySource {
    System,
    ManagedCached,
    Downloaded,
}

impl SingboxBinarySource {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::ManagedCached => "cached",
            Self::Downloaded => "downloaded",
        }
    }
}

/// Result of resolving (and possibly installing) sing-box.
#[derive(Debug, Clone)]
pub struct ResolvedSingBox {
    pub path: PathBuf,
    pub source: SingboxBinarySource,
    pub version: String,
}

/// Resolve a runnable sing-box binary, downloading the managed build when needed.
///
/// Preference order:
/// 1. System binary (`verge-sing-box`, or `sing-box` on PATH)
/// 2. Managed data-dir binary at the detected latest stable version
pub async fn resolve_or_install() -> anyhow::Result<ResolvedSingBox> {
    let target_version = latest_singbox_version().await;
    if let Some(system) = system_singbox() {
        let version = read_singbox_version(&system).await?.unwrap_or_else(|| "unknown".into());
        return Ok(ResolvedSingBox {
            path: system,
            source: SingboxBinarySource::System,
            version,
        });
    }

    let managed = singbox_binary_path();
    if managed.exists()
        && let Ok(Some(version)) = read_singbox_version(&managed).await
        && version_matches_target(&version, target_version)
    {
        super::binary::ensure_executable(&managed).await?;
        return Ok(ResolvedSingBox {
            path: managed,
            source: SingboxBinarySource::ManagedCached,
            version,
        });
    }

    download_managed_singbox(&managed, target_version).await?;
    super::binary::ensure_executable(&managed).await?;
    Ok(ResolvedSingBox {
        path: managed,
        source: SingboxBinarySource::Downloaded,
        version: target_version.to_string(),
    })
}

/// Probe availability without side effects: does NOT download or mutate.
pub async fn read_singbox_version(path: &Path) -> anyhow::Result<Option<String>> {
    let output = Command::new(path)
        .arg("version")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .with_context(|| format!("failed to execute {}", path.display()))?;

    let text = String::from_utf8_lossy(&output.stdout);
    Ok(extract_version_token(&text))
}

/// Parse `sing-box version` output. Example:
/// ```text
/// Version: 1.13.12
/// Environment: linux, amd64
/// Tags: with_gvisor,with_quic,...
/// ```
fn extract_version_token(text: &str) -> Option<String> {
    for line in text.lines() {
        let Some(value) = line.strip_prefix("Version:") else {
            continue;
        };
        let value = value.trim();
        if !value.is_empty() {
            return Some(value.to_string());
        }
    }
    None
}

/// Compare an installed version string (`1.13.12`) against a target tag
/// (`v1.13.12`), tolerating the missing/extra leading `v`.
fn version_matches_target(version: &str, target: &str) -> bool {
    let normalize = |s: &str| s.strip_prefix('v').unwrap_or(s).to_string();
    normalize(version) == normalize(target)
}

async fn download_managed_singbox(dest: &Path, version: &str) -> anyhow::Result<()> {
    let arch = linux_arch_name().context("unsupported CPU architecture for auto-install")?;
    // Release assets carry the version WITHOUT the leading `v`:
    // https://github.com/SagerNet/sing-box/releases/download/v1.13.12/sing-box-1.13.12-linux-amd64.tar.gz
    let bare = version.strip_prefix('v').unwrap_or(version);
    let asset_dir = format!("sing-box-{bare}-linux-{arch}");
    let url = format!("https://github.com/{SINGBOX_REPO}/releases/download/{version}/{asset_dir}.tar.gz");

    tracing::info!(target: "singbox", "downloading sing-box {version} → {}", dest.display());

    let client = reqwest::Client::builder()
        .user_agent(format!("clash-verge-cli/{}", env!("CARGO_PKG_VERSION")))
        .timeout(std::time::Duration::from_secs(180))
        .build()
        .context("failed to build download client")?;

    let response = client
        .get(&url)
        .send()
        .await
        .with_context(|| format!("failed to download sing-box from {url}"))?
        .error_for_status()
        .with_context(|| format!("sing-box download returned error for {url}"))?;

    let compressed = response
        .bytes()
        .await
        .context("failed to read sing-box download body")?;

    let guard = DOWNLOAD_LOCK.lock().await;
    let result = extract_tar_gz_binary(&compressed, dest).await;
    drop(guard);
    result?;

    tracing::info!(target: "singbox", "installed sing-box {version}");
    Ok(())
}

/// Extract the core binary from a sing-box release tarball.
///
/// The archive contains a top-level versioned directory holding the
/// `sing-box` binary; we locate any entry whose file name is exactly
/// `sing-box` regardless of directory depth.
async fn extract_tar_gz_binary(compressed: &[u8], dest: &Path) -> anyhow::Result<()> {
    let decoder = flate2::read::GzDecoder::new(compressed);
    let mut archive = tar::Archive::new(decoder);

    let mut payload: Option<Vec<u8>> = None;
    {
        let mut entries = archive.entries().context("failed to read sing-box tarball entries")?;
        for entry in entries.by_ref() {
            let mut entry = entry.context("failed to read tar entry")?;
            if entry.header().entry_type() != tar::EntryType::Regular {
                continue;
            }
            let path = entry.path().context("failed to read tar entry path")?;
            if path.file_name().is_some_and(|name| name == "sing-box") {
                let mut buf = Vec::new();
                entry
                    .read_to_end(&mut buf)
                    .context("failed to read sing-box binary from tarball")?;
                payload = Some(buf);
                break;
            }
        }
    }

    let payload = payload.ok_or_else(|| anyhow::anyhow!("sing-box tarball did not contain a `sing-box` binary"))?;

    if let Some(parent) = dest.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    let tmp = dest.with_extension("download");
    tokio::fs::write(&tmp, &payload)
        .await
        .with_context(|| format!("failed to write {}", tmp.display()))?;
    tokio::fs::rename(&tmp, dest)
        .await
        .with_context(|| format!("failed to install sing-box to {}", dest.display()))?;
    Ok(())
}

fn linux_arch_name() -> Option<&'static str> {
    match std::env::consts::ARCH {
        "x86_64" => Some("amd64"),
        "aarch64" => Some("arm64"),
        "arm" => Some("armv7"),
        "riscv64" => Some("riscv64"),
        _ => None,
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::io::Write as _;

    #[test]
    fn extracts_version_from_singbox_output() {
        let output = "Version: 1.13.12\nEnvironment: linux, amd64\nTags: with_gvisor,with_quic\n";
        assert_eq!(extract_version_token(output), Some("1.13.12".into()));
        assert!(
            extract_version_token("no version here").is_none(),
            "should parse nothing but must not panic"
        );
    }

    #[test]
    fn version_matching_ignores_v_prefix() {
        assert!(version_matches_target("1.13.12", "v1.13.12"));
        assert!(version_matches_target("v1.13.12", "v1.13.12"));
        assert!(!version_matches_target("1.13.11", "v1.13.12"));
    }

    #[test]
    fn linux_asset_covers_common_arches() {
        if matches!(std::env::consts::ARCH, "x86_64" | "aarch64") {
            assert!(linux_arch_name().is_some());
        }
    }

    #[test]
    fn test_singbox_binary_path_uses_xdg_data_home() {
        let prev = std::env::var_os("XDG_DATA_HOME");
        // SAFETY: single-threaded mutation guarded by serial test runner.
        unsafe {
            std::env::set_var("XDG_DATA_HOME", "/tmp/test-xdg-sb");
        }

        let path = singbox_binary_path();
        assert!(path.ends_with("clash-verge-cli/sing-box"), "got {path:?}");

        match prev {
            Some(v) => unsafe { std::env::set_var("XDG_DATA_HOME", v) },
            None => unsafe { std::env::remove_var("XDG_DATA_HOME") },
        }
    }

    #[test]
    fn extracts_nested_singbox_binary_from_tarball() {
        // Build an in-memory tar.gz shaped like a real release asset:
        // sing-box-1.13.12-linux-amd64/sing-box
        let versioned_dir = "sing-box-1.13.12-linux-amd64";
        let mut builder = tar::Builder::new(Vec::new());
        let payload = b"#!/bin/sh\necho fake-sing-box\n";
        let mut header = tar::Header::new_gnu();
        header.set_size(payload.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        builder
            .append_data(&mut header, format!("{versioned_dir}/sing-box"), &payload[..])
            .expect("append");
        let tar_bytes = builder.into_inner().expect("tar finish");

        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gz.write_all(&tar_bytes).expect("gzip write");
        let compressed = gz.finish().expect("gzip finish");

        let dest = std::env::temp_dir().join(format!("sb-extract-test-{}", uuid::Uuid::new_v4()));

        tokio::runtime::Runtime::new().expect("rt").block_on(async {
            extract_tar_gz_binary(&compressed, &dest).await.expect("extract");
        });

        let installed = std::fs::read(&dest).expect("installed file");
        assert_eq!(installed, payload);
        let _ = std::fs::remove_file(&dest);
    }
}
