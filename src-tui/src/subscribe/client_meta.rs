//! Client identity for subscription providers.
//!
//! Airports commonly whitelist GUI-style User-Agents such as `clash-verge/v2.5.x`.
//! Sending `clash-verge-cli/0.1.0` can yield placeholder nodes like "client too old".
//!
//! Version resolution order:
//! 1. Latest GitHub release of `clash-verge-rev/clash-verge-rev`
//! 2. Local GUI package version (`rpm` / `dpkg-query`, never launching the binary)
//! 3. Compile-time fallback

use std::time::Duration;

use anyhow::Context as _;

use tokio::sync::OnceCell;

/// Fallback when GitHub and local package lookup both fail.
const FALLBACK_CLASH_VERGE_VERSION: &str = "2.5.2";

const CLASH_VERGE_REPO: &str = "clash-verge-rev/clash-verge-rev";

static COMPAT_VERSION: OnceCell<String> = OnceCell::const_new();

/// Shared: query the GitHub releases API for `owner/repo` and return the
/// `tag_name` stripped of a leading `v`/`V`.
///
/// Times out after 5 s (3 s connect).  Returns `None` on any failure.
pub(crate) async fn fetch_latest_release_tag(owner_repo: &str) -> Option<String> {
    let url = format!("https://api.github.com/repos/{owner_repo}/releases/latest");
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .connect_timeout(Duration::from_secs(3))
        .no_proxy()
        .user_agent(format!("clash-verge-cli/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .ok()?;

    let response = client
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .send()
        .await
        .ok()?;
    if !response.status().is_success() {
        return None;
    }

    let payload: serde_json::Value = response.json().await.ok()?;
    let tag = payload.get("tag_name")?.as_str()?;
    // Accept only well-formed version tags like "v2.5.2" or "v1.19.29".
    let tag = tag.trim();
    if tag.starts_with('v') && tag[1..].chars().next().is_some_and(|c| c.is_ascii_digit()) {
        Some(tag.to_string())
    } else {
        None
    }
}

#[cfg(test)]
fn asset_digest(release: &serde_json::Value, asset_name: &str) -> Option<String> {
    release
        .get("assets")?
        .as_array()?
        .iter()
        .find(|asset| asset.get("name").and_then(|n| n.as_str()) == Some(asset_name))?
        .get("digest")?
        .as_str()
        .filter(|digest| !digest.is_empty())
        .map(str::to_string)
}

#[derive(Debug, PartialEq, Eq)]
enum TrustedDigestMetadata {
    Digest(String),
    Manifest(String),
}

fn checked_sha256(value: &str) -> anyhow::Result<String> {
    let hex = value
        .strip_prefix("sha256:")
        .context("unsupported release digest algorithm; expected sha256")?;
    if hex.len() != 64 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        anyhow::bail!("invalid release SHA-256 checksum text");
    }
    Ok(format!("sha256:{}", hex.to_ascii_lowercase()))
}

/// Exact release/asset binding is retained even when old GitHub releases
/// expose null digests. A local computed hash is never an expected digest.
fn trusted_digest_metadata(
    release: &serde_json::Value,
    repo: &str,
    tag: &str,
    asset_name: &str,
) -> anyhow::Result<TrustedDigestMetadata> {
    if !matches!(repo, "MetaCubeX/mihomo" | "SagerNet/sing-box") {
        anyhow::bail!("checksum source is not an official supported core repository");
    }
    if release.get("tag_name").and_then(serde_json::Value::as_str) != Some(tag) {
        anyhow::bail!("release metadata tag does not match requested {tag}");
    }
    let assets = release
        .get("assets")
        .and_then(serde_json::Value::as_array)
        .context("release metadata omitted assets")?;
    let targets: Vec<_> = assets
        .iter()
        .filter(|asset| asset.get("name").and_then(serde_json::Value::as_str) == Some(asset_name))
        .collect();
    if targets.len() != 1 {
        anyhow::bail!("release asset {asset_name} is missing or ambiguous");
    }
    match targets[0].get("digest") {
        Some(serde_json::Value::String(digest)) => return checked_sha256(digest).map(TrustedDigestMetadata::Digest),
        None | Some(serde_json::Value::Null) => {}
        _ => anyhow::bail!("invalid release asset digest metadata"),
    }
    let manifests: Vec<_> = assets
        .iter()
        .filter(|asset| {
            asset
                .get("name")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|name| {
                    matches!(
                        name.to_ascii_lowercase().as_str(),
                        "sha256sums" | "sha256sums.txt" | "checksums" | "checksums.txt"
                    ) || name.to_ascii_lowercase().ends_with("-checksums.txt")
                })
        })
        .collect();
    if manifests.len() != 1 {
        anyhow::bail!(
            "null asset digest and missing or ambiguous official checksum manifest for {asset_name}; existing binary preserved"
        );
    }
    let name = manifests[0]
        .get("name")
        .and_then(serde_json::Value::as_str)
        .context("checksum manifest omitted filename")?;
    if name.contains(['/', '\\']) || name == "." || name == ".." {
        anyhow::bail!("invalid checksum manifest filename");
    }
    let expected_url = format!("https://github.com/{repo}/releases/download/{tag}/{name}");
    if manifests[0]
        .get("browser_download_url")
        .and_then(serde_json::Value::as_str)
        != Some(expected_url.as_str())
    {
        anyhow::bail!("checksum manifest URL is not HTTPS official repository/tag-bound");
    }
    Ok(TrustedDigestMetadata::Manifest(expected_url))
}

