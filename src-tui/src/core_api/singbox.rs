//! sing-box controller API.
//!
//! sing-box's experimental `clash_api` speaks the same wire protocol as
//! mihomo over TCP, so [`SingboxApi`] reuses the shared client and only
//! adds sing-box-specific semantics:
//! - capability bit declares provider endpoints unsupported (they are
//!   empty stubs upstream — verified against the testing branch)
//! - delay-test URLs are forced to https (sing-box silently drops http
//!   URLs and falls back to its own default)

use crate::core_api::proxy_core::{CoreCapabilities, ProxyCoreApi};
use crate::mihomo_api::error::MihomoError;
use crate::mihomo_api::types::{
    ConnectionsData, MihomoVersion, ProxyData, ProxyDelay, RuleProvidersResponse, RulesResponse,
};
use crate::mihomo_api::{MihomoApi, Transport};
use async_trait::async_trait;
use std::net::SocketAddr;

/// sing-box controller API backed by a TCP transport.
pub struct SingboxApi {
    inner: MihomoApi,
}

/// Force an http(s) delay-test URL onto https. Non-http schemes are
/// returned unchanged (callers reject them downstream).
pub fn force_https(url: &str) -> String {
    if let Some(rest) = url.strip_prefix("http://") {
        format!("https://{rest}")
    } else {
        url.to_string()
    }
}

impl SingboxApi {
    /// Build a client for a sing-box `clash_api` TCP endpoint.
    pub fn new(controller: SocketAddr, secret: impl Into<String>) -> Result<Self, MihomoError> {
        Ok(Self {
            inner: MihomoApi::with_transport(Transport::Tcp(controller), secret)?,
        })
    }

    /// Expose the underlying shared client (same wire protocol).
    pub fn inner(&self) -> &MihomoApi {
        &self.inner
    }
}

#[async_trait]
impl ProxyCoreApi for SingboxApi {
    fn capabilities(&self) -> CoreCapabilities {
        // Provider endpoints exist in sing-box's clash_api router but are
        // empty stubs — declared unsupported so UI hides them.
        CoreCapabilities { rule_providers: false }
    }

    async fn version(&self) -> Result<MihomoVersion, MihomoError> {
        self.inner.version().await
    }

    async fn get_proxies(&self) -> Result<ProxyData, MihomoError> {
        self.inner.get_proxies().await
    }

    async fn select_proxy(&self, group: &str, name: &str) -> Result<(), MihomoError> {
        self.inner.select_proxy(group, name).await
    }

    async fn patch_mode(&self, mode: &str) -> Result<(), MihomoError> {
        self.inner.patch_mode(mode).await
    }

    async fn get_mode(&self) -> Result<String, MihomoError> {
        self.inner.get_mode().await
    }

    async fn delay_test(&self, name: &str, test_url: &str, timeout_ms: u64) -> Result<ProxyDelay, MihomoError> {
        let test_url = force_https(test_url);
        self.inner.delay_test(name, &test_url, timeout_ms).await
    }

    async fn stream_traffic(&self) -> Result<reqwest::Response, MihomoError> {
        self.inner.stream_traffic().await
    }

    async fn get_connections(&self) -> Result<ConnectionsData, MihomoError> {
        self.inner.get_connections().await
    }

    async fn close_connection(&self, id: &str) -> Result<(), MihomoError> {
        self.inner.close_connection(id).await
    }

    async fn close_all_connections(&self) -> Result<(), MihomoError> {
        self.inner.close_all_connections().await
    }

    async fn stream_logs(&self, level: &str) -> Result<reqwest::Response, MihomoError> {
        self.inner.stream_logs(level).await
    }

    async fn get_rules(&self) -> Result<RulesResponse, MihomoError> {
        self.inner.get_rules().await
    }

    /// Capability-gated: sing-box's clash_api exposes no functional
    /// provider surface.
    async fn get_rule_providers(&self) -> Result<RuleProvidersResponse, MihomoError> {
        Err(MihomoError::NotFound(
            "/providers/rules is not supported by sing-box clash_api".into(),
        ))
    }

    /// Capability-gated: see [`Self::get_rule_providers`].
    async fn update_rule_provider(&self, _name: &str) -> Result<(), MihomoError> {
        Err(MihomoError::NotFound(
            "/providers/rules updates are not supported by sing-box clash_api".into(),
        ))
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn force_https_upgrades_and_preserves() {
        assert_eq!(
            force_https("http://www.gstatic.com/generate_204"),
            "https://www.gstatic.com/generate_204"
        );
        assert_eq!(
            force_https("https://www.gstatic.com/generate_204"),
            "https://www.gstatic.com/generate_204"
        );
    }

    #[test]
    fn capabilities_declare_providers_unsupported() {
        let addr: SocketAddr = "127.0.0.1:1".parse().expect("addr");
        let api = SingboxApi::new(addr, "secret").expect("build");
        let caps = api.capabilities();
        assert!(!caps.rule_providers);
        assert!(!api.supports_providers());
    }

    #[tokio::test]
    async fn provider_calls_fail_loudly_not_silently() {
        let addr: SocketAddr = "127.0.0.1:1".parse().expect("addr");
        let api = SingboxApi::new(addr, "secret").expect("build");
        let boxed: Box<dyn ProxyCoreApi> = Box::new(api);

        let err = boxed.get_rule_providers().await.expect_err("must fail");
        assert!(err.to_string().contains("not supported"), "{err}");

        let err = boxed.update_rule_provider("x").await.expect_err("must fail");
        assert!(err.to_string().contains("not supported"), "{err}");
    }

    #[tokio::test]
    async fn delay_test_forces_https_on_the_wire() {
        use std::sync::Arc;
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");

        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept");
            let mut buf = Vec::new();
            let mut chunk = [0u8; 256];
            loop {
                let n = stream.read(&mut chunk).await.expect("read");
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
                if buf.windows(4).any(|w| w == b"\r\n\r\n") || buf.len() > 4096 {
                    break;
                }
            }
            let request = String::from_utf8_lossy(&buf);
            // The on-the-wire probe URL must have been upgraded to https.
            assert!(
                request.contains("url=https%3A%2F%2F"),
                "wire request must carry an https probe url:\n{request}"
            );
            let body = r#"{"delay": 42}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(response.as_bytes()).await.expect("write");
            let _ = stream.shutdown().await;
        });

        let api = SingboxApi::new(addr, "secret").expect("build");
        let delay = api
            .delay_test("node-a", "http://www.gstatic.com/generate_204", 5000)
            .await
            .expect("delay");
        assert_eq!(delay.delay, 42);

        server.await.expect("server task");
        let _ = Arc::new(());
    }
}
