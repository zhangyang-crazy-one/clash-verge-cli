use super::*;
use crate::subscribe::fetch::security_tests::Server;
use crate::subscribe::test_transport::{self, Fixture};
use std::sync::atomic::Ordering;

async fn receive(rx: &mut mpsc::Receiver<Action>) -> Action {
    tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
        .await
        .expect("background result within fixture budget")
        .expect("event sender open")
}

#[tokio::test]
async fn issue86_proxy_only_import_reaches_host_specific_confirmation() {
    let home = tempfile::tempdir().unwrap();
    let _home = crate::profile_store::store::tests::claim_test_app_home(home.path().into()).await;
    let server = Server::start("127.0.0.1", |_| {
        b"HTTP/1.1 200 OK\r\nContent-Length: 12\r\nConnection: close\r\n\r\nproxies: []\n".to_vec()
    })
    .await;
    let _transport = test_transport::install(Fixture {
        address: server.address,
        direct_success: false,
    });
    let (ctx, mut rx) = tests::ctx();
    let mut app = App::new();
    let url = "http://initial.issue86.test/sub?token=keep-private";
    handle_intent(&mut app, &ctx, Action::ConfirmImport(url.into())).await;
    let event = receive(&mut rx).await;
    handle_event(&mut app, &ctx, event).await;
    assert_eq!(app.overlay, Some(Overlay::TrustConfirmation), "{:?}", app.status_msg);
    let pending = app.pending_trust.as_ref().unwrap();
    assert_eq!(pending.host, "initial.issue86.test");
    assert_eq!(pending.url, url);
    assert!(pending.proxy_dns);
    assert!(!app.status_msg.as_deref().unwrap().contains("keep-private"));
    assert_eq!(server.hits.load(Ordering::SeqCst), 0);
    assert!(!home.path().join("profiles.yaml").exists());
    handle_event(&mut app, &ctx, Action::ConfirmTrustImport).await;
    assert!(matches!(
        ctx.tx.accept(receive(&mut rx).await),
        Some(Action::ProfileImported)
    ));
    let stored = crate::profile_store::store::ProfileStore::snapshot().await.unwrap();
    let items = stored.items();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].url.as_deref(), Some(url));
    assert_eq!(
        items[0].option.as_ref().unwrap().trusted_hosts.as_ref().unwrap(),
        &["initial.issue86.test"]
    );
    assert_eq!(server.hits.load(Ordering::SeqCst), 1);
    assert!(app.pending_trust.is_none() && app.overlay.is_none());
}

async fn subscription_server() -> Server {
    Server::start("127.0.0.1", |_| {
        b"HTTP/1.1 200 OK\r\nContent-Length: 12\r\nConnection: close\r\n\r\nproxies: []\n".to_vec()
    })
    .await
}