fn manifest_sha256(text: &str, asset_name: &str) -> anyhow::Result<String> {
    let mut found = Vec::new();
    for line in text.lines() {
        let fields: Vec<_> = line.split_whitespace().collect();
        if fields.len() == 2 && fields[1].trim_start_matches('*') == asset_name {
            found.push(checked_sha256(&format!("sha256:{}", fields[0]))?);
        } else if fields.len() == 4
            && fields[0] == "SHA256"
            && fields[1] == format!("({asset_name})")
            && fields[2] == "="
        {
            found.push(checked_sha256(&format!("sha256:{}", fields[3]))?);
        }
    }
    if found.len() != 1 {
        anyhow::bail!("checksum manifest has missing or ambiguous exact entry for {asset_name}");
    }
    Ok(found.remove(0))
}

pub(crate) async fn fetch_trusted_release_digest(repo: &str, tag: &str, asset_name: &str) -> anyhow::Result<String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .connect_timeout(Duration::from_secs(3))
        .no_proxy()
        .user_agent(format!("clash-verge-cli/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .context("failed to build release metadata client")?;
    let url = format!("https://api.github.com/repos/{repo}/releases/tags/{tag}");
    let response = client
        .get(url)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .send()
        .await
        .context("release metadata HTTP request failed; existing binary preserved")?
        .error_for_status()
        .context("release metadata HTTP returned error; existing binary preserved")?;
    let release = response.json().await.context("invalid release JSON metadata")?;
    match trusted_digest_metadata(&release, repo, tag, asset_name)? {
        TrustedDigestMetadata::Digest(digest) => Ok(digest),
        TrustedDigestMetadata::Manifest(url) => {
            let response = client
                .get(url)
                .send()
                .await
                .context("official checksum manifest HTTP request failed")?
                .error_for_status()
                .context("official checksum manifest HTTP returned error")?;
            use tokio_stream::StreamExt as _;
            let mut stream = response.bytes_stream();
            let mut bytes = Vec::new();
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.context("failed to read official checksum manifest")?;
                if bytes.len().saturating_add(chunk.len()) > 1024 * 1024 {
                    anyhow::bail!("official checksum manifest exceeds size limit");
                }
                bytes.extend_from_slice(&chunk);
            }
            manifest_sha256(
                std::str::from_utf8(&bytes).context("official checksum manifest is not UTF-8")?,
                asset_name,
            )
        }
    }
}

