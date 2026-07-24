//! Async reqwest fetch for subscription URLs with cookies, gzip, and SSRF protection.

use std::collections::HashMap;
use std::time::Duration;

use anyhow::Context;
use base64::{Engine as _, engine::general_purpose};
use clash_verge_core::config::{IClashTemp, PrfOption};
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue};
use url::Url;

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
    let (request_url, auth_headers) = prepare_request_url(url)?;

    let response = match client.get(request_url).headers(auth_headers).send().await {
        Ok(resp) => resp,
        Err(err) => return Err(context_fetch_error(err, url)),
    };

    let response = response
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

/// Strip URL userinfo into an Authorization header.
///
/// Empty passwords still produce `Basic user:` (upstream NetworkManager behavior).
fn prepare_request_url(url: &str) -> anyhow::Result<(Url, HeaderMap)> {
    let mut parsed = Url::parse(url).with_context(|| format!("invalid subscription URL: {url}"))?;
    let mut headers = HeaderMap::new();

    if !parsed.username().is_empty() {
        let username = percent_encoding::percent_decode_str(parsed.username())
            .decode_utf8_lossy()
            .into_owned();
        let password = percent_encoding::percent_decode_str(parsed.password().unwrap_or_default())
            .decode_utf8_lossy()
            .into_owned();
        let encoded = general_purpose::STANDARD.encode(format!("{username}:{password}"));
        let value = HeaderValue::from_str(&format!("Basic {encoded}"))
            .context("invalid Basic Auth header value")?;
        headers.insert(AUTHORIZATION, value);
    }

    let _ = parsed.set_username("");
    let _ = parsed.set_password(None);
    Ok((parsed, headers))
}

fn context_fetch_error(err: reqwest::Error, url: &str) -> anyhow::Error {
    let legacy_tls = is_legacy_tls_protocol_error(&err);
    let err = anyhow::Error::new(err).context(format!("failed to fetch URL: {url}"));
    if legacy_tls {
        err.context(
            "Subscription server uses legacy TLS; only TLS 1.2/1.3 is supported. TLS 1.0/1.1 is insecure",
        )
    } else {
        err
    }
}

fn is_legacy_tls_protocol_error(err: &(dyn std::error::Error + 'static)) -> bool {
    let detail = format!("{err:#?}").to_ascii_lowercase();
    detail.contains("protocolversion") || detail.contains("protocol version")
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

    #[test]
    fn empty_password_still_emits_basic_auth() {
        let (url, headers) = prepare_request_url("https://user:@example.com/sub.yaml").expect("url");
        assert!(url.username().is_empty());
        assert!(url.password().is_none());
        let auth = headers.get(AUTHORIZATION).expect("auth").to_str().expect("str");
        let expected = general_purpose::STANDARD.encode("user:");
        assert_eq!(auth, format!("Basic {expected}"));
    }

    #[test]
    fn no_userinfo_skips_authorization() {
        let (url, headers) = prepare_request_url("https://example.com/sub.yaml").expect("url");
        assert_eq!(url.as_str(), "https://example.com/sub.yaml");
        assert!(!headers.contains_key(AUTHORIZATION));
    }
}
