//! Resolve and auto-install the mihomo core binary.

use std::io::Read as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Mutex;

use anyhow::Context;
use tokio::process::Command;
use tokio::sync::OnceCell;

/// Managed (auto-downloaded) mihomo stable version — compile-time fallback
/// when GitHub API is unreachable.
pub const MIHOMO_FALLBACK_VERSION: &str = "v1.19.29";

const MIHOMO_REPO: &str = "MetaCubeX/mihomo";

/// Serialise concurrent `resolve_or_install` calls so two starts cannot
/// overwrite the same `$dest.download` temporary and race on `rename(2)`.
static DOWNLOAD_LOCK: Mutex<()> = Mutex::new(());

static LATEST_VERSION: OnceCell<String> = OnceCell::const_new();

/// Resolve the latest mihomo version tag from GitHub, falling back to the
/// compile-time constant when the API is unreachable.
pub async fn latest_mihomo_version() -> &'static str {
    LATEST_VERSION
        .get_or_init(|| async {
            if let Some(tag) = crate::subscribe::client_meta::fetch_latest_release_tag(MIHOMO_REPO).await {
                tracing::info!(target: "mihomo", "latest mihomo release from GitHub: {tag}");
                return tag;
            }
            tracing::warn!(
                target: "mihomo",
                "GitHub API unreachable, falling back to {MIHOMO_FALLBACK_VERSION}"
            );
            MIHOMO_FALLBACK_VERSION.to_string()
        })
        .await
        .as_str()
}

/// D-01: managed binary path. Resolves to
/// `$XDG_DATA_HOME/clash-verge-cli/mihomo` with a fallback to
/// `~/.local/share/clash-verge-cli/mihomo` for systems without XDG.
pub fn mihomo_binary_path() -> PathBuf {
    if let Some(data_dir) = std::env::var_os("XDG_DATA_HOME") {
        return PathBuf::from(data_dir).join("clash-verge-cli").join("mihomo");
    }
    let home = std::env::var_os("HOME").unwrap_or_default();
    PathBuf::from(home)
        .join(".local")
        .join("share")
        .join("clash-verge-cli")
        .join("mihomo")
}

/// Best-effort system mihomo fallback. Checks standard XDG `bin` first,
/// then common system paths.  Skips paths that are not regular files or
/// are not executable so a stale `verge-mihomo` doesn't block the managed
/// download fallback.
pub fn system_mihomo() -> Option<PathBuf> {
    let candidates = [
        dirs::executable_dir(),
        Some(PathBuf::from("/usr/bin")),
        Some(PathBuf::from("/usr/local/bin")),
    ];
    let mut seen = std::collections::HashSet::new();
    for dir in candidates.into_iter().flatten() {
        let path = dir.join("verge-mihomo");
        if seen.insert(path.clone())
            && path.is_file()
            && std::os::unix::fs::PermissionsExt::mode(&std::fs::metadata(&path).ok()?.permissions()) & 0o111 != 0
        {
            return Some(path);
        }
    }
    None
}

/// Resolve the binary that WOULD be used without downloading anything:
/// the system `verge-mihomo` if present, else the managed binary if it
/// already exists. Used by read-only TUN capability preflights (TUI toggle
/// and capability state) that must not trigger a network install.
pub fn candidate_without_install() -> Option<PathBuf> {
    system_mihomo().or_else(|| {
        let managed = mihomo_binary_path();
        managed.is_file().then_some(managed)
    })
}

/// Where the runnable mihomo binary came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MihomoBinarySource {
    /// System `verge-mihomo`.
    System,
    /// Already present managed binary at the target version.
    ManagedCached,
    /// Freshly downloaded into the managed data directory.
    Downloaded,
}

impl MihomoBinarySource {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::ManagedCached => "cached",
            Self::Downloaded => "downloaded",
        }
    }
}

/// Result of resolving (and possibly installing) mihomo.
#[derive(Debug, Clone)]
pub struct ResolvedMihomo {
    pub path: PathBuf,
    pub source: MihomoBinarySource,
    pub version: String,
}

