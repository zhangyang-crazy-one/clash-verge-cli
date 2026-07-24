// Async reqwest fetch for subscription URLs with cookies, gzip, and SSRF protection.

use std::collections::HashMap;
use std::time::Duration;

use anyhow::Context;
use clash_verge_core::config::{IClashTemp, PrfOption};

use super::ssrf;

/// How the HTTP client should reach the subscription host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProxyMode {
    /// Disable env/system proxies (direct).
    None,
    /// Use the process environment / system proxy settings.
    System,
    /// Tunnel through the local mihomo mixed-port.
    Localhost,
}

/// Result of a subscription fetch.
pub struct FetchResult {
    pub body: String,
    pub headers: HashMap<String, String>,
    #[allow(dead_code)]
    pub final_url: String,
}

/// Fetch a subscription URL with SSRF protection and optional PrfOption knobs.
pub async fn fetch_subscription(
    url: &str,
    option: Option<&PrfOption>,
    allowlist: &[String],
) -> anyhow::Result<FetchResult> {
    ssrf::check_url_host(url, allowlist).map_err(|e| anyhow::anyhow!(e))?;

    let timeout = option.and_then(|o| o.timeout_seconds).unwrap_or(20);
    let accept_invalid = option.and_then(|o| o.danger_accept_invalid_certs).unwrap_or(false);
    let user_agent = option
        .and_then(|o| o.user_agent.as_ref())
        .map(|s| s.to_string())
        .unwrap_or_else(|| format!("clash-verge-cli/{}", env!("CARGO_PKG_VERSION")));

    let mode = proxy_mode_from_option(option);
    let mut builder = reqwest::Client::builder()
        .cookie_store(true)
        .gzip(true)
        .user_agent(user_agent)
        .timeout(Duration::from_secs(timeout))
        .connect_timeout(Duration::from_secs(10.min(timeout)))
        .redirect(reqwest::redirect::Policy::limited(10))
        .danger_accept_invalid_certs(accept_invalid);

    match mode {
        ProxyMode::None => {
            builder = builder.no_proxy();
        }
        ProxyMode::System => {
            // reqwest defaults to env proxies when no_proxy is not set.
        }
        ProxyMode::Localhost => {
            let port = IClashTemp::new().await.get_mixed_port();
            // Proxy::all covers both http and https subscription URLs through mixed-port.
            let proxy =
                reqwest::Proxy::all(format!("http://127.0.0.1:{port}")).context("invalid localhost proxy URL")?;
            builder = builder.proxy(proxy);
        }
    }

    let client = builder.build().context("failed to build reqwest client")?;

    let response = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("failed to fetch URL: {url}"))?
        .error_for_status()
        .with_context(|| format!("subscription request failed: {url}"))?;

    let final_url = response.url().to_string();
    let mut headers = HashMap::new();
    for (key, value) in response.headers() {
        if let Ok(v) = value.to_str() {
            headers.insert(key.as_str().to_ascii_lowercase(), v.to_string());
        }
    }

    let body = response.text().await.context("failed to read response body")?;

    Ok(FetchResult {
        body,
        headers,
        final_url,
    })
}

pub fn proxy_mode_from_option(option: Option<&PrfOption>) -> ProxyMode {
    let self_proxy = option.and_then(|o| o.self_proxy).unwrap_or(false);
    let with_proxy = option.and_then(|o| o.with_proxy).unwrap_or(false);
    if self_proxy {
        ProxyMode::Localhost
    } else if with_proxy {
        ProxyMode::System
    } else {
        ProxyMode::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clash_verge_core::config::PrfOption;

    #[test]
    fn proxy_mode_prefers_self_proxy() {
        let opt = PrfOption {
            self_proxy: Some(true),
            with_proxy: Some(true),
            ..Default::default()
        };
        assert_eq!(proxy_mode_from_option(Some(&opt)), ProxyMode::Localhost);
    }

    #[test]
    fn proxy_mode_system_when_with_proxy() {
        let opt = PrfOption {
            with_proxy: Some(true),
            ..Default::default()
        };
        assert_eq!(proxy_mode_from_option(Some(&opt)), ProxyMode::System);
    }

    #[test]
    fn proxy_mode_defaults_to_direct() {
        assert_eq!(proxy_mode_from_option(None), ProxyMode::None);
    }
}