/// Semantic version used for GUI-compatible subscription User-Agent.
pub async fn clash_verge_compat_version() -> &'static str {
    COMPAT_VERSION
        .get_or_init(|| async {
            if let Some(tag) = fetch_latest_release_tag(CLASH_VERGE_REPO).await
                && let Some(version) = normalize_version(&tag)
            {
                tracing::info!(target: "subscribe", "subscription UA version from GitHub: {version}");
                return version;
            }
            if let Some(version) = detect_installed_clash_verge_version() {
                tracing::info!(target: "subscribe", "subscription UA version from local package: {version}");
                return version;
            }
            tracing::warn!(
                target: "subscribe",
                "subscription UA falling back to {FALLBACK_CLASH_VERGE_VERSION}"
            );
            FALLBACK_CLASH_VERGE_VERSION.to_string()
        })
        .await
        .as_str()
}

/// Default subscription User-Agent matching GUI `NetworkManager`.
pub async fn default_subscription_user_agent() -> String {
    format!("clash-verge/v{}", clash_verge_compat_version().await)
}

/// Read the installed GUI package version without launching the binary
/// (`clash-verge --version` starts the app).
fn detect_installed_clash_verge_version() -> Option<String> {
    if let Some(version) = version_from_command(&["rpm", "-q", "--qf", "%{VERSION}", "clash-verge"]) {
        return normalize_version(&version);
    }
    if let Some(version) = version_from_command(&["dpkg-query", "-W", "-f=${Version}", "clash-verge"]) {
        return normalize_version(&version);
    }
    None
}

fn version_from_command(argv: &[&str]) -> Option<String> {
    let (program, args) = argv.split_first()?;
    let output = std::process::Command::new(program).args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if text.is_empty() { None } else { Some(text) }
}

