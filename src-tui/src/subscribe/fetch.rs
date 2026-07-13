// Async reqwest fetch for subscription URLs with cookies, gzip, and SSRF protection.

use anyhow::Context;

use super::ssrf;

/// Result of a subscription fetch.
pub struct FetchResult {
    pub body: String,
}

/// Fetch a subscription URL with SSRF protection.
///
/// Uses reqwest with cookies enabled, gzip decompression, and a
/// configurable allowlist for private hosts.
pub async fn fetch_subscription(url: &str, allowlist: &[String]) -> anyhow::Result<FetchResult> {
    ssrf::check_url_host(url, allowlist).map_err(|e| anyhow::anyhow!(e))?;

    let client = reqwest::Client::builder()
        .cookie_store(true)
        .gzip(true)
        .user_agent("clash-verge-cli/0.1")
        .timeout(std::time::Duration::from_secs(30))
        .connect_timeout(std::time::Duration::from_secs(10))
        .build()
        .context("failed to build reqwest client")?;

    let response = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("failed to fetch URL: {url}"))?
        .error_for_status()
        .with_context(|| format!("subscription request failed: {url}"))?;

    let body = response.text().await.context("failed to read response body")?;

    Ok(FetchResult { body })
}
