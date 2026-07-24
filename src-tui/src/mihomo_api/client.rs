use crate::mihomo_api::error::MihomoError;
use crate::mihomo_api::types::{ConnectionsData, MihomoVersion, ProxyData, ProxyDelay, SelectProxyRequest};
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Thin wrapper around a `reqwest::Client` configured to talk to mihomo
/// over a Unix domain socket with a `Authorization: Bearer {secret}`
/// header applied to every request.
///
/// One instance is built once and reused — building a new
/// `reqwest::Client` per call leaks file descriptors and re-resolves
/// DNS / socket paths (see RESEARCH.md, Pitfall 2).
pub struct MihomoApi {
    pub client: reqwest::Client,
    stream_client: reqwest::Client,
    socket_path: PathBuf,
}

impl MihomoApi {
    /// Build a new client targeting `socket_path` with bearer `secret`.
    ///
    /// Construction is lazy: the socket is not contacted until the
    /// first request. Returns `Err` only if the secret cannot be
    /// encoded as a header value (e.g. contains a NUL byte) or the
    /// underlying `reqwest::Client` fails to build.
    pub fn new(socket_path: PathBuf, secret: impl Into<String>) -> Result<Self, MihomoError> {
        let secret = secret.into();
        let mut headers = HeaderMap::new();
        let bearer = format!("Bearer {secret}");
        let header_value = HeaderValue::from_str(&bearer).map_err(|e| MihomoError::InvalidUri(e.to_string()))?;
        headers.insert(AUTHORIZATION, header_value);

        let client = reqwest::Client::builder()
            .unix_socket(socket_path.clone())
            .default_headers(headers.clone())
            .timeout(Duration::from_secs(5))
            .connect_timeout(Duration::from_secs(2))
            .pool_max_idle_per_host(4)
            .build()?;

        // Mihomo keeps /traffic and /logs open indefinitely. They need a
        // separate client because reqwest's global timeout includes body reads.
        let stream_client = reqwest::Client::builder()
            .unix_socket(socket_path.clone())
            .default_headers(headers)
            .connect_timeout(Duration::from_secs(2))
            .pool_max_idle_per_host(4)
            .build()?;

        Ok(Self {
            client,
            stream_client,
            socket_path,
        })
    }

