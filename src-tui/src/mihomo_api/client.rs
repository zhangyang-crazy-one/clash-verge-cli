use crate::mihomo_api::error::MihomoError;
use crate::mihomo_api::types::{
    ConnectionsData, MihomoVersion, ProxyData, ProxyDelay, ProxyProvidersResponse, RuleProvidersResponse,
    RulesResponse, SelectProxyRequest,
};
use reqwest::Url;
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

/// Transport endpoint of a proxy core's external controller.
///
/// mihomo listens on a Unix domain socket; sing-box's `clash_api` only
/// supports TCP listeners. Both speak the same HTTP protocol on top, so
/// the transport only changes how reqwest connects and which base URL
/// the request paths are appended to.
#[derive(Debug, Clone, PartialEq, Eq)]
// Tcp is constructed by SingboxApi (group 4 of add-singbox-dual-core);
// until then only tests construct it.
#[allow(dead_code)]
pub enum Transport {
    /// Unix domain socket (mihomo external-controller).
    UnixSocket(PathBuf),
    /// TCP listener (sing-box clash_api, or any TCP controller).
    Tcp(SocketAddr),
}

impl Transport {
    /// Base URL for request paths.
    ///
    /// For Unix sockets reqwest ignores the host portion of the URL, so
    /// `http://localhost` is the conventional placeholder. For TCP the
    /// socket address itself is the authority.
    fn base_url(&self) -> String {
        match self {
            Self::UnixSocket(_) => "http://localhost".to_string(),
            Self::Tcp(addr) => format!("http://{addr}"),
        }
    }

    /// Human-readable endpoint label for error messages.
    fn endpoint_label(&self) -> String {
        match self {
            Self::UnixSocket(path) => path.display().to_string(),
            Self::Tcp(addr) => addr.to_string(),
        }
    }
}

/// Thin wrapper around a `reqwest::Client` configured to talk to a proxy
/// core's external controller (mihomo over a Unix domain socket, sing-box
/// over TCP) with an `Authorization: Bearer {secret}` header applied to
/// every request.
///
/// One instance is built once and reused — building a new
/// `reqwest::Client` per call leaks file descriptors and re-resolves
/// DNS / socket paths (see RESEARCH.md, Pitfall 2).
pub struct MihomoApi {
    pub client: reqwest::Client,
    stream_client: reqwest::Client,
    transport: Transport,
    base_url: String,
    singbox: bool,
    provider_timeout: Duration,
}

impl MihomoApi {
    pub fn for_core(mut self, kind: crate::mihomo_manager::CoreKind) -> Self {
        self.singbox = kind == crate::mihomo_manager::CoreKind::SingBox;
        self
    }

    pub fn core_kind(&self) -> crate::mihomo_manager::CoreKind {
        if self.singbox {
            crate::mihomo_manager::CoreKind::SingBox
        } else {
            crate::mihomo_manager::CoreKind::Mihomo
        }
    }

    pub fn supports_providers(&self) -> bool {
        !self.singbox
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn transport(&self) -> &Transport {
        &self.transport
    }

    /// Set the provider refresh deadline. Defaults to 30 seconds; accepted
    /// values range from 1 ms through 120 seconds.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn with_provider_timeout(mut self, timeout: Duration) -> Result<Self, MihomoError> {
        validate_provider_timeout(timeout)?;
        self.provider_timeout = timeout;
        Ok(self)
    }

    /// Build a new client targeting the mihomo unix socket at `socket_path`
    /// with bearer `secret`.
    ///
    /// Convenience constructor preserving the historical unix-socket-only
    /// signature; equivalent to [`Self::with_transport`] with
    /// [`Transport::UnixSocket`].
    pub fn new(socket_path: PathBuf, secret: impl Into<String>) -> Result<Self, MihomoError> {
        Self::with_transport(Transport::UnixSocket(socket_path), secret)
    }