/// Resolve a runnable mihomo binary, downloading the managed build when needed.
///
/// Preference order:
/// 1. System `verge-mihomo` (left untouched)
/// 2. Managed data-dir binary at the detected latest version (download/upgrade as needed)
pub async fn resolve_or_install() -> anyhow::Result<ResolvedMihomo> {
    let target_version = latest_mihomo_version().await;
    if let Some(system) = system_mihomo() {
        let version = read_mihomo_version(&system).await?.unwrap_or_else(|| "unknown".into());
        return Ok(ResolvedMihomo {
            path: system,
            source: MihomoBinarySource::System,
            version,
        });
    }

    let managed = mihomo_binary_path();
    if managed.exists()
        && let Ok(Some(version)) = read_mihomo_version(&managed).await
        && version_matches_target(&version, target_version)
    {
        ensure_executable(&managed).await?;
        return Ok(ResolvedMihomo {
            path: managed,
            source: MihomoBinarySource::ManagedCached,
            version,
        });
    }

    download_managed_mihomo(&managed, target_version).await?;
    ensure_executable(&managed).await?;
    Ok(ResolvedMihomo {
        path: managed,
        source: MihomoBinarySource::Downloaded,
        version: target_version.to_string(),
    })
}

/// Set the executable bit on the binary. Idempotent — if the bits are
/// already 0o755 we return success without touching the inode.
pub async fn ensure_executable(path: &Path) -> std::io::Result<()> {
    let target = std::fs::Permissions::from_mode(0o755);
    let current = tokio::fs::metadata(path).await?.permissions();
    // Mask file-type bits — mode() includes e.g., 0o100755 (regular file)
    if (current.mode() & 0o777) == target.mode() {
        return Ok(());
    }
    tokio::fs::set_permissions(path, target).await
}

async fn download_managed_mihomo(dest: &Path, version: &str) -> anyhow::Result<()> {
    let asset = linux_asset_name().context("unsupported CPU architecture for auto-install")?;
    // Serialise concurrent downloads — two starts racing on the same
    // `$dest.download` temp file can cause a rename(2) to fail.
    // Use a block so the MutexGuard is dropped before the first await,
    // keeping the future `Send`.
    {
        let _guard = DOWNLOAD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // Critical section: guard is dropped at the closing brace.
    }
    let url = format!(
        "https://github.com/MetaCubeX/mihomo/releases/download/{version}/{asset}-{version}.gz",
        version = version,
        asset = asset
    );

    tracing::info!(
        target: "mihomo",
        "downloading mihomo {version} → {}",
        dest.display()
    );

    let client = reqwest::Client::builder()
        .user_agent(format!("clash-verge-cli/{}", env!("CARGO_PKG_VERSION")))
        .timeout(std::time::Duration::from_secs(120))
        .build()
        .context("failed to build download client")?;

    let response = client
        .get(&url)
        .send()
        .await
        .with_context(|| format!("failed to download mihomo from {url}"))?
        .error_for_status()
        .with_context(|| format!("mihomo download returned error for {url}"))?;

    let compressed = response.bytes().await.context("failed to read mihomo download body")?;

    let mut decoder = flate2::read::GzDecoder::new(compressed.as_ref());
    let mut binary = Vec::new();
    decoder
        .read_to_end(&mut binary)
        .context("failed to decompress mihomo gzip archive")?;

    if let Some(parent) = dest.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    let tmp = dest.with_extension("download");
    tokio::fs::write(&tmp, &binary)
        .await
        .with_context(|| format!("failed to write {}", tmp.display()))?;
    tokio::fs::rename(&tmp, dest)
        .await
        .with_context(|| format!("failed to install mihomo to {}", dest.display()))?;

    tracing::info!(target: "mihomo", "installed mihomo {version}");
    Ok(())
}

fn linux_asset_name() -> Option<&'static str> {
    match std::env::consts::ARCH {
        "x86_64" => Some("mihomo-linux-amd64-v2"),
        "aarch64" => Some("mihomo-linux-arm64"),
        "arm" => Some("mihomo-linux-armv7"),
        "riscv64" => Some("mihomo-linux-riscv64"),
        // MetaCubeX ships loongarch as abi1/abi2-specific assets; do not guess.
        "loongarch64" => None,
        _ => None,
    }
}

async fn read_mihomo_version(path: &Path) -> anyhow::Result<Option<String>> {
    let output = Command::new(path)
        .arg("-v")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .with_context(|| format!("failed to execute {}", path.display()))?;

    let text = String::from_utf8_lossy(&output.stdout);
    let err = String::from_utf8_lossy(&output.stderr);
    let combined = format!("{text}{err}");
    Ok(extract_version_token(&combined))
}

fn extract_version_token(text: &str) -> Option<String> {
    // Examples: "Mihomo Meta v1.19.29", "v1.19.29"
    for token in text.split_whitespace() {
        let trimmed = token.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '.' && c != '-');
        if trimmed.starts_with('v') && trimmed.contains('.') {
            return Some(trimmed.to_string());
        }
        if trimmed.chars().next().is_some_and(|c| c.is_ascii_digit()) && trimmed.contains('.') {
            return Some(format!("v{trimmed}"));
        }
    }
    None
}

