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
use sha2::Digest as _;
use tokio::process::Command;

/// Pinned sing-box policy target. Release discovery is informational and does
/// not select a launch version; moving this pin requires explicit review of
/// the supported configuration capabilities.
pub const SINGBOX_FALLBACK_VERSION: &str = "v1.14.2";

const SINGBOX_REPO: &str = "SagerNet/sing-box";

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
    super::binary::managed_binary_path(
        std::env::var_os("XDG_DATA_HOME").map(PathBuf::from).as_deref(),
        std::env::var_os("HOME").map(PathBuf::from).as_deref(),
        "sing-box",
    )
}

/// Best-effort system sing-box fallback: `verge-sing-box` in standard bin
/// dirs first, then a plain `sing-box` found on `$PATH`.
fn system_singbox() -> Option<PathBuf> {
    system_singbox_candidates().into_iter().next()
}

pub(crate) fn system_singbox_candidates() -> Vec<PathBuf> {
    let mut seen = std::collections::HashSet::new();
    let mut found = Vec::new();

    let named_dirs = [
        dirs::executable_dir(),
        Some(PathBuf::from("/usr/bin")),
        Some(PathBuf::from("/usr/local/bin")),
    ];
    for dir in named_dirs.into_iter().flatten() {
        let path = dir.join("verge-sing-box");
        if seen.insert(path.clone()) && is_executable_file(&path) {
            found.push(path);
        }
    }

    if let Some(path_var) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path_var) {
            let path = dir.join("sing-box");
            if seen.insert(path.clone()) && is_executable_file(&path) {
                found.push(path);
            }
        }
    }
    found
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
    super::binary::candidate_from_paths(system_singbox(), &singbox_binary_path(), super::CoreKind::SingBox)
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
/// 2. Validated managed cache, or the pinned managed build
pub async fn resolve_or_install() -> anyhow::Result<ResolvedSingBox> {
    use super::binary::{CoreInspection, DownloadAuthorization};
    let kind = super::CoreKind::SingBox;
    let candidate = match super::binary::inspect(kind).await {
        CoreInspection::Ready(candidate) => candidate,
        CoreInspection::Review(diagnostic) => anyhow::bail!("{diagnostic}"),
        CoreInspection::NeedsUpdate(_) => {
            super::binary::acquire(kind, DownloadAuthorization::confirmed(kind, 0), None).await?
        }
    };
    Ok(ResolvedSingBox {
        path: candidate.path,
        source: match candidate.source.as_str() {
            "system" => SingboxBinarySource::System,
            "cached" => SingboxBinarySource::ManagedCached,
            _ => SingboxBinarySource::Downloaded,
        },
        version: candidate.version,
    })
}

/// Probe availability without side effects: does NOT download or mutate.
pub async fn read_singbox_version(path: &Path) -> anyhow::Result<Option<String>> {
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        Command::new(path)
            .arg("version")
            .kill_on_drop(true)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output(),
    )
    .await
    .context("sing-box version probe timed out")?
    .with_context(|| format!("failed to execute {}", path.display()))?;

    let text = String::from_utf8_lossy(&output.stdout);
    if !output.status.success() {
        anyhow::bail!(
            "sing-box version exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    parse_singbox_version_output(&text).map(Some)
}

/// Parse `sing-box version` output. Example:
/// ```text
/// Version: 1.13.12
/// Environment: linux, amd64
/// Tags: with_gvisor,with_quic,...
/// ```
fn parse_singbox_version_output(text: &str) -> anyhow::Result<String> {
    let mut found = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        let candidate = line.strip_prefix("Version:").map(str::trim).or_else(|| {
            line.strip_prefix("sing-box version ")
                .map(|s| s.split_whitespace().next().unwrap_or(""))
        });
        if let Some(value) = candidate {
            let value = value.trim_start_matches('v');
            super::core_policy::Version::parse(value)
                .with_context(|| format!("malformed sing-box version output line: {line}"))?;
            found.push(value.to_string());
        }
    }
    if found.is_empty() {
        anyhow::bail!("sing-box version output omitted a complete version")
    }
    if found.iter().any(|v| v != &found[0]) {
        anyhow::bail!(
            "sing-box version output contains conflicting versions: {}",
            found.join(", ")
        )
    }
    Ok(found.remove(0))
}

/// Compare an installed version string (`1.14.2`) against a target tag
/// (`v1.13.12`), tolerating the missing/extra leading `v`.
fn version_matches_target(version: &str, target: &str) -> bool {
    super::core_policy::Version::parse(version).ok() == super::core_policy::Version::parse(target).ok()
}