    /// Build a new client for an arbitrary controller transport with
    /// bearer `secret`.
    ///
    /// Construction is lazy: the endpoint is not contacted until the
    /// first request. Returns `Err` only if the secret cannot be
    /// encoded as a header value (e.g. contains a NUL byte) or the
    /// underlying `reqwest::Client` fails to build.
    pub fn with_transport(transport: Transport, secret: impl Into<String>) -> Result<Self, MihomoError> {
        let secret = secret.into();
        let mut headers = HeaderMap::new();
        let bearer = format!("Bearer {secret}");
        let header_value = HeaderValue::from_str(&bearer).map_err(|e| MihomoError::InvalidUri(e.to_string()))?;
        headers.insert(AUTHORIZATION, header_value);

        let mut short_builder = reqwest::Client::builder()
            .no_proxy()
            .default_headers(headers.clone())
            .timeout(Duration::from_secs(5))
            .connect_timeout(Duration::from_secs(2))
            .pool_max_idle_per_host(4);
        let mut stream_builder = reqwest::Client::builder()
            .no_proxy()
            .default_headers(headers)
            .connect_timeout(Duration::from_secs(2))
            .pool_max_idle_per_host(4);

        match &transport {
            Transport::UnixSocket(path) => {
                short_builder = short_builder.unix_socket(path.clone());
                stream_builder = stream_builder.unix_socket(path.clone());
            }
            Transport::Tcp(_) => {}
        }

        let client = short_builder.build()?;
        // Mihomo keeps /traffic and /logs open indefinitely. They need a
        // separate client because reqwest's global timeout includes body reads.
        let stream_client = stream_builder.build()?;

        let base_url = transport.base_url();
        Ok(Self {
            client,
            stream_client,
            transport,
            base_url,
            singbox: false,
            provider_timeout: DEFAULT_PROVIDER_TIMEOUT,
        })
    }

    /// `GET /version` — health check.
    ///
    /// Returns the parsed version string on 200, or a typed error on
    /// any other outcome. Connection refused / timeout map to
    /// `MihomoError::CoreDown` so the caller can transition the core
    /// state machine.
    pub async fn version(&self) -> Result<MihomoVersion, MihomoError> {
        let resp = self.client.get(format!("{}/version", self.base_url)).send().await;

        let resp = match resp {
            Ok(r) => r,
            Err(e) => {
                if e.is_connect() || e.is_timeout() || e.is_request() {
                    return Err(self.core_down());
                }
                return Err(MihomoError::Http(e));
            }
        };

        let status = resp.status();
        if status.is_success() {
            let body = resp.text().await?;
            return serde_json::from_str(&body).map_err(|e| MihomoError::Parse(e.to_string()));
        }

        let code = status.as_u16();
        let body = resp.text().await.unwrap_or_default();
        match code {
            401 => Err(MihomoError::Unauthorized),
            404 => Err(MihomoError::NotFound("/version".into())),
            _ => Err(MihomoError::HttpStatus { status: code, body }),
        }
    }

    /// `GET /proxies` — fetch all proxy groups and nodes.
    pub async fn get_proxies(&self) -> Result<ProxyData, MihomoError> {
        let resp = self
            .client
            .get(format!("{}/proxies", self.base_url))
            .send()
            .await
            .map_err(|e| self.map_http_err(e))?;

        let status = resp.status();
        if status.is_success() {
            let body = resp.text().await?;
            return serde_json::from_str(&body).map_err(|e| MihomoError::Parse(e.to_string()));
        }
        let body = resp.text().await.unwrap_or_default();
        Err(MihomoError::HttpStatus {
            status: status.as_u16(),
            body,
        })
    }

    /// `PUT /proxies/:group` — select a proxy node for a group.
    pub async fn select_proxy(&self, group: &str, name: &str) -> Result<(), MihomoError> {
        let req = SelectProxyRequest { name: name.to_string() };
        let path = self.path_url(&["proxies", group])?;
        let resp = self
            .client
            .put(path)
            .json(&req)
            .send()
            .await
            .map_err(|e| self.map_http_err(e))?;

        let status = resp.status();
        if status.is_success() {
            return Ok(());
        }
        let body = resp.text().await.unwrap_or_default();
        Err(MihomoError::HttpStatus {
            status: status.as_u16(),
            body,
        })
    }

    /// `PATCH /configs` — set clash mode (`rule` / `global` / `direct`).
    pub async fn patch_mode(&self, mode: &str) -> Result<(), MihomoError> {
        self.patch_configs(&serde_json::json!({ "mode": mode })).await
    }

    /// `PATCH /configs` — set the core's log level (`debug` / `info` /
    /// `warning` / `error` / `silent`) until the next restart.
    pub async fn patch_log_level(&self, level: &str) -> Result<(), MihomoError> {
        self.patch_configs(&serde_json::json!({ "log-level": level })).await
    }

