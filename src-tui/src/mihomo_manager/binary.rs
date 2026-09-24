//! Resolve and auto-install the mihomo core binary.

use std::io::Read as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::Context;
use sha2::Digest as _;
use tokio::process::Command;
use tokio::sync::{Mutex, OnceCell};

/// Managed (auto-downloaded) mihomo stable version — compile-time fallback
/// when GitHub API is unreachable.
pub const MIHOMO_FALLBACK_VERSION: &str = "v1.19.29";

const MIHOMO_REPO: &str = "MetaCubeX/mihomo";

/// Serialises the check-then-install sequence of `resolve_or_install`
/// within this process. An async mutex so it is held across the download
/// awaits; the cross-process half is the `flock` on [`install_lock_path`].
static INSTALL_LOCK: Mutex<()> = Mutex::const_new(());

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
    if let Some(system) = system_mihomo() {
        let version = read_mihomo_version(&system).await?.unwrap_or_else(|| "unknown".into());
        return Ok(ResolvedMihomo {
            path: system,
            source: MihomoBinarySource::System,
            version,
        });
    }

    // Only the managed binary depends on the latest release; a system
    // binary must not wait on the GitHub API.
    let target_version = latest_mihomo_version().await;
    let managed = mihomo_binary_path();
    // Hold both locks across check → download → install so concurrent
    // starts (TUI + CLI + systemd service) never race on the managed path.
    // The second caller re-checks after the lock and reuses the fresh install.
    let _in_process = INSTALL_LOCK.lock().await;
    let _cross_process = lock_install(&managed).await?;

    if let Some(version) = managed_version_if_current(&managed, target_version).await {
        ensure_executable(&managed).await?;
        return Ok(ResolvedMihomo {
            path: managed,
            source: MihomoBinarySource::ManagedCached,
            version,
        });
    }

    download_managed_mihomo(&managed, target_version).await?;
    Ok(ResolvedMihomo {
        path: managed,
        source: MihomoBinarySource::Downloaded,
        version: target_version.to_string(),
    })
}

async fn managed_version_if_current(managed: &Path, target_version: &str) -> Option<String> {
    if !managed.exists() {
        return None;
    }
    let version = read_mihomo_version(managed).await.ok().flatten()?;
    version_matches_target(&version, target_version).then_some(version)
}

/// Lock file guarding the managed binary across processes.
fn install_lock_path(managed: &Path) -> PathBuf {
    managed.with_extension("lock")
}

/// Take an exclusive `flock` on the install lock file. The lock is released
/// when the returned file is dropped (or the process dies).
async fn lock_install(managed: &Path) -> anyhow::Result<std::fs::File> {
    let lock_path = install_lock_path(managed);
    if let Some(parent) = lock_path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    tokio::task::spawn_blocking(move || {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock_path)
            .with_context(|| format!("failed to open {}", lock_path.display()))?;
        file.lock()
            .with_context(|| format!("failed to lock {}", lock_path.display()))?;
        Ok(file)
    })
    .await
    .context("install lock task panicked")?
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