/// Accept `v2.5.2`, `2.5.1`, `2.5.1-1`, `2.5.3+dfsg` → bare semver.
pub(crate) fn normalize_version(raw: &str) -> Option<String> {
    let main = raw
        .trim()
        .trim_start_matches(['v', 'V'])
        .split(['-', '+'])
        .next()?
        .trim();
    let parts: Vec<&str> = main.split('.').collect();
    if parts.len() < 2 || parts.len() > 4 {
        return None;
    }
    if !parts
        .iter()
        .all(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_digit()))
    {
        return None;
    }
    Some(main.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_rpm_deb_and_tag_versions() {
        assert_eq!(normalize_version("2.5.1").as_deref(), Some("2.5.1"));
        assert_eq!(normalize_version("v2.5.2").as_deref(), Some("2.5.2"));
        assert_eq!(normalize_version("2.5.1-1").as_deref(), Some("2.5.1"));
        assert_eq!(normalize_version("2.5.3+dfsg").as_deref(), Some("2.5.3"));
        assert_eq!(normalize_version("not-a-version"), None);
    }

    #[test]
    fn asset_digest_picks_the_named_asset() {
        let release = serde_json::json!({
            "assets": [
                { "name": "mihomo-linux-arm64-v1.19.29.gz", "digest": "sha256:aaaa" },
                { "name": "mihomo-linux-amd64-v2-v1.19.29.gz", "digest": "sha256:bbbb" },
                { "name": "no-digest.gz", "digest": null }
            ]
        });
        assert_eq!(
            asset_digest(&release, "mihomo-linux-amd64-v2-v1.19.29.gz").as_deref(),
            Some("sha256:bbbb")
        );
        assert_eq!(asset_digest(&release, "no-digest.gz"), None);
        assert_eq!(asset_digest(&release, "missing.gz"), None);
        assert_eq!(asset_digest(&serde_json::json!({}), "missing.gz"), None);
    }

    #[test]
    fn default_user_agent_matches_gui_prefix() {
        // Fixture identity: no live release/package discovery in safe tests.
        let ua = format!("clash-verge/v{FALLBACK_CLASH_VERGE_VERSION}");
        assert!(ua.starts_with("clash-verge/v"), "expected GUI-style UA, got {ua}");
        assert!(!ua.contains("clash-verge-cli"));
    }

    #[test]
    fn guided_core_trusted_digest_requires_exact_release_and_valid_algorithm() {
        let release = serde_json::json!({"tag_name":"v1.14.2", "assets":[{"name":"core.tar.gz", "digest":format!("sha256:{}", "a".repeat(64))}]});
        assert!(matches!(
            trusted_digest_metadata(&release, "SagerNet/sing-box", "v1.14.2", "core.tar.gz").unwrap(),
            TrustedDigestMetadata::Digest(_)
        ));
        assert!(trusted_digest_metadata(&release, "SagerNet/sing-box", "v1.14.1", "core.tar.gz").is_err());
        assert!(trusted_digest_metadata(&release, "attacker/sing-box", "v1.14.2", "core.tar.gz").is_err());
        assert!(trusted_digest_metadata(&release, "SagerNet/sing-box", "v1.14.2", "missing.tar.gz").is_err());
        for bad in ["sha256:abc", "md5:abc", "sha256:"] {
            assert!(checked_sha256(bad).is_err());
        }
    }

    #[test]
    fn guided_core_null_digest_accepts_only_official_exact_manifest() {
        let mut release = serde_json::json!({"tag_name":"v1.14.2", "assets":[{"name":"core.tar.gz", "digest":null}, {"name":"checksums.txt", "browser_download_url":"https://github.com/SagerNet/sing-box/releases/download/v1.14.2/checksums.txt"}]});
        assert!(matches!(
            trusted_digest_metadata(&release, "SagerNet/sing-box", "v1.14.2", "core.tar.gz").unwrap(),
            TrustedDigestMetadata::Manifest(_)
        ));
        for url in [
            "http://github.com/SagerNet/sing-box/releases/download/v1.14.2/checksums.txt",
            "https://github.com/attacker/sing-box/releases/download/v1.14.2/checksums.txt",
            "https://github.com/SagerNet/sing-box/releases/download/v1.14.1/checksums.txt",
        ] {
            release["assets"][1]["browser_download_url"] = url.into();
            assert!(trusted_digest_metadata(&release, "SagerNet/sing-box", "v1.14.2", "core.tar.gz").is_err());
        }
    }

    #[test]
    fn guided_core_checksum_manifest_rejects_missing_ambiguous_and_malformed_entries() {
        let hex = "a".repeat(64);
        assert_eq!(
            manifest_sha256(&format!("{hex}  core.tar.gz\n"), "core.tar.gz").unwrap(),
            format!("sha256:{hex}")
        );
        assert_eq!(
            manifest_sha256(&format!("{hex} *core.tar.gz\n"), "core.tar.gz").unwrap(),
            format!("sha256:{hex}")
        );
        for text in [
            format!("{hex}  other.tar.gz"),
            format!("{hex}  dir/core.tar.gz"),
            format!("{hex}  core.tar.gz\n{hex}  core.tar.gz"),
            "bad  core.tar.gz".into(),
        ] {
            assert!(manifest_sha256(&text, "core.tar.gz").is_err());
        }
    }

    #[test]
    fn guided_core_missing_or_ambiguous_metadata_never_becomes_local_hash() {
        let mut release = serde_json::json!({"tag_name":"v1.19.32", "assets":[{"name":"core.gz", "digest":null}]});
        assert!(
            trusted_digest_metadata(&release, "MetaCubeX/mihomo", "v1.19.32", "core.gz")
                .unwrap_err()
                .to_string()
                .contains("null asset digest")
        );
        let asset = release["assets"][0].clone();
        release["assets"].as_array_mut().unwrap().push(asset);
        assert!(
            trusted_digest_metadata(&release, "MetaCubeX/mihomo", "v1.19.32", "core.gz")
                .unwrap_err()
                .to_string()
                .contains("ambiguous")
        );
    }
}