    async fn patch_configs(&self, body: &serde_json::Value) -> Result<(), MihomoError> {
        let resp = self
            .client
            .patch(format!("{}/configs", self.base_url))
            .json(body)
            .send()
            .await
            .map_err(|e| self.map_http_err(e))?;

        if resp.status().is_success() {
            Ok(())
        } else {
            Err(MihomoError::HttpStatus {
                status: resp.status().as_u16(),
                body: resp.text().await.unwrap_or_default(),
            })
        }
    }

    /// `GET /configs` — the running core's configuration, as JSON.
    pub async fn get_configs(&self) -> Result<serde_json::Value, MihomoError> {
        let resp = self
            .client
            .get(format!("{}/configs", self.base_url))
            .send()
            .await
            .map_err(|e| self.map_http_err(e))?;

        let status = resp.status();
        let body = resp.text().await?;
        if !status.is_success() {
            return Err(MihomoError::HttpStatus {
                status: status.as_u16(),
                body,
            });
        }
        serde_json::from_str(&body).map_err(|e| MihomoError::Parse(e.to_string()))
    }

    /// Read clash mode from mihomo `/configs`, falling back to tolerant parse of `mode` only.
    pub async fn get_mode(&self) -> Result<String, MihomoError> {
        // Tolerant: accept non-standard payloads as long as `mode` is present.
        self.get_configs()
            .await?
            .get("mode")
            .and_then(|m| m.as_str())
            .map(str::to_string)
            .ok_or_else(|| MihomoError::Parse("configs response missing mode".into()))
    }

    /// The port the running core serves HTTP proxy requests on: `mixed-port`,
    /// else `port`.
    pub async fn http_proxy_port(&self) -> Result<u16, MihomoError> {
        let configs = self.get_configs().await?;
        ["mixed-port", "port"]
            .iter()
            .filter_map(|key| configs.get(key).and_then(serde_json::Value::as_u64))
            .find(|port| *port > 0)
            .and_then(|port| u16::try_from(port).ok())
            .ok_or_else(|| MihomoError::Parse("the core has no mixed-port or port for HTTP proxying".into()))
    }

    /// `GET /proxies/:name/delay?timeout=N&url=U` — test delay for a node.
    pub async fn delay_test(&self, name: &str, test_url: &str, timeout_ms: u64) -> Result<ProxyDelay, MihomoError> {
        let normalized = self.singbox.then(|| crate::core_api::singbox::force_https(test_url));
        let test_url = normalized.as_deref().unwrap_or(test_url);
        let deadline = operation_deadline(timeout_ms)?;
        let url = delay_test_url(&self.base_url, name, test_url, timeout_ms)?;
        let resp = self
            .stream_client
            .get(url)
            .timeout(deadline)
            .send()
            .await
            .map_err(|e| self.map_operation_http_err(e, "delay test"))?;

        let status = resp.status();
        if status.is_success() {
            let body = resp
                .text()
                .await
                .map_err(|error| self.map_operation_http_err(error, "delay test"))?;
            return serde_json::from_str(&body).map_err(|e| MihomoError::Parse(e.to_string()));
        }
        let body = resp.text().await.unwrap_or_default();
        Err(MihomoError::HttpStatus {
            status: status.as_u16(),
            body,
        })
    }

    /// Mihomo v1.19.32 provider-scoped node healthcheck. Names are resolved
    /// only within `provider`, so equal display names from other providers
    /// cannot redirect the request.
    pub async fn provider_proxy_delay_test(
        &self,
        provider: &str,
        name: &str,
        test_url: &str,
        timeout_ms: u64,
    ) -> Result<ProxyDelay, MihomoError> {
        self.require_provider_support("proxy provider healthcheck")?;
        let mut url = self.path_url(&["providers", "proxies", provider, name, "healthcheck"])?;
        url.query_pairs_mut()
            .append_pair("timeout", &timeout_ms.to_string())
            .append_pair("url", test_url);
        let deadline = operation_deadline(timeout_ms)?;
        let response = self
            .stream_client
            .get(url)
            .timeout(deadline)
            .send()
            .await
            .map_err(|error| self.map_operation_http_err(error, "provider healthcheck"))?;
        let status = response.status();
        let body = response
            .text()
            .await
            .map_err(|error| self.map_operation_http_err(error, "provider healthcheck"))?;
        if status.is_success() {
            serde_json::from_str(&body).map_err(|error| MihomoError::Parse(error.to_string()))
        } else {
            Err(MihomoError::HttpStatus {
                status: status.as_u16(),
                body,
            })
        }
    }