/// Download, verify, and atomically install the managed mihomo build.
///
/// Callers must hold the install locks (see [`resolve_or_install`]).
/// Verification, in order:
/// 1. sha256 of the downloaded archive against the digest GitHub publishes
///    for the release asset (skipped with a warning when the API is
///    unreachable — the version check below still applies);
/// 2. the decompressed payload is an ELF executable;
/// 3. the staged binary runs and reports exactly `version`.
///
/// Only then is it renamed over `dest`, so a failed or tampered download
/// never replaces a working binary.
async fn download_managed_mihomo(dest: &Path, version: &str) -> anyhow::Result<()> {
    let asset = linux_asset_name().context("unsupported CPU architecture for auto-install")?;
    let asset_file = format!("{asset}-{version}.gz");
    let url = format!("https://github.com/{MIHOMO_REPO}/releases/download/{version}/{asset_file}");

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

    match crate::subscribe::client_meta::fetch_release_asset_digest(MIHOMO_REPO, version, &asset_file).await {
        Some(expected) => {
            verify_sha256(&compressed, &expected).with_context(|| format!("integrity check failed for {url}"))?;
            tracing::info!(target: "mihomo", "verified sha256 of {asset_file}");
        }
        None => tracing::warn!(
            target: "mihomo",
            "no published sha256 digest for {asset_file} (GitHub API unreachable?); relying on the version check"
        ),
    }

    let mut decoder = flate2::read::GzDecoder::new(compressed.as_ref());
    let mut binary = Vec::new();
    decoder
        .read_to_end(&mut binary)
        .context("failed to decompress mihomo gzip archive")?;
    ensure_elf(&binary).with_context(|| format!("{url} did not contain a Linux executable"))?;

    let parent = dest
        .parent()
        .with_context(|| format!("managed mihomo path {} has no parent", dest.display()))?;
    tokio::fs::create_dir_all(parent)
        .await
        .with_context(|| format!("failed to create {}", parent.display()))?;

    // Stage under a unique name in the destination directory so the final
    // rename is atomic and never collides with another process's staging.
    let staged = tempfile::Builder::new()
        .prefix(".mihomo-")
        .suffix(".download")
        .tempfile_in(parent)
        .with_context(|| format!("failed to create a staging file in {}", parent.display()))?;
    tokio::fs::write(staged.path(), &binary)
        .await
        .with_context(|| format!("failed to write {}", staged.path().display()))?;
    // Close the write handle before executing it (ETXTBSY otherwise); the
    // TempPath still deletes the file if anything below fails.
    let staged = staged.into_temp_path();
    ensure_executable(&staged).await?;

    let reported = read_mihomo_version(&staged)
        .await
        .context("downloaded mihomo failed to run")?
        .unwrap_or_else(|| "unknown".into());
    if !version_matches_target(&reported, version) {
        anyhow::bail!("downloaded mihomo reports version {reported}, expected {version}; refusing to install");
    }

    staged
        .persist(dest)
        .with_context(|| format!("failed to install mihomo to {}", dest.display()))?;

    tracing::info!(target: "mihomo", "installed mihomo {version}");
    Ok(())
}

/// Compare `data` against a GitHub asset digest (`sha256:<hex>`).
fn verify_sha256(data: &[u8], expected: &str) -> anyhow::Result<()> {
    let expected_hex = expected
        .strip_prefix("sha256:")
        .with_context(|| format!("unsupported digest format: {expected}"))?;
    let actual_hex: String = sha2::Sha256::digest(data)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    if !actual_hex.eq_ignore_ascii_case(expected_hex) {
        anyhow::bail!("sha256 mismatch: expected {expected_hex}, got {actual_hex}");
    }
    Ok(())
}

fn ensure_elf(binary: &[u8]) -> anyhow::Result<()> {
    if binary.starts_with(b"\x7fELF") {
        Ok(())
    } else {
        anyhow::bail!("payload is not an ELF binary ({} bytes)", binary.len())
    }
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
pub(crate) mod tests {
    use super::*;
    use std::sync::Mutex;

    pub(crate) static ENV_LOCK: Mutex<()> = Mutex::new(());

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
    fn verify_sha256_accepts_matching_digest_and_rejects_others() {
        // sha256("abc")
        let digest = "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        assert!(verify_sha256(b"abc", digest).is_ok());
        assert!(verify_sha256(b"abc", &digest.to_uppercase().replace("SHA256:", "sha256:")).is_ok());
        assert!(verify_sha256(b"abd", digest).is_err());
        assert!(verify_sha256(b"abc", "md5:900150983cd24fb0d6963f7d28e17f72").is_err());
    }

    #[test]
    fn ensure_elf_rejects_non_executables() {
        assert!(ensure_elf(b"\x7fELF\x02\x01\x01").is_ok());
        assert!(ensure_elf(b"<html>Not Found</html>").is_err());
        assert!(ensure_elf(b"").is_err());
    }

    #[tokio::test]
    async fn install_lock_is_exclusive_across_handles() {
        let dir = std::env::temp_dir().join(format!("cv-lock-{}", uuid::Uuid::new_v4()));
        let managed = dir.join("mihomo");
        let first = lock_install(&managed).await.unwrap();

        // A second, independent open of the lock file (as another process
        // would do) must not acquire the lock while the first is held.
        let other = std::fs::OpenOptions::new()
            .write(true)
            .open(install_lock_path(&managed))
            .unwrap();
        assert!(other.try_lock().is_err(), "lock must be exclusive");

        drop(first);
        assert!(other.try_lock().is_ok(), "lock must release on drop");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn linux_asset_covers_common_arches() {
        assert!(linux_asset_name().is_some() || !matches!(std::env::consts::ARCH, "x86_64" | "aarch64"));
    }
}
