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
    let user_agent = match option.and_then(|o| o.user_agent.as_ref()) {
        Some(custom) if !custom.trim().is_empty() => custom.to_string(),
        _ => super::client_meta::default_subscription_user_agent().await,
    };

    let mode = proxy_mode_from_option(option);
    let mut builder = subscription_client_builder(user_agent, Duration::from_secs(timeout), accept_invalid);

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

/// Build the subscription client with a protocol floor that rejects TLS 1.0
/// and 1.1 while retaining TLS 1.2 and 1.3. The rustls backend is selected in
/// `Cargo.toml`; this function makes the policy explicit instead of depending
/// on a backend default.
fn subscription_client_builder(
    user_agent: String,
    timeout: Duration,
    accept_invalid_certs: bool,
) -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .cookie_store(true)
        .gzip(true)
        .user_agent(user_agent)
        .timeout(timeout)
        .connect_timeout(Duration::from_secs(10).min(timeout))
        .redirect(reqwest::redirect::Policy::limited(10))
        .danger_accept_invalid_certs(accept_invalid_certs)
        .tls_version_min(reqwest::tls::Version::TLS_1_2)
}

/// Strip URL userinfo into an Authorization header.
///
/// Empty passwords still produce `Basic user:` (upstream NetworkManager behavior).
/// Password-only userinfo (`:secret@host`) produces `Basic :secret`.
fn prepare_request_url(url: &str) -> anyhow::Result<(Url, HeaderMap)> {
    let mut parsed = Url::parse(url).with_context(|| format!("invalid subscription URL: {url}"))?;
    let mut headers = HeaderMap::new();

    let has_userinfo = !parsed.username().is_empty() || parsed.password().is_some();
    if has_userinfo {
        let username = percent_encoding::percent_decode_str(parsed.username())
            .decode_utf8_lossy()
            .into_owned();
        let password = percent_encoding::percent_decode_str(parsed.password().unwrap_or_default())
            .decode_utf8_lossy()
            .into_owned();
        let encoded = general_purpose::STANDARD.encode(format!("{username}:{password}"));
        let value = HeaderValue::from_str(&format!("Basic {encoded}")).context("invalid Basic Auth header value")?;
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
        err.context("Subscription server uses legacy TLS; only TLS 1.2/1.3 is supported. TLS 1.0/1.1 is insecure")
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

/// Strip query parameters and userinfo from a URL for safe display.
pub fn redact_url(raw: &str) -> String {
    let parsed = match Url::parse(raw) {
        Ok(u) => u,
        Err(_) => return raw.to_string(),
    };
    let scheme = parsed.scheme();
    let host = parsed.host_str().unwrap_or("");
    let port = parsed.port().map_or_else(String::new, |p| format!(":{p}"));
    let path = parsed.path();
    // Keep the scheme://host:port/path, drop query and fragment.
    format!("{scheme}://{host}{port}{path}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use clash_verge_core::config::PrfOption;

    #[test]
    fn subscription_client_builds_with_explicit_tls_12_minimum() {
        // This pins the configured reqwest policy. It does not claim to
        // exercise a negotiated TLS handshake.
        subscription_client_builder("test-agent".into(), Duration::from_secs(5), false)
            .build()
            .expect("TLS 1.2 minimum is supported by the configured rustls backend");
    }

    #[test]
    fn legacy_tls_protocol_version_error_is_classified_for_guidance() {
        let error = std::io::Error::other("alert: protocol version");
        assert!(is_legacy_tls_protocol_error(&error));
        let unrelated = std::io::Error::other("connection reset");
        assert!(!is_legacy_tls_protocol_error(&unrelated));
    }

    #[tokio::test]
    async fn gzip_subscription_response_is_decoded() {
        use std::io::Write as _;
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind test listener");
        let address = listener.local_addr().expect("listener address");
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        encoder.write_all(b"proxies: []\n").expect("compress fixture");
        let compressed = encoder.finish().expect("finish gzip fixture");

        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept test request");
            let mut request = Vec::new();
            let mut chunk = [0u8; 512];
            loop {
                let read = stream.read(&mut chunk).await.expect("read request");
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&chunk[..read]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            let headers = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/yaml\r\nContent-Encoding: gzip\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                compressed.len()
            );
            stream.write_all(headers.as_bytes()).await.expect("write headers");
            stream.write_all(&compressed).await.expect("write compressed body");
            let _ = stream.shutdown().await;
        });

        let option = PrfOption {
            user_agent: Some("fixture-test".into()),
            ..Default::default()
        };
        let result = fetch_subscription(
            &format!("http://{address}/subscription.yaml"),
            Some(&option),
            &["127.0.0.1".to_owned()],
        )
        .await
        .expect("local fixture response should be fetched and decoded");
        assert_eq!(result.body, "proxies: []\n");
        server.await.expect("test server task");
    }

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
    fn password_only_userinfo_emits_basic_auth() {
        let (url, headers) = prepare_request_url("https://:secret@example.com/sub.yaml").expect("url");
        assert!(url.username().is_empty());
        assert!(url.password().is_none());
        let auth = headers.get(AUTHORIZATION).expect("auth").to_str().expect("str");
        let expected = general_purpose::STANDARD.encode(":secret");
        assert_eq!(auth, format!("Basic {expected}"));
    }

    #[test]
    fn no_userinfo_skips_authorization() {
        let (url, headers) = prepare_request_url("https://example.com/sub.yaml").expect("url");
        assert_eq!(url.as_str(), "https://example.com/sub.yaml");
        assert!(!headers.contains_key(AUTHORIZATION));
    }

    #[test]
    fn redact_url_strips_query_and_userinfo() {
        let redacted = redact_url("https://user:pass@example.com/sub?token=abc&flag=1");
        assert_eq!(redacted, "https://example.com/sub");
    }

    #[test]
    fn redact_url_keeps_port() {
        let redacted = redact_url("http://example.com:8080/path?a=1");
        assert_eq!(redacted, "http://example.com:8080/path");
    }

    #[test]
    fn redact_url_plain_unchanged() {
        assert_eq!(
            redact_url("https://example.com/sub.yaml"),
            "https://example.com/sub.yaml"
        );
    }
}