    /// `GET /version` — health check.
    ///
    /// Returns the parsed version string on 200, or a typed error on
    /// any other outcome. Connection refused / timeout map to
    /// `MihomoError::CoreDown` so the caller can transition the core
    /// state machine.
    pub async fn version(&self) -> Result<MihomoVersion, MihomoError> {
        let resp = self.client.get("http://localhost/version").send().await;

        let resp = match resp {
            Ok(r) => r,
            Err(e) => {
                if e.is_connect() || e.is_timeout() || e.is_request() {
                    return Err(MihomoError::CoreDown {
                        path: self.socket_path.clone(),
                    });
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
            .get("http://localhost/proxies")
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
        let path = format!("http://localhost/proxies/{group}");
        let resp = self
            .client
            .put(&path)
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
        let resp = self
            .client
            .patch("http://localhost/configs")
            .json(&serde_json::json!({ "mode": mode }))
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

    /// Read clash mode from mihomo `/configs`, falling back to tolerant parse of `mode` only.
    pub async fn get_mode(&self) -> Result<String, MihomoError> {
        let resp = self
            .client
            .get("http://localhost/configs")
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

        // Tolerant: accept non-standard payloads as long as `mode` is present.
        let value: serde_json::Value =
            serde_json::from_str(&body).map_err(|e| MihomoError::Parse(e.to_string()))?;
        value
            .get("mode")
            .and_then(|m| m.as_str())
            .map(str::to_string)
            .ok_or_else(|| MihomoError::Parse("configs response missing mode".into()))
    }

    /// `GET /proxies/:name/delay?timeout=N&url=U` — test delay for a node.
    pub async fn delay_test(&self, name: &str, test_url: &str, timeout_ms: u64) -> Result<ProxyDelay, MihomoError> {
        let url = delay_test_url(name, test_url, timeout_ms)?;
        let resp = self.client.get(url).send().await.map_err(|e| self.map_http_err(e))?;

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

    fn map_http_err(&self, e: reqwest::Error) -> MihomoError {
        if e.is_connect() || e.is_timeout() {
            MihomoError::CoreDown {
                path: self.socket_path.clone(),
            }
        } else {
            MihomoError::Http(e)
        }
    }

    /// Open Mihomo's newline-delimited real-time traffic stream.
    pub async fn stream_traffic(&self) -> Result<reqwest::Response, MihomoError> {
        self.stream_endpoint("/traffic").await
    }

    pub async fn get_connections(&self) -> Result<ConnectionsData, MihomoError> {
        let resp = self
            .client
            .get("http://localhost/connections")
            .send()
            .await
            .map_err(|e| self.map_http_err(e))?;
        let body = resp.text().await?;
        serde_json::from_str(&body).map_err(|e| MihomoError::Parse(e.to_string()))
    }

    pub async fn close_connection(&self, id: &str) -> Result<(), MihomoError> {
        let path = format!("http://localhost/connections/{id}");
        let resp = self
            .client
            .delete(&path)
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
    pub async fn stream_logs(&self) -> Result<reqwest::Response, MihomoError> {
        self.stream_endpoint("/logs?level=info").await
    }

    async fn stream_endpoint(&self, endpoint: &str) -> Result<reqwest::Response, MihomoError> {
        let resp = self
            .stream_client
            .get(format!("http://localhost{endpoint}"))
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

fn delay_test_url(name: &str, test_url: &str, timeout_ms: u64) -> Result<reqwest::Url, MihomoError> {
    let mut url =
        reqwest::Url::parse("http://localhost/").map_err(|error| MihomoError::InvalidUri(error.to_string()))?;
    url.path_segments_mut()
        .map_err(|_| MihomoError::InvalidUri("localhost URL cannot hold path segments".into()))?
        .extend(["proxies", name, "delay"]);
    url.query_pairs_mut()
        .append_pair("timeout", &timeout_ms.to_string())
        .append_pair("url", test_url);
    Ok(url)
}

/// Helper trait for building a client from anything path-like.
impl MihomoApi {
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }
}

#[cfg(test)]
#[allow(clippy::panic, clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use tokio::net::UnixListener;
    use tokio::sync::Notify;

    #[test]
    fn test_new_construction() {
        let path = PathBuf::from("/tmp/nonexistent-for-test.sock");
        // We don't touch the file at construction; the client is lazy.
        let api = MihomoApi::new(path.clone(), "secret").expect("build");
        assert_eq!(api.socket_path(), path.as_path());
    }

    #[test]
    fn delay_url_encodes_proxy_name_and_test_url() {
        let name = "\u{1f1fa}\u{1f1f8}11\u{7f8e}\u{56fd}\u{897f}\u{96c6}\u{7fa4}-\u{5168}\u{7f51}\u{4f18}\u{5316}(M)";
        let test_url = "http://www.gstatic.com/generate_204?source=tui";
        let url = delay_test_url(name, test_url, 5000).expect("delay URL");

        let segments = url.path_segments().expect("path segments").collect::<Vec<_>>();
        assert_eq!(segments.first(), Some(&"proxies"));
        assert_eq!(segments.last(), Some(&"delay"));
        assert_ne!(segments.get(1), Some(&name));
        assert!(url.as_str().contains("%F0%9F%87%BA"));
        let query: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(query.get("timeout"), Some(&"5000".to_string()));
        assert_eq!(query.get("url"), Some(&test_url.to_string()));
    }

    #[tokio::test]
    async fn test_version_against_missing_socket() {
        // Pick a path that almost certainly does not exist.
        let path = PathBuf::from("/tmp/nonexistent-uds-for-mihomo-api-test.sock");
        if path.exists() {
            eprintln!("skipping: {} unexpectedly exists", path.display());
            return;
        }

        let api = MihomoApi::new(path.clone(), "secret").expect("build");
        let res = api.version().await;
        match res {
            Err(MihomoError::CoreDown { path: p }) => {
                assert_eq!(p, path);
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
        let tmp = std::env::temp_dir().join(format!("mihomo-api-test-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&tmp);

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
}