    /// Build a URL while encoding every dynamic path component as one segment.
    pub fn path_url(&self, segments: &[&str]) -> Result<Url, MihomoError> {
        let mut url = Url::parse(&self.base_url).map_err(|error| MihomoError::InvalidUri(error.to_string()))?;
        url.path_segments_mut()
            .map_err(|_| MihomoError::InvalidUri("base URL cannot hold path segments".into()))?
            .extend(segments.iter().copied());
        Ok(url)
    }

    fn map_http_err(&self, e: reqwest::Error) -> MihomoError {
        if e.is_connect() {
            self.core_down()
        } else {
            MihomoError::Http(e)
        }
    }

    fn map_operation_http_err(&self, error: reqwest::Error, operation: &str) -> MihomoError {
        if error.is_timeout() {
            MihomoError::OperationTimeout {
                core: self.core_name().into(),
                operation: operation.into(),
            }
        } else {
            self.map_http_err(error)
        }
    }

    fn core_down(&self) -> MihomoError {
        MihomoError::CoreDown {
            endpoint: self.transport.endpoint_label(),
        }
    }

    fn core_name(&self) -> &'static str {
        match self.core_kind() {
            crate::mihomo_manager::CoreKind::SingBox => "sing-box",
            crate::mihomo_manager::CoreKind::Mihomo => "mihomo",
        }
    }

    fn require_provider_support(&self, operation: &str) -> Result<(), MihomoError> {
        if self.singbox {
            return Err(MihomoError::UnsupportedCoreOperation {
                core: self.core_name().into(),
                operation: operation.into(),
            });
        }
        Ok(())
    }

    async fn confirm_ready(&self, client: &reqwest::Client, operation: &str) -> Result<(), MihomoError> {
        let response = client
            .get(format!("{}/version", self.base_url))
            .send()
            .await
            .map_err(|error| self.map_operation_http_err(error, operation))?;
        let status = response.status();
        if !status.is_success() {
            return Err(MihomoError::HttpStatus {
                status: status.as_u16(),
                body: response.text().await.unwrap_or_default(),
            });
        }
        let body = response
            .text()
            .await
            .map_err(|error| self.map_operation_http_err(error, operation))?;
        serde_json::from_str::<MihomoVersion>(&body)
            .map(|_| ())
            .map_err(|error| MihomoError::Parse(error.to_string()))
    }

    /// Open Mihomo's newline-delimited real-time traffic stream.
    pub async fn stream_traffic(&self) -> Result<reqwest::Response, MihomoError> {
        self.stream_endpoint("/traffic").await
    }

    pub async fn get_connections(&self) -> Result<ConnectionsData, MihomoError> {
        let resp = self
            .client
            .get(format!("{}/connections", self.base_url))
            .send()
            .await
            .map_err(|e| self.map_http_err(e))?;
        if !resp.status().is_success() {
            return Err(MihomoError::HttpStatus {
                status: resp.status().as_u16(),
                body: resp.text().await.unwrap_or_default(),
            });
        }
        let body = resp.text().await?;
        serde_json::from_str(&body).map_err(|e| MihomoError::Parse(e.to_string()))
    }

    pub async fn close_connection(&self, id: &str) -> Result<(), MihomoError> {
        let path = self.path_url(&["connections", id])?;
        let resp = self
            .client
            .delete(path)
            .send()
            .await
            .map_err(|e| self.map_http_err(e))?;
        if resp.status().is_success() {
            Ok(())
        } else {
            Err(MihomoError::HttpStatus {
                status: resp.status().as_u16(),
                body: resp.text().await.unwrap_or_default(),
            })
        }
    }

    /// `DELETE /connections` — close all active connections.
    pub async fn close_all_connections(&self) -> Result<(), MihomoError> {
        let resp = self
            .client
            .delete(format!("{}/connections", self.base_url))
            .send()
            .await
            .map_err(|e| self.map_http_err(e))?;
        if resp.status().is_success() {
            Ok(())
        } else {
            Err(MihomoError::HttpStatus {
                status: resp.status().as_u16(),
                body: resp.text().await.unwrap_or_default(),
            })
        }
    }

    /// Open Mihomo's newline-delimited real-time log stream.
    pub async fn stream_logs(&self, level: &str) -> Result<reqwest::Response, MihomoError> {
        self.stream_endpoint(&format!("/logs?level={level}")).await
    }

    /// `GET /rules` — fetch all rules.
    pub async fn get_rules(&self) -> Result<RulesResponse, MihomoError> {
        let resp = self
            .client
            .get(format!("{}/rules", self.base_url))
            .send()
            .await
            .map_err(|e| self.map_http_err(e))?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if status.is_success() {
            serde_json::from_str(&body).map_err(|e| MihomoError::Parse(e.to_string()))
        } else {
            Err(MihomoError::HttpStatus {
                status: status.as_u16(),
                body,
            })
        }
    }

    /// `GET /providers/rules` — fetch all rule providers.
    pub async fn get_proxy_providers(&self) -> Result<ProxyProvidersResponse, MihomoError> {
        self.require_provider_support("list proxy providers")?;
        let path = self.path_url(&["providers", "proxies"])?;
        let resp = self
            .client
            .get(path)
            .send()
            .await
            .map_err(|error| self.map_http_err(error))?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if status.is_success() {
            serde_json::from_str(&body).map_err(|error| MihomoError::Parse(error.to_string()))
        } else {
            Err(MihomoError::HttpStatus {
                status: status.as_u16(),
                body,
            })
        }
    }

    /// `GET /providers/rules` — fetch all rule providers.
    pub async fn get_rule_providers(&self) -> Result<RuleProvidersResponse, MihomoError> {
        self.require_provider_support("list rule providers")?;
        let resp = self
            .client
            .get(format!("{}/providers/rules", self.base_url))
            .send()
            .await
            .map_err(|e| self.map_http_err(e))?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if status.is_success() {
            serde_json::from_str(&body).map_err(|e| MihomoError::Parse(e.to_string()))
        } else {
            Err(MihomoError::HttpStatus {
                status: status.as_u16(),
                body,
            })
        }
    }

    /// `PUT /providers/rules/:name` — update a rule provider.
    pub async fn update_rule_provider(&self, name: &str) -> Result<(), MihomoError> {
        self.update_rule_provider_with_timeout(name, self.provider_timeout)
            .await
    }

    /// Refresh a provider with an explicit bounded deadline, then confirm
    /// the controller still answers `/version` before reporting success.
    pub async fn update_rule_provider_with_timeout(&self, name: &str, timeout: Duration) -> Result<(), MihomoError> {
        self.require_provider_support("refresh rule provider")?;
        validate_provider_timeout(timeout)?;
        let path = self.path_url(&["providers", "rules", name])?;
        let resp = self
            .stream_client
            .put(path)
            .timeout(timeout)
            .send()
            .await
            .map_err(|error| self.map_operation_http_err(error, "rule provider refresh"))?;
        if resp.status().is_success() {
            self.confirm_ready(&self.client, "rule provider refresh").await
        } else {
            Err(MihomoError::HttpStatus {
                status: resp.status().as_u16(),
                body: resp.text().await.unwrap_or_default(),
            })
        }
    }

    async fn stream_endpoint(&self, endpoint: &str) -> Result<reqwest::Response, MihomoError> {
        let resp = self
            .stream_client
            .get(format!("{}{endpoint}", self.base_url))
            .send()
            .await
            .map_err(|e| self.map_http_err(e))?;

        if resp.status().is_success() {
            Ok(resp)
        } else {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            Err(MihomoError::HttpStatus { status, body })
        }
    }
}