#[tokio::test]
async fn issue86_cancel_real_proxy_prompt_writes_no_profile_or_trust() {
    let home = tempfile::tempdir().unwrap();
    let _home = crate::profile_store::store::tests::claim_test_app_home(home.path().into()).await;
    let server = subscription_server().await;
    let _transport = test_transport::install(Fixture {
        address: server.address,
        direct_success: false,
    });
    let (ctx, mut rx) = tests::ctx();
    let mut app = App::new();
    handle_event(
        &mut app,
        &ctx,
        Action::ConfirmImport("http://cancel.issue86.test/sub".into()),
    )
    .await;
    handle_event(&mut app, &ctx, receive(&mut rx).await).await;
    assert_eq!(app.overlay, Some(Overlay::TrustConfirmation));
    handle_event(&mut app, &ctx, Action::CancelTrustImport).await;
    handle_event(&mut app, &ctx, Action::ConfirmTrustImport).await;
    assert!(app.pending_trust.is_none() && app.overlay.is_none());
    assert!(rx.try_recv().is_err());
    assert!(!home.path().join("profiles.yaml").exists());
    assert_eq!(std::fs::read_dir(home.path().join("profiles")).unwrap().count(), 0);
    assert_eq!(server.hits.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn issue86_redirect_requires_separate_confirm_and_preserves_options_name_and_exact_hosts() {
    use clash_verge_core::config::PrfOption;
    let home = tempfile::tempdir().unwrap();
    let _home = crate::profile_store::store::tests::claim_test_app_home(home.path().into()).await;
    let server = Server::start("127.0.0.1", |request| {
        let target = request.split_whitespace().nth(1).unwrap();
        let location = if target.contains("initial.issue86.test/sub") {
            Some("http://redirect.issue86.test/next")
        } else if target.contains("redirect.issue86.test/next") {
            Some("http://initial.issue86.test/final")
        } else {
            None
        };
        if let Some(location) = location {
            format!("HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .into_bytes()
        } else {
            b"HTTP/1.1 200 OK\r\nContent-Length: 12\r\nConnection: close\r\n\r\nproxies: []\n".to_vec()
        }
    })
    .await;
    let _transport = test_transport::install(Fixture {
        address: server.address,
        direct_success: false,
    });
    let (ctx, mut rx) = tests::ctx();
    let mut app = App::new();
    let option = PrfOption {
        user_agent: Some("custom-issue86-agent".into()),
        timeout_seconds: Some(2),
        update_interval: Some(777),
        trusted_hosts: Some(vec!["existing.issue86.test".into()]),
        unknown_fields: serde_yaml_ng::from_str("custom-policy: retained").unwrap(),
        ..Default::default()
    };
    let url = "http://initial.issue86.test/sub?token=preserve";
    // Exercise the real background importer and fallback, carrying the same
    // optional request fields that must survive every confirmation retry.
    profile::spawn_import_request(&ctx.tx, url.into(), Some("Requested name".into()), Some(option));
    handle_event(&mut app, &ctx, receive(&mut rx).await).await;
    assert_eq!(app.pending_trust.as_ref().unwrap().host, "initial.issue86.test");
    handle_event(&mut app, &ctx, Action::ConfirmTrustImport).await;
    handle_event(&mut app, &ctx, receive(&mut rx).await).await;
    let pending = app.pending_trust.as_ref().unwrap();
    assert_eq!(pending.host, "redirect.issue86.test");
    assert_eq!(pending.url, url);
    assert_eq!(pending.name.as_deref(), Some("Requested name"));
    assert_eq!(
        pending.option.as_ref().unwrap().trusted_hosts.as_ref().unwrap(),
        &["existing.issue86.test", "initial.issue86.test"]
    );
    assert!(!home.path().join("profiles.yaml").exists());
    assert!(
        server
            .requests
            .lock()
            .iter()
            .all(|request| !request.contains("GET http://redirect.issue86.test")),
        "untrusted redirect must receive zero requests"
    );
    handle_event(&mut app, &ctx, Action::ConfirmTrustImport).await;
    assert!(matches!(
        ctx.tx.accept(receive(&mut rx).await),
        Some(Action::ProfileImported)
    ));
    let stored = crate::profile_store::store::ProfileStore::snapshot().await.unwrap();
    let item = stored.items().pop().unwrap();
    assert_eq!(item.name.as_deref(), Some("Requested name"));
    assert_eq!(item.url.as_deref(), Some(url));
    let option = item.option.unwrap();
    assert_eq!(
        option.trusted_hosts.unwrap(),
        ["existing.issue86.test", "initial.issue86.test", "redirect.issue86.test"]
    );
    assert_eq!(option.user_agent.as_deref(), Some("custom-issue86-agent"));
    assert_eq!(option.timeout_seconds, Some(2));
    assert_eq!(option.update_interval, Some(777));
    assert_eq!(
        option.unknown_fields.get("custom-policy").unwrap().as_str(),
        Some("retained")
    );
    assert!(
        server
            .requests
            .lock()
            .iter()
            .all(|request| request.contains("custom-issue86-agent"))
    );
}

#[tokio::test]
async fn issue86_safe_direct_public_import_succeeds_without_trust_prompt() {
    let home = tempfile::tempdir().unwrap();
    let _home = crate::profile_store::store::tests::claim_test_app_home(home.path().into()).await;
    let server = subscription_server().await;
    let _transport = test_transport::install(Fixture {
        address: server.address,
        direct_success: true,
    });
    let (ctx, mut rx) = tests::ctx();
    let mut app = App::new();
    let url = format!("http://direct.issue86.test:{}/sub", server.address.port());
    handle_event(&mut app, &ctx, Action::ConfirmImport(url)).await;
    assert!(matches!(
        ctx.tx.accept(receive(&mut rx).await),
        Some(Action::ProfileImported)
    ));
    assert!(app.pending_trust.is_none() && app.overlay.is_none());
    let items = crate::profile_store::store::ProfileStore::snapshot()
        .await
        .unwrap()
        .items();
    let option = items[0].option.as_ref().unwrap();
    assert!(option.trusted_hosts.is_none());
    assert_eq!(
        crate::subscribe::fetch::proxy_mode_from_option(Some(option)),
        crate::subscribe::fetch::ProxyMode::None
    );
    assert_eq!(server.hits.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn issue86_already_trusted_import_has_no_new_prompt_or_extra_grant() {
    let home = tempfile::tempdir().unwrap();
    let _home = crate::profile_store::store::tests::claim_test_app_home(home.path().into()).await;
    let server = subscription_server().await;
    let _transport = test_transport::install(Fixture {
        address: server.address,
        direct_success: false,
    });
    let (ctx, mut rx) = tests::ctx();
    let app = App::new();
    profile::spawn_import(
        &ctx.tx,
        "http://trusted.issue86.test/sub".into(),
        Some(clash_verge_core::config::PrfOption {
            trusted_hosts: Some(vec!["trusted.issue86.test".into()]),
            ..Default::default()
        }),
    );
    assert!(matches!(
        ctx.tx.accept(receive(&mut rx).await),
        Some(Action::ProfileImported)
    ));
    assert!(app.pending_trust.is_none() && app.overlay.is_none());
    let items = crate::profile_store::store::ProfileStore::snapshot()
        .await
        .unwrap()
        .items();
    assert_eq!(
        items[0].option.as_ref().unwrap().trusted_hosts.as_ref().unwrap(),
        &["trusted.issue86.test"]
    );
    assert_eq!(server.hits.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn issue86_auto_refresh_reports_proxy_trust_error_without_interactive_prompt() {
    let home = tempfile::tempdir().unwrap();
    let _home = crate::profile_store::store::tests::claim_test_app_home(home.path().into()).await;
    let server = subscription_server().await;
    let _transport = test_transport::install(Fixture {
        address: server.address,
        direct_success: false,
    });
    std::fs::write(home.path().join("profiles.yaml"), "items:\n  - uid: R86\n    type: remote\n    url: http://background.issue86.test/sub\n    file: R86.yaml\n    updated: 1\n    option:\n      allow_auto_update: true\n      update_interval: 1\n      user_agent: issue86-background\n").unwrap();
    std::fs::write(home.path().join("profiles/R86.yaml"), "proxies: []\n").unwrap();
    let before = std::fs::read(home.path().join("profiles.yaml")).unwrap();
    let (ctx, mut rx) = tests::ctx();
    let mut app = App::new();
    let scheduler = Arc::new(tokio::sync::Mutex::new(
        crate::subscribe::scheduler::AutoUpdateScheduler::default(),
    ));
    profile::spawn_auto_update(&app, &ctx, scheduler);
    let failed = receive(&mut rx).await;
    assert!(
        matches!(ctx.tx.accept(failed.clone()), Some(Action::ProfileUpdateFailed(error)) if error.contains("background.issue86.test") && error.contains("remote proxy DNS"))
    );
    handle_event(&mut app, &ctx, failed).await;
    handle_event(&mut app, &ctx, receive(&mut rx).await).await;
    assert!(app.pending_trust.is_none() && app.overlay.is_none());
    assert_eq!(std::fs::read(home.path().join("profiles.yaml")).unwrap(), before);
    assert_eq!(server.hits.load(Ordering::SeqCst), 0);
}
