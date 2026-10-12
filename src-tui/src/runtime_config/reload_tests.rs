//! Isolated localhost regression coverage for ambiguous Mihomo reloads.
use super::*;
use crate::mihomo_api::{MihomoApi, Transport};
use crate::profile_store::store::ProfileStore;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

#[derive(Clone, Copy)]
pub(crate) enum Mode {
    Accept(Duration),
    Timeout(bool),
    LostResponse,
    LostBody,
    Unready,
    Reject,
    PendingIo(bool),
}

pub(crate) struct MockController {
    pub(crate) addr: std::net::SocketAddr,
    pub(crate) puts: Arc<AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
}

impl MockController {
    pub(crate) async fn new(mode: Mode, home: Option<std::path::PathBuf>) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let puts = Arc::new(AtomicUsize::new(0));
        let counter = puts.clone();
        let task = tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let counter = counter.clone();
                let home = home.clone();
                tokio::spawn(async move {
                    let mut request = Vec::new();
                    let mut buffer = [0; 4096];
                    loop {
                        let count = stream.read(&mut buffer).await.unwrap();
                        if count == 0 {
                            return;
                        }
                        request.extend_from_slice(&buffer[..count]);
                        if let Some(end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                            let headers = String::from_utf8_lossy(&request[..end]);
                            let length = headers
                                .lines()
                                .find_map(|line| {
                                    let (name, value) = line.split_once(':')?;
                                    name.eq_ignore_ascii_case("content-length")
                                        .then(|| value.trim().parse::<usize>().unwrap())
                                })
                                .unwrap_or(0);
                            if request.len() >= end + 4 + length {
                                break;
                            }
                        }
                    }
                    let request = String::from_utf8_lossy(&request);
                    let is_put = request.starts_with("PUT /configs?force=true ");
                    let (status, body) = if is_put {
                        counter.fetch_add(1, Ordering::SeqCst);
                        match mode {
                            Mode::Accept(delay) => {
                                tokio::time::sleep(delay).await;
                                ("204 No Content", "")
                            }
                            Mode::Timeout(_) => {
                                tokio::time::sleep(Duration::from_millis(150)).await;
                                ("204 No Content", "")
                            }
                            Mode::LostResponse => return,
                            Mode::LostBody => {
                                let _ = stream
                                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\nConnection: close\r\n\r\nx")
                                    .await;
                                return;
                            }
                            Mode::Unready => ("204 No Content", ""),
                            Mode::Reject => ("400 Bad Request", "invalid candidate"),
                            Mode::PendingIo(accepted) => {
                                let path = reload_pending_path(&home.unwrap().join("config.yaml"));
                                std::fs::remove_file(&path).unwrap();
                                std::fs::create_dir(&path).unwrap();
                                if accepted {
                                    ("204 No Content", "")
                                } else {
                                    ("400 Bad Request", "invalid candidate")
                                }
                            }
                        }
                    } else if matches!(mode, Mode::Unready | Mode::Timeout(false)) {
                        ("503 Unavailable", "unavailable")
                    } else {
                        assert!(request.starts_with("GET /version "));
                        ("200 OK", r#"{"version":"v1.19.32"}"#)
                    };
                    let _ = stream
                        .write_all(
                            format!(
                                "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                                body.len()
                            )
                            .as_bytes(),
                        )
                        .await;
                });
            }
        });
        Self { addr, puts, task }
    }

    pub(crate) fn api(&self, short: bool) -> MihomoApi {
        let api = MihomoApi::with_transport(Transport::Tcp(self.addr), "fixture").unwrap();
        if short {
            api.with_reload_timeout(Duration::from_millis(25)).unwrap()
        } else {
            api
        }
    }
}
impl Drop for MockController {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub(crate) fn seed(home: &std::path::Path) -> clash_verge_core::config::PrfItem {
    std::fs::write(
        home.join("verge.yaml"),
        "enable_tun_mode: false\nverge_mixed_port: 35123\n",
    )
    .unwrap();
    std::fs::write(home.join("config.yaml"), "# exact old bytes\nmode: rule\nmixed-port: 35123\nsecret: fixture\nfuture: {old: true}\ntun: {enable: false}\n").unwrap();
    std::fs::write(home.join("profiles.yaml"), "# exact metadata\ncurrent: old\nfuture: {keep: true}\nitems:\n  - {uid: old, type: local, file: old.yaml}\n  - {uid: next, type: remote, file: next.yaml, updated: 1, extra: {upload: 10, download: 20, total: 100, expire: 0}}\n").unwrap();
    std::fs::write(home.join("profiles/old.yaml"), "rules: ['MATCH,DIRECT']\n").unwrap();
    std::fs::write(
        home.join("profiles/next.yaml"),
        "rules: ['DOMAIN,new.example,DIRECT', 'MATCH,DIRECT']\nfuture: {candidate: true}\n",
    )
    .unwrap();
    clash_verge_core::config::PrfItem {
        uid: Some("next".into()),
        itype: Some("remote".into()),
        file: Some("next.yaml".into()),
        ..Default::default()
    }
}

#[tokio::test]
async fn issue80_reload_delay_over_five_seconds_uses_long_deadline() {
    let mock = MockController::new(Mode::Accept(Duration::from_millis(5_500)), None).await;
    assert!(matches!(
        mock.api(false)
            .reload_config(std::path::Path::new("/tmp/isolated-candidate.yaml"))
            .await,
        crate::mihomo_api::client::ReloadOutcome::Confirmed
    ));
    assert_eq!(mock.puts.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn issue80_uncertain_preserves_candidate_uid_and_private_pending_for_all_response_failures() {
    for mode in [
        Mode::Timeout(true),
        Mode::Timeout(false),
        Mode::LostResponse,
        Mode::LostBody,
        Mode::Unready,
    ] {
        let home = tempfile::tempdir().unwrap();
        let _guard = crate::profile_store::store::tests::claim_test_app_home(home.path().to_path_buf()).await;
        let item = seed(home.path());
        let mock = MockController::new(mode, None).await;
        let error = crate::services::profile::switch_profile(&mock.api(true), &item, false, true)
            .await
            .unwrap_err();
        assert!(error.contains("candidate retained"), "{error}");
        assert_eq!(
            ProfileStore::snapshot().await.unwrap().current_uid().as_deref(),
            Some("next")
        );
        let path = home.path().join("config.yaml");
        let bytes = std::fs::read(&path).unwrap();
        let mapping: serde_yaml_ng::Mapping = serde_yaml_ng::from_slice(&bytes).unwrap();
        assert_eq!(mapping["future"]["candidate"], serde_yaml_ng::Value::Bool(true));
        let pending = read_reload_pending(&path).unwrap().unwrap();
        assert_eq!(pending.sha256, runtime_hash(&bytes).unwrap());
        assert_eq!(pending.profile_uid.as_deref(), Some("next"));
        use std::os::unix::fs::PermissionsExt as _;
        for private in [path, reload_pending_path(&home.path().join("config.yaml"))] {
            assert_eq!(std::fs::metadata(private).unwrap().permissions().mode() & 0o777, 0o600);
        }
        assert_eq!(
            mock.puts.load(Ordering::SeqCst),
            1,
            "uncertainty never sends rollback PUT"
        );
    }
}

#[tokio::test]
async fn issue80_timeout_with_unreachable_health_keeps_candidate_and_uid() {
    let home = tempfile::tempdir().unwrap();
    let _guard = crate::profile_store::store::tests::claim_test_app_home(home.path().to_path_buf()).await;
    let item = seed(home.path());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        drop(listener); // the health probe now receives connection refused
        let mut bytes = [0; 8192];
        let count = stream.read(&mut bytes).await.unwrap();
        assert!(String::from_utf8_lossy(&bytes[..count]).starts_with("PUT /configs?force=true "));
        tokio::time::sleep(Duration::from_millis(150)).await;
    });
    let api = MihomoApi::with_transport(Transport::Tcp(addr), "fixture")
        .unwrap()
        .with_reload_timeout(Duration::from_millis(25))
        .unwrap();
    let error = crate::services::profile::switch_profile(&api, &item, false, true)
        .await
        .unwrap_err();
    assert!(
        error.contains("uncertain") && error.contains("health unavailable"),
        "{error}"
    );
    assert!(
        std::fs::read_to_string(home.path().join("config.yaml"))
            .unwrap()
            .contains("candidate: true")
    );
    assert!(read_reload_pending(&home.path().join("config.yaml")).unwrap().is_some());
    assert_eq!(
        ProfileStore::snapshot().await.unwrap().current_uid().as_deref(),
        Some("next")
    );
    server.await.unwrap();
}

#[tokio::test]
async fn issue80_explicit_rejection_and_pre_send_connect_failure_restore_exact_runtime_and_uid() {
    for reject in [true, false] {
        let home = tempfile::tempdir().unwrap();
        let _guard = crate::profile_store::store::tests::claim_test_app_home(home.path().to_path_buf()).await;
        let item = seed(home.path());
        let path = home.path().join("config.yaml");
        let original = std::fs::read(&path).unwrap();
        let mock = MockController::new(Mode::Reject, None).await;
        let api = if reject {
            mock.api(false)
        } else {
            MihomoApi::new(home.path().join("absent.sock"), "fixture").unwrap()
        };
        let error = crate::services::profile::switch_profile(&api, &item, false, true)
            .await
            .unwrap_err();
        assert!(error.contains("restored the previous config"), "{error}");
        assert_eq!(std::fs::read(&path).unwrap(), original);
        assert_eq!(
            ProfileStore::snapshot().await.unwrap().current_uid().as_deref(),
            Some("old")
        );
        assert!(read_reload_pending(&path).unwrap().is_none());
        assert_eq!(
            mock.puts.load(Ordering::SeqCst),
            usize::from(reject),
            "no rollback PUT is needed on explicit rejection"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn issue80_identical_pending_retry_in_new_process_reloads_reordered_candidate_then_clears_pending() {
    let home = tempfile::tempdir().unwrap();
    let _guard = crate::profile_store::store::tests::claim_test_app_home(home.path().to_path_buf()).await;
    let item = seed(home.path());
    let first = MockController::new(Mode::LostResponse, None).await;
    crate::services::profile::switch_profile(&first.api(false), &item, false, true)
        .await
        .unwrap_err();
    let path = home.path().join("config.yaml");
    let mapping: serde_yaml_ng::Mapping = serde_yaml_ng::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let mut reordered = serde_yaml_ng::Mapping::new();
    for (key, value) in mapping.into_iter().collect::<Vec<_>>().into_iter().rev() {
        reordered.insert(key, value);
    }
    std::fs::write(
        &path,
        format!(
            "# external formatting change\n{}",
            serde_yaml_ng::to_string(&reordered).unwrap()
        ),
    )
    .unwrap();
    let second = MockController::new(Mode::Accept(Duration::ZERO), None).await;
    let task_home = home.path().to_path_buf();
    let addr = second.addr;
    let output = tokio::task::spawn_blocking(move || {
        std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "runtime_config::reload_tests::issue80_retry_subprocess_helper",
                "--nocapture",
            ])
            .env("ISSUE80_RETRY_HOME", task_home)
            .env("ISSUE80_RETRY_ADDR", addr.to_string())
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(second.puts.load(Ordering::SeqCst), 1);
    assert!(read_reload_pending(&path).unwrap().is_none());
    assert_eq!(
        ProfileStore::snapshot().await.unwrap().current_uid().as_deref(),
        Some("next")
    );
    assert_eq!(
        commit_runtime_config_with_outcome(&second.api(false), false, true, None, Ok)
            .await
            .unwrap()
            .1,
        ApplyOutcome::Unchanged
    );
    assert_eq!(
        second.puts.load(Ordering::SeqCst),
        1,
        "confirmed subsequent no-op is preserved"
    );
}

#[test]
fn issue80_retry_subprocess_helper() {
    let Some(home) = std::env::var_os("ISSUE80_RETRY_HOME") else {
        return;
    };
    let home = std::path::PathBuf::from(home);
    let addr = std::env::var("ISSUE80_RETRY_ADDR").unwrap().parse().unwrap();
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let _guard = crate::profile_store::store::tests::claim_test_app_home(home).await;
        let api = MihomoApi::with_transport(Transport::Tcp(addr), "fixture").unwrap();
        assert_eq!(
            commit_runtime_config_with_outcome(&api, false, true, None, Ok)
                .await
                .unwrap()
                .1,
            ApplyOutcome::Applied
        );
    });
}

#[tokio::test]
async fn issue80_cancel_inflight_retains_candidate_pending_uid_and_releases_io_lock() {
    let home = tempfile::tempdir().unwrap();
    let _guard = crate::profile_store::store::tests::claim_test_app_home(home.path().to_path_buf()).await;
    let item = seed(home.path());
    let mock = MockController::new(Mode::Accept(Duration::from_secs(30)), None).await;
    let api = mock.api(false);
    let task = tokio::spawn(async move { crate::services::profile::switch_profile(&api, &item, false, true).await });
    tokio::time::timeout(Duration::from_secs(2), async {
        while mock.puts.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    let path = home.path().join("config.yaml");
    assert!(read_reload_pending(&path).unwrap().is_some());
    assert!(std::fs::read_to_string(&path).unwrap().contains("candidate: true"));
    assert_eq!(
        ProfileStore::snapshot().await.unwrap().current_uid().as_deref(),
        Some("next")
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(100), RUNTIME_CONFIG_IO.lock())
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn issue80_pending_io_failure_preserves_correct_rollback_authority() {
    for accepted in [true, false] {
        let home = tempfile::tempdir().unwrap();
        let _guard = crate::profile_store::store::tests::claim_test_app_home(home.path().to_path_buf()).await;
        let item = seed(home.path());
        let path = home.path().join("config.yaml");
        let old = std::fs::read(&path).unwrap();
        let mock = MockController::new(Mode::PendingIo(accepted), Some(home.path().to_path_buf())).await;
        let error = crate::services::profile::switch_profile(&mock.api(false), &item, false, true)
            .await
            .unwrap_err();
        assert!(error.contains("pending state"), "{error}");
        let uid = ProfileStore::snapshot().await.unwrap().current_uid().unwrap();
        if accepted {
            assert_eq!(uid, "next");
            assert_ne!(std::fs::read(&path).unwrap(), old);
        } else {
            assert_eq!(uid, "old");
            assert_eq!(std::fs::read(&path).unwrap(), old);
        }
    }
}

#[tokio::test]
async fn issue80_stale_pending_does_not_override_unrelated_unchanged_candidate() {
    let home = tempfile::tempdir().unwrap();
    let _guard = crate::profile_store::store::tests::claim_test_app_home(home.path().to_path_buf()).await;
    seed(home.path());
    let api = MihomoApi::new(home.path().join("absent.sock"), "fixture").unwrap();
    commit_runtime_config_with_outcome(&api, false, false, None, Ok)
        .await
        .unwrap();
    let path = home.path().join("config.yaml");
    write_reload_pending(
        &path,
        &ReloadPending {
            sha256: runtime_hash(b"future: unrelated\n").unwrap(),
            profile_uid: None,
        },
    )
    .unwrap();
    assert_eq!(
        commit_runtime_config_with_outcome(&api, false, true, None, Ok)
            .await
            .unwrap()
            .1,
        ApplyOutcome::Unchanged
    );
}