fn delay_test_url(base: &str, name: &str, test_url: &str, timeout_ms: u64) -> Result<reqwest::Url, MihomoError> {
    let mut url = reqwest::Url::parse(base).map_err(|error| MihomoError::InvalidUri(error.to_string()))?;
    url.path_segments_mut()
        .map_err(|_| MihomoError::InvalidUri("base URL cannot hold path segments".into()))?
        .extend(["proxies", name, "delay"]);
    url.query_pairs_mut()
        .append_pair("timeout", &timeout_ms.to_string())
        .append_pair("url", test_url);
    Ok(url)
}

const DEFAULT_PROVIDER_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_PROVIDER_TIMEOUT: Duration = Duration::from_secs(120);

fn validate_provider_timeout(timeout: Duration) -> Result<(), MihomoError> {
    if timeout.is_zero() || timeout > MAX_PROVIDER_TIMEOUT {
        return Err(MihomoError::InvalidUri(format!(
            "provider timeout must be in 1 ms..={} s, got {timeout:?}",
            MAX_PROVIDER_TIMEOUT.as_secs()
        )));
    }
    Ok(())
}

fn operation_deadline(timeout_ms: u64) -> Result<Duration, MihomoError> {
    if !(1..=32_767).contains(&timeout_ms) {
        return Err(MihomoError::InvalidUri(format!(
            "delay timeout must be in 1..=32767 ms, got {timeout_ms}"
        )));
    }
    Ok(Duration::from_millis(timeout_ms + 5_000))
}

