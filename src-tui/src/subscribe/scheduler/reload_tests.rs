use super::*;
use crate::runtime_config::reload_tests::{MockController, Mode, seed};
use crate::runtime_config::{ApplyError, ApplyOutcome};
use async_trait::async_trait;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

struct ReloadLifecycle {
    api: MihomoApi,
    reloads: AtomicUsize,
    selections: AtomicUsize,
}
#[async_trait]
impl CoreLifecycle for ReloadLifecycle {
    fn core_kind(&self) -> CoreKind {
        CoreKind::Mihomo
    }
    fn is_running(&self) -> bool {
        true
    }
    fn owns_core(&self) -> bool {
        false
    }
    async fn current_exit_node(&self, _: &[PrfItem], _: &str) -> Option<String> {
        Some("JP-1".into())
    }
    async fn delay_test(&self, _: &str, _: &str, _: u64) -> Result<ProxyDelay, MihomoError> {
        Ok(ProxyDelay { delay: 10 })
    }
    async fn reload_profile(&self, uid: &str, tun: bool) -> Result<ApplyOutcome, ApplyError> {
        self.reloads.fetch_add(1, Ordering::SeqCst);
        reload_current_profile(&self.api, uid, tun, true).await
    }
    async fn reapply_selected(&self, _: &str) -> Result<(), String> {
        self.selections.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[tokio::test]
async fn issue80_forced_refresh_uncertain_retains_new_source_metadata_without_rollback_apply() {
    forced_refresh_case(Mode::Timeout(true), false).await;
    forced_refresh_case(Mode::Unready, false).await;
}

#[tokio::test]
async fn issue80_forced_refresh_explicit_rejection_restores_exact_source_metadata_and_runtime() {
    forced_refresh_case(Mode::Reject, true).await;
}

async fn forced_refresh_case(mode: Mode, rejected: bool) {
    let home = tempfile::tempdir().unwrap();
    let _guard = crate::profile_store::store::tests::claim_test_app_home(home.path().to_path_buf()).await;
    let item = seed(home.path());
    let body = "proxies:\n  - {name: JP-1, type: ss, server: old.example, port: 443, cipher: aes-128-gcm, password: fixture}\nrules: ['MATCH,DIRECT']\n";
    std::fs::write(home.path().join("profiles/next.yaml"), body).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let refreshed = "proxies:\n  - {name: JP-1, type: ss, server: new.example, port: 443, cipher: aes-128-gcm, password: fixture}\nrules: ['MATCH,DIRECT']\nfuture: {refresh: true}\n";
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut buffer = [0; 8192];
        let count = stream.read(&mut buffer).await.unwrap();
        assert!(String::from_utf8_lossy(&buffer[..count]).starts_with("GET /subscription "));
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nSubscription-Userinfo: upload=200; download=300; total=1000; expire=0\r\nProfile-Web-Page-Url: https://new.example\r\nConnection: close\r\n\r\n{refreshed}",
            refreshed.len()
        );
        stream.write_all(response.as_bytes()).await.unwrap();
    });
    let metadata = format!(
        "# exact old refresh metadata\ncurrent: next\nfuture: {{keep: true}}\nitems:\n  - {{uid: old, type: local, file: old.yaml}}\n  - uid: next\n    type: remote\n    file: next.yaml\n    url: http://{addr}/subscription\n    updated: 1\n    extra: {{upload: 10, download: 20, total: 100, expire: 0, future: kept}}\n    option: {{trusted_hosts: [127.0.0.1], with_proxy: false, self_proxy: false}}\n"
    );
    std::fs::write(home.path().join("profiles.yaml"), &metadata).unwrap();
    let absent = MihomoApi::new(home.path().join("absent.sock"), "fixture").unwrap();
    crate::runtime_config::reload_remote_profile_with_outcome(&absent, &item, false, false)
        .await
        .unwrap();
    let old_runtime = std::fs::read(home.path().join("config.yaml")).unwrap();
    let mock = MockController::new(mode, None).await;
    let lifecycle = ReloadLifecycle {
        api: mock.api(true),
        reloads: AtomicUsize::new(0),
        selections: AtomicUsize::new(0),
    };
    let outcome = AutoUpdateScheduler::force_refresh_with_lifecycle(&lifecycle, "next", "JP-1", false).await;
    server.await.unwrap();
    assert!(
        outcome.error.is_some(),
        "reload failure must never be reported as refresh success"
    );
    assert_eq!(outcome.rolled_back, rejected, "{:?}", outcome.error);
    assert_eq!(
        mock.puts.load(Ordering::SeqCst),
        1,
        "uncertainty and rejection must not send rollback PUT"
    );
    assert_eq!(
        lifecycle.selections.load(Ordering::SeqCst),
        0,
        "no selection follows unconfirmed reload"
    );
    let source = std::fs::read_to_string(home.path().join("profiles/next.yaml")).unwrap();
    let saved_metadata = std::fs::read_to_string(home.path().join("profiles.yaml")).unwrap();
    if rejected {
        assert_eq!(source, body);
        assert_eq!(
            saved_metadata, metadata,
            "quota/home/option and unknown metadata restore exactly"
        );
        assert_eq!(std::fs::read(home.path().join("config.yaml")).unwrap(), old_runtime);
        assert_eq!(
            lifecycle.reloads.load(Ordering::SeqCst),
            2,
            "old source comparison is a safe no-op"
        );
    } else {
        assert_eq!(source, refreshed);
        assert_ne!(saved_metadata, metadata);
        let store = ProfileStore::snapshot().await.unwrap();
        let refreshed_item = store
            .items()
            .into_iter()
            .find(|item| item.uid.as_deref() == Some("next"))
            .unwrap();
        assert_eq!(refreshed_item.extra.unwrap().upload, 200);
        assert_eq!(refreshed_item.home.as_deref(), Some("https://new.example"));
        assert!(home.path().join(".mihomo-reload-pending.json").is_file());
        assert_eq!(
            lifecycle.reloads.load(Ordering::SeqCst),
            1,
            "no competing old-source apply follows uncertainty"
        );
    }
    // An uncertain request can still complete asynchronously after the fixture.
    tokio::time::sleep(Duration::from_millis(1)).await;
}