pub(crate) async fn download_managed_singbox(
    dest: &Path,
    version: &str,
    progress: Option<(
        super::binary::DownloadAuthorization,
        Option<&tokio::sync::mpsc::Sender<super::binary::CoreProgress>>,
    )>,
) -> anyhow::Result<String> {
    let arch = linux_arch_name().context("unsupported CPU architecture for auto-install")?;
    // Release assets carry the version WITHOUT the leading `v`:
    // https://github.com/SagerNet/sing-box/releases/download/v1.14.2/sing-box-1.14.2-linux-amd64.tar.gz
    let bare = version.strip_prefix('v').unwrap_or(version);
    let asset_dir = format!("sing-box-{bare}-linux-{arch}");
    let url = format!("https://github.com/{SINGBOX_REPO}/releases/download/{version}/{asset_dir}.tar.gz");

    tracing::info!(target: "singbox", "downloading sing-box {version} → {}", dest.display());

    let client = reqwest::Client::builder()
        .user_agent(format!("clash-verge-cli/{}", env!("CARGO_PKG_VERSION")))
        .timeout(std::time::Duration::from_secs(180))
        .build()
        .context("failed to build download client")?;

    let asset_file = format!("{asset_dir}.tar.gz");
    let digest =
        crate::subscribe::client_meta::fetch_trusted_release_digest(SINGBOX_REPO, version, &asset_file).await?;
    let response = client
        .get(&url)
        .send()
        .await
        .with_context(|| format!("failed to download sing-box from {url}"))?
        .error_for_status()
        .with_context(|| format!("sing-box download returned error for {url}"))?;

    use tokio_stream::StreamExt as _;
    let mut stream = response.bytes_stream();
    let mut compressed = Vec::new();
    let mut hasher = sha2::Sha256::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("failed to read sing-box download stream")?;
        if compressed.len().saturating_add(chunk.len()) > 256 * 1024 * 1024 {
            anyhow::bail!("sing-box archive exceeds download size limit");
        }
        hasher.update(&chunk);
        compressed.extend_from_slice(&chunk);
    }
    if let Some((authorization, sender)) = progress {
        super::binary::report_progress(sender, authorization, super::binary::CoreProgressPhase::Verifying, None);
    }
    let actual = format!(
        "sha256:{}",
        hasher
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    );
    super::binary::verify_sha256_digest(&actual, &digest).context("sing-box archive integrity check failed")?;

    let installed_version = extract_tar_gz_binary(&compressed, dest, version).await?;

    tracing::info!(target: "singbox", "installed sing-box {version}");
    Ok(installed_version)
}

/// Extract the core binary from a sing-box release tarball.
///
/// The archive contains a top-level versioned directory holding the
/// `sing-box` binary; we locate any entry whose file name is exactly
/// `sing-box` regardless of directory depth.
async fn extract_tar_gz_binary(compressed: &[u8], dest: &Path, expected_version: &str) -> anyhow::Result<String> {
    extract_tar_gz_binary_with_probe(compressed, dest, expected_version, |path| async move {
        read_singbox_version(&path).await
    })
    .await
}

async fn extract_tar_gz_binary_with_probe<F, Fut>(
    compressed: &[u8],
    dest: &Path,
    expected_version: &str,
    probe: F,
) -> anyhow::Result<String>
where
    F: FnOnce(PathBuf) -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<Option<String>>>,
{
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
                if entry.size() > 512 * 1024 * 1024 {
                    anyhow::bail!("sing-box executable exceeds size limit");
                }
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
    super::binary::install_verified_payload(super::CoreKind::SingBox, dest, &payload, expected_version, probe).await
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
    fn parses_official_and_installed_singbox_output_shapes() {
        assert_eq!(
            parse_singbox_version_output("sing-box version 1.14.2 (go1.24.2 linux/amd64)\nTags: with_gvisor\n")
                .unwrap(),
            "1.14.2"
        );
        assert_eq!(
            parse_singbox_version_output("Version: 1.14.2\nEnvironment: linux, amd64\n").unwrap(),
            "1.14.2"
        );
        assert!(parse_singbox_version_output("no version here").is_err());
        assert!(parse_singbox_version_output("sing-box version 1.14\n").is_err());
        assert!(parse_singbox_version_output("sing-box version 1.14.2\nVersion: 1.13.21\n").is_err());
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
    fn singbox_managed_path_uses_xdg_data_home_without_environment_mutation() {
        let path = super::super::binary::managed_binary_path(
            Some(Path::new("/tmp/test-xdg-sb")),
            Some(Path::new("/ignored-home")),
            "sing-box",
        );
        assert!(path.ends_with("clash-verge-cli/sing-box"), "got {path:?}");
        assert!(path.starts_with("/tmp/test-xdg-sb"));
    }

    #[test]
    fn guided_core_extracts_nested_singbox_binary_with_captured_probe() {
        // Build an in-memory tar.gz shaped like a release asset.
        let versioned_dir = "sing-box-1.14.2-linux-amd64";
        let mut builder = tar::Builder::new(Vec::new());
        let payload = b"\x7fELFcaptured-fixture-no-execution";
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
            extract_tar_gz_binary_with_probe(&compressed, &dest, "v1.14.2", |_| async { Ok(Some("1.14.2".into())) })
                .await
                .expect("extract");
        });

        let installed = std::fs::read(&dest).expect("installed file");
        assert_eq!(installed, payload);
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            assert!(super::super::binary::verify_cached_digest(&dest).await.unwrap());
        });
        let digest = sha2::Sha256::digest(&installed)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let receipt_path = dest.with_extension(format!("sha256.{digest}"));
        let _ = std::fs::remove_file(&dest);
        let _ = std::fs::remove_file(receipt_path);
    }
}