#[cfg(test)]
#[allow(clippy::panic, clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use tokio::net::TcpListener;
    use tokio::net::UnixListener;
    use tokio::sync::Notify;

    #[test]
    fn test_new_construction() {
        let path = PathBuf::from("/tmp/nonexistent-for-test.sock");
        // We don't touch the file at construction; the client is lazy.
        let api = MihomoApi::new(path.clone(), "secret").expect("build");
        assert_eq!(api.transport(), &Transport::UnixSocket(path));
    }

    #[test]
    fn transport_base_url_matches_transport_kind() {
        let unix = Transport::UnixSocket(PathBuf::from("/tmp/x.sock"));
        assert_eq!(unix.base_url(), "http://localhost");

        let tcp = Transport::Tcp("127.0.0.1:9090".parse::<SocketAddr>().expect("addr"));
        assert_eq!(tcp.base_url(), "http://127.0.0.1:9090");
        assert_eq!(tcp.endpoint_label(), "127.0.0.1:9090");
    }

    #[test]
    fn tcp_transport_construction_is_lazy() {
        // An address with no listener must still construct fine; the
        // endpoint is only contacted on the first request.
        let addr: SocketAddr = "127.0.0.1:1".parse().expect("addr");
        let api = MihomoApi::with_transport(Transport::Tcp(addr), "secret").expect("build");
        assert_eq!(api.transport(), &Transport::Tcp(addr));
    }

    #[test]
    fn delay_url_encodes_proxy_name_and_test_url() {
        let name = "\u{1f1fa}\u{1f1f8}11\u{7f8e}\u{56fd}\u{897f}\u{96c6}\u{7fa4}-\u{5168}\u{7f51}\u{4f18}\u{5316}(M)";
        let test_url = "http://www.gstatic.com/generate_204?source=tui";
        let url = delay_test_url("http://localhost", name, test_url, 5000).expect("delay URL");

        let segments = url.path_segments().expect("path segments").collect::<Vec<_>>();
        assert_eq!(segments.first(), Some(&"proxies"));
        assert_eq!(segments.last(), Some(&"delay"));
        assert_ne!(segments.get(1), Some(&name));
        assert!(url.as_str().contains("%F0%9F%87%BA"));
        let query: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(query.get("timeout"), Some(&"5000".to_string()));
        assert_eq!(query.get("url"), Some(&test_url.to_string()));
    }

    #[test]
    fn provider_path_encodes_dynamic_segments_and_deadline_is_bounded() {
        let api = MihomoApi::with_transport(Transport::Tcp("127.0.0.1:9090".parse().unwrap()), "s").unwrap();
        let url = api
            .path_url(&["providers", "proxies", "a/b", "node #1", "healthcheck"])
            .unwrap();
        assert!(url.path().contains("a%2Fb"));
        assert!(url.path().contains("node%20%231"));
        assert_eq!(operation_deadline(32_767).unwrap(), Duration::from_millis(37_767));
        assert!(operation_deadline(0).is_err());
        assert!(operation_deadline(32_768).is_err());
    }

    #[test]
    fn provider_healthcheck_encodes_each_path_segment() {
        let api = MihomoApi::with_transport(Transport::Tcp("127.0.0.1:9090".parse().unwrap()), "s").unwrap();
        let url = api
            .path_url(&["providers", "proxies", "provider/a", "node #1", "healthcheck"])
            .unwrap();
        assert!(url.path().contains("provider%2Fa"), "{}", url.path());
        assert!(url.path().contains("node%20%231"), "{}", url.path());
        assert_eq!(
            api.path_url(&["proxies", "group/a#b"]).unwrap().path(),
            "/proxies/group%2Fa%23b"
        );
    }

    #[test]
    fn delay_deadline_rejects_values_outside_controller_integer_range() {
        assert_eq!(operation_deadline(32_767).unwrap(), Duration::from_millis(37_767));
        assert!(operation_deadline(0).is_err());
        assert!(operation_deadline(32_768).is_err());
        assert_eq!(operation_deadline(6_000).unwrap(), Duration::from_secs(11));
    }

    #[test]
    fn provider_deadline_defaults_to_30_seconds_and_rejects_out_of_range_values() {
        let api = MihomoApi::with_transport(Transport::Tcp("127.0.0.1:9090".parse().unwrap()), "s").unwrap();
        assert_eq!(api.provider_timeout, Duration::from_secs(30));
        assert!(validate_provider_timeout(Duration::ZERO).is_err());
        assert!(validate_provider_timeout(Duration::from_secs(121)).is_err());
        let api = MihomoApi::with_transport(Transport::Tcp("127.0.0.1:9090".parse().unwrap()), "s").unwrap();
        assert_eq!(
            api.with_provider_timeout(Duration::from_secs(45))
                .unwrap()
                .provider_timeout,
            Duration::from_secs(45)
        );
    }

    #[tokio::test]
    async fn singbox_provider_operations_are_rejected_before_connecting() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let api = MihomoApi::with_transport(Transport::Tcp(addr), "s")
            .unwrap()
            .for_core(crate::mihomo_manager::CoreKind::SingBox);
        assert_eq!(api.core_kind(), crate::mihomo_manager::CoreKind::SingBox);
        assert!(!api.supports_providers());
        for result in [
            api.get_rule_providers().await.map(|_| ()),
            api.get_proxy_providers().await.map(|_| ()),
            api.update_rule_provider("p").await,
            api.provider_proxy_delay_test("p", "n", "https://example.test", 500)
                .await
                .map(|_| ()),
        ] {
            assert!(matches!(result, Err(MihomoError::UnsupportedCoreOperation { .. })));
        }
        let listener = listener.into_std().unwrap();
        listener.set_nonblocking(true).unwrap();
        assert!(matches!(listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock));
    }

    #[tokio::test]
    async fn singbox_delay_client_normalizes_http_url_on_the_wire() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = read_request(&mut stream).await;
            assert!(request.contains("url=https%3A%2F%2Fexample.test%2Fprobe"), "{request}");
            write_response(&mut stream, "200 OK", r#"{"delay":8}"#).await;
        });
        let api = MihomoApi::with_transport(Transport::Tcp(addr), "s")
            .unwrap()
            .for_core(crate::mihomo_manager::CoreKind::SingBox);
        assert_eq!(
            api.delay_test("node", "http://example.test/probe", 500)
                .await
                .unwrap()
                .delay,
            8
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn provider_refresh_encodes_path_waits_for_version_and_reports_success_after_readiness() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let mut requests = Vec::new();
            for (status, body) in [("204 No Content", ""), ("200 OK", r#"{"version":"1.19.32"}"#)] {
                let (mut stream, _) = listener.accept().await.unwrap();
                requests.push(read_request(&mut stream).await);
                write_response(&mut stream, status, body).await;
            }
            requests
        });
        let api = MihomoApi::with_transport(Transport::Tcp(addr), "s").unwrap();
        api.update_rule_provider_with_timeout("provider/name #1", Duration::from_secs(1))
            .await
            .unwrap();
        let requests = server.await.unwrap();
        assert!(
            requests[0].starts_with("PUT /providers/rules/provider%2Fname%20%231 "),
            "{}",
            requests[0]
        );
        assert!(requests[1].starts_with("GET /version "), "{}", requests[1]);
    }

    #[tokio::test]
    async fn provider_request_timeout_is_not_classified_as_core_down() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let _ = read_request(&mut stream).await;
            std::future::pending::<()>().await;
        });
        let api = MihomoApi::with_transport(Transport::Tcp(addr), "s").unwrap();
        let error = api
            .update_rule_provider_with_timeout("provider", Duration::from_millis(50))
            .await
            .unwrap_err();
        assert!(
            matches!(error, MihomoError::OperationTimeout { ref core, ref operation }
            if core == "mihomo" && operation == "rule provider refresh"),
            "{error:?}"
        );
        server.abort();
    }

    async fn read_request(stream: &mut tokio::net::TcpStream) -> String {
        let mut buf = Vec::new();
        let mut chunk = [0_u8; 512];
        loop {
            let n = stream.read(&mut chunk).await.unwrap();
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
            if buf.windows(4).any(|window| window == b"\r\n\r\n") || buf.len() > 8192 {
                break;
            }
        }
        String::from_utf8_lossy(&buf).into_owned()
    }

    async fn write_response(stream: &mut tokio::net::TcpStream, status: &str, body: &str) {
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(response.as_bytes()).await.unwrap();
        stream.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn test_version_against_missing_socket() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing.sock");

        let api = MihomoApi::new(path.clone(), "secret").expect("build");
        let res = api.version().await;
        match res {
            Err(MihomoError::CoreDown { endpoint }) => {
                assert_eq!(endpoint, path.display().to_string());
            }
            Err(other) => panic!("expected CoreDown, got {other:?}"),
            Ok(v) => panic!("expected CoreDown, got Ok({v:?})"),
        }
    }

    #[tokio::test]
    async fn test_version_unavailable_endpoint_is_core_down() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing.sock");
        let api = MihomoApi::new(path.clone(), "secret").expect("build");
        let res = api.version().await;
        match res {
            Err(MihomoError::CoreDown { endpoint }) => {
                assert_eq!(endpoint, path.display().to_string());
            }
            Err(other) => panic!("expected CoreDown, got {other:?}"),
            Ok(v) => panic!("expected CoreDown, got Ok({v:?})"),
        }
    }

    #[tokio::test]
    async fn test_bearer_header_in_request() {
        // Bind a temp Unix socket, accept one connection, read the
        // request bytes, assert the Authorization header is present,
        // and reply with a minimal /version body.
        let dir = tempfile::tempdir().unwrap();
        let tmp = dir.path().join("controller.sock");

        let listener = UnixListener::bind(&tmp).expect("bind uds");
        let ready = Arc::new(Notify::new());
        let ready_clone = Arc::clone(&ready);
        let tmp_for_server = tmp.clone();

        let server = tokio::spawn(async move {
            ready_clone.notify_one();
            let (mut stream, _addr) = listener.accept().await.expect("accept");

            // Read until we see the end of headers (CRLF CRLF) or 4 KiB.
            let mut buf = Vec::with_capacity(1024);
            let mut tmp_buf = [0u8; 256];
            loop {
                let n = stream.read(&mut tmp_buf).await.expect("read");
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&tmp_buf[..n]);
                if buf.windows(4).any(|w| w == b"\r\n\r\n") || buf.len() > 4096 {
                    break;
                }
            }

            let request = String::from_utf8_lossy(&buf).to_lowercase();
            assert!(
                request.contains("authorization: bearer secret123"),
                "request did not contain expected bearer header:\n{request}"
            );
            assert!(
                request.starts_with("get /version"),
                "unexpected request line: {request}"
            );

            // Reply with a minimal mihomo /version body.
            let body = r#"{"version":"Mihomo Meta v1.19.29"}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(response.as_bytes()).await.expect("write response");
            let _ = stream.shutdown().await;
        });

        ready.notified().await;

        let api = MihomoApi::new(tmp_for_server, "secret123").expect("build");
        let v = api.version().await.expect("version should succeed");
        assert_eq!(v.version, "Mihomo Meta v1.19.29");

        server.await.expect("server task");
        let _ = std::fs::remove_file(&tmp);
    }

    #[tokio::test]
    async fn test_bearer_header_over_tcp_transport() {
        // Same contract as the unix-socket test, but over TCP: this is the
        // transport sing-box's clash_api speaks.
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind tcp");
        let addr = listener.local_addr().expect("local addr");

        let server = tokio::spawn(async move {
            let (mut stream, _addr) = listener.accept().await.expect("accept");

            let mut buf = Vec::with_capacity(1024);
            let mut tmp_buf = [0u8; 256];
            loop {
                let n = stream.read(&mut tmp_buf).await.expect("read");
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&tmp_buf[..n]);
                if buf.windows(4).any(|w| w == b"\r\n\r\n") || buf.len() > 4096 {
                    break;
                }
            }

            let request = String::from_utf8_lossy(&buf).to_lowercase();
            assert!(
                request.contains("authorization: bearer tcp-secret"),
                "request did not contain expected bearer header:\n{request}"
            );
            assert!(
                request.starts_with("get /version"),
                "unexpected request line: {request}"
            );

            // Reply with a minimal sing-box /version body.
            let body = r#"{"version":"sing-box 1.13.12","meta":true}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(response.as_bytes()).await.expect("write response");
            let _ = stream.shutdown().await;
        });

        let api = MihomoApi::with_transport(Transport::Tcp(addr), "tcp-secret").expect("build");
        let v = api.version().await.expect("version should succeed");
        assert_eq!(v.version, "sing-box 1.13.12");

        server.await.expect("server task");
    }
}