fn version_matches_target(version: &str, target: &str) -> bool {
    let normalized = if version.starts_with('v') {
        version.to_string()
    } else {
        format!("v{version}")
    };
    normalized == target
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn test_mihomo_binary_path_uses_xdg_data_home() {
        let _guard = ENV_LOCK.lock().unwrap();
        let prev = std::env::var_os("XDG_DATA_HOME");
        // SAFETY: this is a single-threaded test runner for these tests.
        unsafe {
            std::env::set_var("XDG_DATA_HOME", "/tmp/test-xdg");
        }

        let path = mihomo_binary_path();
        assert!(path.ends_with("clash-verge-cli/mihomo"), "got {path:?}");

        match prev {
            Some(v) => unsafe { std::env::set_var("XDG_DATA_HOME", v) },
            None => unsafe { std::env::remove_var("XDG_DATA_HOME") },
        }
    }

    #[test]
    fn test_mihomo_binary_path_falls_back_to_home() {
        let _guard = ENV_LOCK.lock().unwrap();
        let prev_xdg = std::env::var_os("XDG_DATA_HOME");
        let prev_home = std::env::var_os("HOME");
        unsafe {
            std::env::remove_var("XDG_DATA_HOME");
            std::env::set_var("HOME", "/tmp/fake-home");
        }

        let path = mihomo_binary_path();
        assert!(
            path.starts_with("/tmp/fake-home/.local/share/clash-verge-cli/mihomo"),
            "got {path:?}"
        );

        match prev_xdg {
            Some(v) => unsafe { std::env::set_var("XDG_DATA_HOME", v) },
            None => unsafe { std::env::remove_var("XDG_DATA_HOME") },
        }
        match prev_home {
            Some(v) => unsafe { std::env::set_var("HOME", v) },
            None => unsafe { std::env::remove_var("HOME") },
        }
    }

    #[test]
    fn extracts_version_from_mihomo_output() {
        assert_eq!(
            extract_version_token("Mihomo Meta v1.19.29 linux amd64"),
            Some("v1.19.29".into())
        );
        assert_eq!(extract_version_token("v1.19.29"), Some("v1.19.29".into()));
        assert!(version_matches_target("v1.19.29", "v1.19.29"));
        assert!(!version_matches_target("v1.19.25", "v1.19.29"));
    }

    #[test]
    fn candidate_without_install_prefers_system_binary() {
        // A regular executable file in XDG bin is preferred over a managed
        // path; neither triggers a download.
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = std::env::temp_dir().join(format!("cv-bin-{}", uuid::Uuid::new_v4()));
        let _ = std::fs::create_dir_all(&dir);
        let sys = dir.join("verge-mihomo");
        let _ = std::fs::write(&sys, b"#!/bin/sh\n");
        let _ = std::fs::set_permissions(&sys, std::fs::Permissions::from_mode(0o755));

        let old_exec = std::env::var_os("XDG_DATA_HOME");
        let old_exec_dir = std::env::var_os("XDG_BIN_HOME");
        unsafe {
            std::env::remove_var("XDG_DATA_HOME");
            std::env::set_var("XDG_BIN_HOME", &dir);
        }

        let candidate = candidate_without_install();
        assert_eq!(candidate.as_deref(), Some(sys.as_path()));

        match old_exec {
            Some(v) => unsafe { std::env::set_var("XDG_DATA_HOME", v) },
            None => unsafe { std::env::remove_var("XDG_DATA_HOME") },
        }
        match old_exec_dir {
            Some(v) => unsafe { std::env::set_var("XDG_BIN_HOME", v) },
            None => unsafe { std::env::remove_var("XDG_BIN_HOME") },
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn candidate_without_install_is_a_no_download_probe() {
        // candidate_without_install must mirror the resolve preference
        // (system first, then existing managed) without downloading and
        // without mutating anything. Works on hosts with or without a
        // system verge-mihomo.
        let _guard = ENV_LOCK.lock().unwrap();
        let candidate = candidate_without_install();
        match system_mihomo() {
            Some(sys) => assert_eq!(candidate.as_deref(), Some(sys.as_path())),
            None => {
                let managed = mihomo_binary_path();
                if managed.is_file() {
                    assert_eq!(candidate.as_deref(), Some(managed.as_path()));
                } else {
                    assert!(candidate.is_none());
                }
            }
        }
    }

    #[test]
    fn linux_asset_covers_common_arches() {
        assert!(linux_asset_name().is_some() || !matches!(std::env::consts::ARCH, "x86_64" | "aarch64"));
    }
}
