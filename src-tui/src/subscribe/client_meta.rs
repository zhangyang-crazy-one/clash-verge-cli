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

/// Shared: the `digest` GitHub publishes for one asset of the `tag` release
/// of `owner/repo` (e.g. `sha256:<hex>`). `None` on any failure or when the
/// release predates GitHub's asset digests.
pub(crate) async fn fetch_release_asset_digest(owner_repo: &str, tag: &str, asset_name: &str) -> Option<String> {
    let url = format!("https://api.github.com/repos/{owner_repo}/releases/tags/{tag}");
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
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
    asset_digest(&payload, asset_name)
}

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

    #[tokio::test]
    async fn default_user_agent_matches_gui_prefix() {
        let ua = default_subscription_user_agent().await;
        assert!(ua.starts_with("clash-verge/v"), "expected GUI-style UA, got {ua}");
        assert!(!ua.contains("clash-verge-cli"));
    }
}
