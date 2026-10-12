//! Local network regressions; never access user config or a real core.

use super::*;
use parking_lot::Mutex;
use std::net::SocketAddr;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

struct Server {
    address: SocketAddr,
    requests: Arc<Mutex<Vec<String>>>,
    hits: Arc<AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
}

impl Server {
    async fn start(ip: &str, handler: impl Fn(&str) -> Vec<u8> + Send + Sync + 'static) -> Self {
        let listener = TcpListener::bind((ip, 0)).await.expect("bind local fixture");
        let address = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let hits = Arc::new(AtomicUsize::new(0));
        let captured = requests.clone();
        let count = hits.clone();
        let handler = Arc::new(handler);
        let task = tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let captured = captured.clone();
                let count = count.clone();
                let handler = handler.clone();
                tokio::spawn(async move {
                    count.fetch_add(1, Ordering::SeqCst);
                    let mut request = Vec::new();
                    let mut chunk = [0u8; 1024];
                    while let Ok(read) = stream.read(&mut chunk).await {
                        if read == 0 {
                            return;
                        }
                        request.extend_from_slice(&chunk[..read]);
                        if request.windows(4).any(|w| w == b"\r\n\r\n") {
                            break;
                        }
                    }
                    let request = String::from_utf8(request).unwrap();
                    let response = handler(&request);
                    captured.lock().push(request);
                    let _ = stream.write_all(&response).await;
                    let _ = stream.shutdown().await;
                });
            }
        });
        Self {
            address,
            requests,
            hits,
            task,
        }
    }

    fn url(&self) -> String {
        format!("http://{}/sub", self.address)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn response(body: &[u8]) -> Vec<u8> {
    let mut bytes = format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    bytes.extend_from_slice(body);
    bytes
}

fn option() -> PrfOption {
    PrfOption {
        user_agent: Some("local-security-test".into()),
        timeout_seconds: Some(3),
        ..Default::default()
    }
}

#[tokio::test]
async fn untrusted_loopback_redirect_never_contacts_target() {
    let target = Server::start("127.0.0.2", |_| response(b"proxies: []\n")).await;
    let location = target.url();
    let source = Server::start("127.0.0.1", move |_| {
        format!("HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .into_bytes()
    })
    .await;
    let result = fetch_subscription(&source.url(), Some(&option()), &["127.0.0.1".into()]).await;
    assert!(result.is_err(), "untrusted redirected host must be blocked");
    assert_eq!(source.hits.load(Ordering::SeqCst), 1);
    assert_eq!(target.hits.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn trusted_redirect_succeeds_and_strips_cross_host_auth() {
    let target = Server::start("127.0.0.2", |_| response(b"proxies: []\n")).await;
    let location = target.url();
    let source = Server::start("127.0.0.1", move |_| {
        format!("HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .into_bytes()
    })
    .await;
    let url = source.url().replace("http://", "http://alice:secret@");
    let result = fetch_subscription(&url, Some(&option()), &["127.0.0.1".into(), "127.0.0.2".into()])
        .await
        .unwrap();
    assert_eq!(result.body, "proxies: []\n");
    assert_eq!(result.final_url, target.url());
    assert!(
        source.requests.lock()[0]
            .to_lowercase()
            .contains("authorization: basic ")
    );
    assert!(!target.requests.lock()[0].to_lowercase().contains("authorization:"));
    assert_eq!(target.hits.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn cross_port_auth_is_stripped_and_redirect_userinfo_is_ignored() {
    let target = Server::start("127.0.0.1", |_| response(b"proxies: []\n")).await;
    let location = target.url().replace("http://", "http://injected:password@");
    let source = Server::start("127.0.0.1", move |_| {
        format!(
            "HTTP/1.1 307 Temporary Redirect\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        )
        .into_bytes()
    })
    .await;
    let url = source.url().replace("http://", "http://alice:secret@");
    fetch_subscription(&url, Some(&option()), &["127.0.0.1".into()])
        .await
        .unwrap();
    assert!(!target.requests.lock()[0].to_lowercase().contains("authorization:"));
}

#[tokio::test]
async fn relative_redirects_keep_same_origin_auth_and_get_semantics() {
    for status in [301, 302, 303, 307, 308] {
        let server = Server::start("127.0.0.1", move |request| {
            if request.starts_with("GET /sub ") {
                format!(
                    "HTTP/1.1 {status} Redirect\r\nLocation: /final\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                )
                .into_bytes()
            } else {
                response(b"proxies: []\n")
            }
        })
        .await;
        let url = server.url().replace("http://", "http://alice:secret@");
        let result = fetch_subscription(&url, Some(&option()), &["127.0.0.1".into()])
            .await
            .unwrap();
        assert_eq!(result.body, "proxies: []\n");
        let requests = server.requests.lock();
        assert_eq!(requests.len(), 2);
        assert!(requests[1].starts_with("GET /final "));
        assert!(
            requests
                .iter()
                .all(|r| r.to_lowercase().contains("authorization: basic "))
        );
    }
}

#[tokio::test]
async fn redirect_limit_allows_ten_hops_and_rejects_eleven() {
    for hops in [10, 11] {
        let server = Server::start("127.0.0.1", move |request| {
            let path = request.split_whitespace().nth(1).unwrap();
            let current = path.trim_start_matches('/').parse::<usize>().unwrap_or(0);
            if current < hops {
                format!(
                    "HTTP/1.1 302 Found\r\nLocation: /{}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    current + 1
                )
                .into_bytes()
            } else {
                response(b"proxies: []\n")
            }
        })
        .await;
        let result = fetch_subscription(&server.url(), Some(&option()), &["127.0.0.1".into()]).await;
        assert_eq!(result.is_ok(), hops == 10);
        assert_eq!(server.hits.load(Ordering::SeqCst), 11);
    }
}

#[tokio::test]
async fn redirect_invalid_scheme_is_rejected_without_fetching() {
    let server = Server::start("127.0.0.1", |_| {
        b"HTTP/1.1 302 Found\r\nLocation: file:///etc/passwd\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec()
    })
    .await;
    let err = fetch_subscription(&server.url(), Some(&option()), &["127.0.0.1".into()])
        .await
        .err()
        .unwrap();
    assert!(err.to_string().contains("http or https"));
    assert_eq!(server.hits.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn exact_body_limit_is_accepted_and_chunked_overflow_rejected() {
    let exact = Arc::new(vec![b'a'; MAX_BODY_BYTES]);
    let server = Server::start("127.0.0.1", move |_| response(&exact)).await;
    let result = fetch_subscription(&server.url(), Some(&option()), &["127.0.0.1".into()])
        .await
        .unwrap();
    assert_eq!(result.body.len(), MAX_BODY_BYTES);

    let body = Arc::new(vec![b'b'; MAX_BODY_BYTES + 1]);
    let oversized = Server::start("127.0.0.1", move |_| {
        let mut response = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n".to_vec();
        // No Content-Length: the streaming count must enforce the cap.
        for chunk in body.chunks(8192) {
            response.extend_from_slice(format!("{:x}\r\n", chunk.len()).as_bytes());
            response.extend_from_slice(chunk);
            response.extend_from_slice(b"\r\n");
        }
        response.extend_from_slice(b"0\r\n\r\n");
        response
    })
    .await;
    let err = fetch_subscription(&oversized.url(), Some(&option()), &["127.0.0.1".into()])
        .await
        .err()
        .unwrap();
    assert!(err.to_string().contains("20 MiB"));
}

#[tokio::test]
async fn gzip_expansion_is_bounded_by_decoded_not_wire_size() {
    use std::io::Write;
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    encoder.write_all(&vec![b'x'; MAX_BODY_BYTES + 1]).unwrap();
    let body = Arc::new(encoder.finish().unwrap());
    assert!(body.len() < MAX_BODY_BYTES / 100);
    let server = Server::start("127.0.0.1", move |_| {
        let mut bytes = format!(
            "HTTP/1.1 200 OK\r\nContent-Encoding: gzip\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        bytes.extend_from_slice(&body);
        bytes
    })
    .await;
    let err = fetch_subscription(&server.url(), Some(&option()), &["127.0.0.1".into()])
        .await
        .err()
        .unwrap();
    assert!(err.to_string().contains("20 MiB"));
}

struct ChangingResolver {
    calls: Arc<AtomicUsize>,
}

impl reqwest::dns::Resolve for ChangingResolver {
    fn resolve(&self, _: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let ip = if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            "8.8.8.8:0"
        } else {
            "127.0.0.2:0"
        };
        let addr: SocketAddr = ip.parse().unwrap();
        Box::pin(async move { Ok(Box::new(std::iter::once(addr)) as reqwest::dns::Addrs) })
    }
}

#[tokio::test]
async fn rebinding_connection_answer_is_rejected_before_tcp_connect() {
    use reqwest::dns::Resolve;
    let target = Server::start("127.0.0.2", |_| response(b"internal")).await;
    let calls = Arc::new(AtomicUsize::new(0));
    let resolver = ssrf::SafeResolver::with_inner(Arc::new(ChangingResolver { calls: calls.clone() }), &[]);
    // The public preflight answer passes, but cannot authorize a later answer.
    let preflight: Vec<_> = resolver
        .resolve("rebinding.test".parse().unwrap())
        .await
        .unwrap()
        .collect();
    assert_eq!(preflight[0].ip().to_string(), "8.8.8.8");
    let client = subscription_client_builder("test".into(), Duration::from_secs(3), false)
        .no_proxy()
        .dns_resolver(resolver)
        .build()
        .unwrap();
    let url = Url::parse(&format!("http://rebinding.test:{}/sub", target.address.port())).unwrap();
    let err = fetch_with_client(&client, url, HeaderMap::new(), &[], ProxyMode::None)
        .await
        .err()
        .unwrap();
    assert!(format!("{err:#}").contains("SSRF blocked"), "{err:#}");
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(target.hits.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn fetch_errors_redact_query_userinfo_and_reqwest_error_url() {
    let server = Server::start("127.0.0.1", |_| {
        b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec()
    })
    .await;
    let url = format!(
        "{}?token=secret-query",
        server.url().replace("http://", "http://alice:secret-password@")
    );
    let err = fetch_subscription(&url, Some(&option()), &["127.0.0.1".into()])
        .await
        .err()
        .unwrap();
    let detail = format!("{err:#?}");
    for secret in ["alice", "secret-password", "secret-query"] {
        assert!(!detail.contains(secret), "{detail}");
    }
    let closed = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = closed.local_addr().unwrap();
    drop(closed);
    let url = format!("http://alice:secret-password@{address}/sub?token=secret-query");
    let err = fetch_subscription(&url, Some(&option()), &["127.0.0.1".into()])
        .await
        .err()
        .unwrap();
    let detail = format!("{err:#?}");
    for secret in ["alice", "secret-password", "secret-query"] {
        assert!(!detail.contains(secret), "{detail}");
    }
}

// Environment proxy state is process-global in reqwest. Use a separate test
// process with isolated XDG directories; no unsafe set_var or live configs.
async fn proxy_child(case: &str, proxy: SocketAddr, url: &str, no_proxy: &str) {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("config.yaml"),
        format!("mixed-port: {}\n", proxy.port()),
    )
    .unwrap();
    let mut command = tokio::process::Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--exact",
            "subscribe::fetch::security_tests::environment_proxy_child",
            "--nocapture",
        ])
        .env("ISSUE78_PROXY_CASE", case)
        .env("ISSUE78_PROXY_URL", url)
        .env("ISSUE78_TEST_ROOT", root.path())
        .env("XDG_CONFIG_HOME", root.path().join("config"))
        .env("XDG_DATA_HOME", root.path().join("data"))
        .env("XDG_RUNTIME_DIR", root.path().join("runtime"));
    for key in [
        "HTTP_PROXY",
        "http_proxy",
        "HTTPS_PROXY",
        "https_proxy",
        "ALL_PROXY",
        "all_proxy",
        "NO_PROXY",
        "no_proxy",
    ] {
        command.env_remove(key);
    }
    command
        .env("HTTP_PROXY", format!("http://{proxy}"))
        .env("HTTPS_PROXY", format!("http://{proxy}"))
        .env("NO_PROXY", no_proxy)
        .kill_on_drop(true);
    let output = tokio::time::timeout(Duration::from_secs(10), command.output())
        .await
        .unwrap()
        .unwrap();
    assert!(
        output.status.success(),
        "proxy case {case}: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[tokio::test]
async fn environment_proxy_child() {
    let Ok(case) = std::env::var("ISSUE78_PROXY_CASE") else {
        return;
    };
    clash_verge_core::utils::dirs::set_app_home_dir(std::env::var_os("ISSUE78_TEST_ROOT").unwrap().into());
    let url = std::env::var("ISSUE78_PROXY_URL").unwrap();
    let allowlist = if case == "untrusted" {
        vec![]
    } else {
        vec![Url::parse(&url).unwrap().host_str().unwrap().into()]
    };
    let mut option = option();
    if case == "localhost" {
        option.self_proxy = Some(true);
    } else {
        option.with_proxy = Some(true);
    }
    let result = fetch_subscription(&url, Some(&option), &allowlist).await;
    if matches!(case.as_str(), "untrusted" | "redirect") {
        let err = result.err().expect("untrusted proxy domain must be rejected");
        assert!(err.to_string().contains("option.trusted_hosts"), "{err:#}");
    } else if case == "failed" {
        assert!(result.is_err(), "failed proxy cannot trigger hidden direct connection");
    } else {
        assert_eq!(result.unwrap().body, "proxies: []\n");
    }
}

#[tokio::test]
async fn http_environment_and_localhost_proxy_enforce_per_host_trust() {
    let proxy = Server::start("127.0.0.1", |request| {
        if request.contains("/redirect ") {
            b"HTTP/1.1 302 Found\r\nLocation: http://untrusted-next.invalid/sub\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec()
        } else { response(b"proxies: []\n") }
    }).await;
    proxy_child("untrusted", proxy.address, "http://example.com/sub", "").await;
    assert_eq!(proxy.hits.load(Ordering::SeqCst), 0);
    proxy_child("trusted", proxy.address, "http://remote-only.invalid/sub", "").await;
    assert_eq!(proxy.hits.load(Ordering::SeqCst), 1);
    assert!(proxy.requests.lock()[0].starts_with("GET http://remote-only.invalid/sub "));
    proxy_child("localhost", proxy.address, "http://remote-only.invalid/sub", "").await;
    assert_eq!(proxy.hits.load(Ordering::SeqCst), 2);
    proxy_child("redirect", proxy.address, "http://remote-only.invalid/redirect", "").await;
    assert_eq!(
        proxy.hits.load(Ordering::SeqCst),
        3,
        "untrusted redirect must never reach even the selected proxy"
    );
}

#[tokio::test]
async fn proxy_failure_has_no_direct_fallback_and_no_proxy_exclusion_is_preserved() {
    let target = Server::start("127.0.0.2", |_| response(b"proxies: []\n")).await;
    let broken_proxy = Server::start("127.0.0.1", |_| Vec::new()).await;
    proxy_child("failed", broken_proxy.address, &target.url(), "").await;
    assert!(broken_proxy.hits.load(Ordering::SeqCst) >= 1);
    assert_eq!(
        target.hits.load(Ordering::SeqCst),
        0,
        "selected proxy failure must not leak a direct request"
    );
    let proxy = Server::start("127.0.0.1", |_| response(b"wrong proxy body")).await;
    proxy_child("no-proxy", proxy.address, &target.url(), "127.0.0.2").await;
    assert_eq!(proxy.hits.load(Ordering::SeqCst), 0);
    assert_eq!(target.hits.load(Ordering::SeqCst), 1);
}
