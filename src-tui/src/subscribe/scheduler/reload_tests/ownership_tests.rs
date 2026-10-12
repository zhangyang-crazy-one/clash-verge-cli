use super::*;
use crate::profile_store::store::tests::claim_test_app_home;
use clash_verge_core::config::PrfOption;
use serde_yaml_ng::Value;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{Mutex, oneshot};

const OLD: &str =
    "proxies:\n  - {name: JP-1, type: ss, server: old.example, port: 443, cipher: aes-128-gcm, password: fixture}\n";
const NEW: &str =
    "proxies:\n  - {name: JP-1, type: ss, server: new.example, port: 443, cipher: aes-128-gcm, password: fixture}\n";
const LATER: &str =
    "proxies:\n  - {name: JP-1, type: ss, server: later.example, port: 443, cipher: aes-128-gcm, password: fixture}\n";

#[derive(Clone, Copy)]
enum LaterEdit {
    None,
    Quota,
    Source,
    FragmentReference,
    FragmentChanges,
    BindingUrl,
    BindingType,
}

struct RejectAfterEdits {
    home: PathBuf,
    done: Mutex<Option<oneshot::Receiver<()>>>,
    later: LaterEdit,
    reloads: AtomicUsize,
}

#[async_trait]
impl CoreLifecycle for RejectAfterEdits {
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
        Ok(ProxyDelay { delay: 1 })
    }
    async fn reload_profile(&self, _: &str, _: bool) -> Result<ApplyOutcome, ApplyError> {
        if self.reloads.fetch_add(1, Ordering::SeqCst) != 0 {
            return Ok(ApplyOutcome::Unchanged);
        }
        self.done.lock().await.take().unwrap().await.unwrap();
        // This mutation occurs after the real service captured its expected
        // state; no store operation is running while this lifecycle owns it.
        match self.later {
            LaterEdit::None => {}
            LaterEdit::Quota => {
                let path = self.home.join("profiles.yaml");
                let mut metadata: Value = serde_yaml_ng::from_slice(&std::fs::read(&path).unwrap()).unwrap();
                let item = metadata["items"]
                    .as_sequence_mut()
                    .unwrap()
                    .iter_mut()
                    .find(|item| item["uid"].as_str() == Some("next"))
                    .unwrap();
                item["extra"]["upload"] = Value::from(999);
                item["extra"]["late_unknown"] = Value::from("keep");
                std::fs::write(path, serde_yaml_ng::to_string(&metadata).unwrap()).unwrap();
            }
            LaterEdit::FragmentReference => {
                let path = self.home.join("profiles.yaml");
                let mut metadata: Value = serde_yaml_ng::from_slice(&std::fs::read(&path).unwrap()).unwrap();
                let items = metadata["items"].as_sequence_mut().unwrap();
                let uid = items.iter().find(|item| item["uid"] == "next").unwrap()["option"]["merge"].clone();
                items.iter_mut().find(|item| item["uid"] == "B").unwrap()["option"] =
                    serde_yaml_ng::from_str(&format!("merge: {}", uid.as_str().unwrap())).unwrap();
                std::fs::write(path, serde_yaml_ng::to_string(&metadata).unwrap()).unwrap();
            }
            LaterEdit::FragmentChanges => {
                let path = self.home.join("profiles.yaml");
                let mut metadata: Value = serde_yaml_ng::from_slice(&std::fs::read(&path).unwrap()).unwrap();
                let items = metadata["items"].as_sequence_mut().unwrap();
                let option = items.iter().find(|item| item["uid"] == "next").unwrap()["option"].clone();
                let merge = items.iter_mut().find(|item| item["uid"] == option["merge"]).unwrap();
                merge["later_unknown"] = Value::from("keep");
                let script = items.iter().find(|item| item["uid"] == option["script"]).unwrap();
                std::fs::write(
                    self.home.join("profiles").join(script["file"].as_str().unwrap()),
                    "// later script edit\n",
                )
                .unwrap();
                std::fs::write(path, serde_yaml_ng::to_string(&metadata).unwrap()).unwrap();
            }
            LaterEdit::BindingUrl | LaterEdit::BindingType => {
                let path = self.home.join("profiles.yaml");
                let mut metadata: Value = serde_yaml_ng::from_slice(&std::fs::read(&path).unwrap()).unwrap();
                let item = metadata["items"]
                    .as_sequence_mut()
                    .unwrap()
                    .iter_mut()
                    .find(|item| item["uid"] == "next")
                    .unwrap();
                if matches!(self.later, LaterEdit::BindingUrl) {
                    item["url"] = Value::from("https://later.example/subscription");
                } else {
                    item["type"] = Value::from("local");
                }
                std::fs::write(path, serde_yaml_ng::to_string(&metadata).unwrap()).unwrap();
            }
            LaterEdit::Source => {
                std::fs::write(self.home.join("profiles/next.yaml"), LATER).unwrap();
            }
        }
        Err(ApplyError::Rejected("fixture explicitly rejects refresh".into()))
    }
    async fn reapply_selected(&self, _: &str) -> Result<(), String> {
        panic!("rejected/unchanged reload must not write selectors")
    }
}

#[tokio::test]
async fn issue87_rejected_refresh_preserves_queued_rename_current_trust_and_unknowns() {
    ownership_case(false, LaterEdit::None, true).await;
}

#[tokio::test]
async fn issue87_rejected_refresh_preserves_queued_url_import_and_its_fragment_files() {
    ownership_case(true, LaterEdit::None, true).await;
}

#[tokio::test]
async fn issue87_rejected_refresh_preserves_entire_newer_owned_transaction_on_later_quota() {
    ownership_case(false, LaterEdit::Quota, true).await;
}

#[tokio::test]
async fn issue87_rejected_refresh_refuses_to_overwrite_later_source() {
    ownership_case(false, LaterEdit::Source, true).await;
}

#[tokio::test]
async fn issue87_rejected_refresh_preserves_queued_rename_only() {
    ownership_case(false, LaterEdit::None, false).await;
}

#[tokio::test]
async fn issue87_rejected_refresh_preserves_queued_import_only() {
    ownership_case(true, LaterEdit::None, false).await;
}

#[tokio::test]
async fn issue87_rejected_refresh_keeps_generated_fragment_referenced_by_another_profile() {
    ownership_case(false, LaterEdit::FragmentReference, true).await;
}

#[tokio::test]
async fn issue87_rejected_refresh_keeps_later_modified_generated_fragment_metadata_and_file() {
    ownership_case(false, LaterEdit::FragmentChanges, true).await;
}

#[tokio::test]
async fn issue87_rejected_refresh_refuses_later_source_url_or_type_binding_changes() {
    ownership_case(false, LaterEdit::BindingUrl, true).await;
    ownership_case(false, LaterEdit::BindingType, true).await;
}

async fn ownership_case(import: bool, later: LaterEdit, more_edits: bool) {
    let home = tempfile::tempdir().unwrap();
    let _home = claim_test_app_home(home.path().to_path_buf()).await;
    std::fs::write(home.path().join("profiles/next.yaml"), OLD).unwrap();
    std::fs::write(home.path().join("profiles/B.yaml"), "proxies: []\n").unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (fetch_started, started) = oneshot::channel();
    let (release, response_released) = oneshot::channel();
    let server = tokio::spawn(async move {
        let mut block = Some((fetch_started, response_released));
        for _ in 0..if import { 2 } else { 1 } {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buffer = [0; 8192];
            let count = stream.read(&mut buffer).await.unwrap();
            assert!(String::from_utf8_lossy(&buffer[..count]).starts_with("GET /"));
            if let Some((started, release)) = block.take() {
                started.send(()).unwrap();
                release.await.unwrap();
            }
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nSubscription-Userinfo: upload=200; download=300; total=1000; expire=0\r\nProfile-Web-Page-Url: https://new.example\r\nConnection: close\r\n\r\n{NEW}",
                NEW.len()
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        }
    });
    let original = format!(
        "current: next\nfuture_root: {{keep: true}}\nitems:\n  - {{uid: B, type: local, file: B.yaml, name: before, future_B: kept}}\n  - uid: next\n    type: remote\n    name: A\n    file: next.yaml\n    url: http://{addr}/subscription\n    updated: 1\n    home: https://old.example\n    future_item: {{keep: true}}\n    extra: {{upload: 10, download: 20, total: 100, expire: 0, future_usage: kept}}\n    option: {{trusted_hosts: [127.0.0.1], with_proxy: false, self_proxy: false, future_option: kept}}\n"
    );
    std::fs::write(home.path().join("profiles.yaml"), &original).unwrap();
    let (edited, edits_done) = oneshot::channel();
    let lifecycle = Arc::new(RejectAfterEdits {
        home: home.path().to_path_buf(),
        done: Mutex::new(Some(edits_done)),
        later,
        reloads: AtomicUsize::new(0),
    });
    let refresh_lifecycle = lifecycle.clone();
    let refresh = tokio::spawn(async move {
        AutoUpdateScheduler::force_refresh_with_lifecycle(&*refresh_lifecycle, "next", "JP-1", false).await
    });
    started.await.unwrap();
    let (queued, queued_ready) = oneshot::channel();
    let editor_home = home.path().to_path_buf();
    let editor = tokio::spawn(async move {
        // Sending and polling the first *_locked call have no intervening
        // await, so observing queued_ready proves it is queued behind fetch.
        queued.send(()).unwrap();
        let imported = if import {
            Some(
                ProfileStore::import_url_locked(
                    &format!("http://{addr}/import"),
                    Some("imported-B"),
                    Some(&PrfOption {
                        trusted_hosts: Some(vec!["127.0.0.1".into()]),
                        with_proxy: Some(false),
                        self_proxy: Some(false),
                        ..Default::default()
                    }),
                )
                .await
                .unwrap(),
            )
        } else {
            ProfileStore::rename_locked("B", "after").await.unwrap();
            None
        };
        if more_edits {
            ProfileStore::replace_current_locked("B").await.unwrap();
            ProfileStore::add_trusted_host_locked("next", "later.example")
                .await
                .unwrap();
            ProfileStore::rename_locked("next", "A-later-name").await.unwrap();
        }
        let imported_uids: Vec<_> = imported
            .as_ref()
            .into_iter()
            .flat_map(|item| {
                let option = item.option.as_ref().unwrap();
                [
                    item.uid.clone(),
                    option.merge.clone(),
                    option.script.clone(),
                    option.rules.clone(),
                    option.proxies.clone(),
                    option.groups.clone(),
                ]
                .into_iter()
                .flatten()
            })
            .collect();
        let files: Vec<_> = ProfileStore::snapshot()
            .await
            .unwrap()
            .all_items()
            .into_iter()
            .filter(|item| {
                item.uid.as_deref() == Some("B") || item.uid.as_ref().is_some_and(|uid| imported_uids.contains(uid))
            })
            .filter_map(|item| item.file)
            .map(|file| {
                let bytes = std::fs::read(editor_home.join("profiles").join(file.as_str())).unwrap();
                (file, bytes)
            })
            .collect();
        edited.send(()).unwrap();
        (imported, files)
    });
    queued_ready.await.unwrap();
    release.send(()).unwrap();
    let outcome = tokio::time::timeout(Duration::from_secs(10), refresh)
        .await
        .unwrap()
        .unwrap();
    let (imported, files) = editor.await.unwrap();
    server.await.unwrap();
    assert!(outcome.forced_refresh && outcome.error.is_some());
    let store = ProfileStore::snapshot().await.unwrap();
    assert_eq!(
        store.current_uid().as_deref(),
        Some(if more_edits { "B" } else { "next" })
    );
    let saved: Value = serde_yaml_ng::from_slice(&std::fs::read(home.path().join("profiles.yaml")).unwrap()).unwrap();
    assert_eq!(saved["future_root"]["keep"], Value::Bool(true));
    let a = saved["items"]
        .as_sequence()
        .unwrap()
        .iter()
        .find(|item| item["uid"] == "next")
        .unwrap();
    assert_eq!(a["name"], if more_edits { "A-later-name" } else { "A" });
    assert_eq!(a["future_item"]["keep"], Value::Bool(true));
    assert_eq!(a["option"]["future_option"], "kept");
    assert_eq!(
        a["option"]["trusted_hosts"],
        serde_yaml_ng::to_value(if more_edits {
            vec!["127.0.0.1", "later.example"]
        } else {
            vec!["127.0.0.1"]
        })
        .unwrap()
    );
    if let Some(item) = imported {
        assert!(store.items().iter().any(|saved| saved.uid == item.uid));
        for (file, bytes) in files {
            assert_eq!(
                std::fs::read(home.path().join("profiles").join(file.as_str())).unwrap(),
                bytes
            );
        }
    } else {
        assert_eq!(
            store
                .items()
                .iter()
                .find(|item| item.uid.as_deref() == Some("B"))
                .unwrap()
                .name
                .as_deref(),
            Some("after")
        );
    }
    let source = std::fs::read_to_string(home.path().join("profiles/next.yaml")).unwrap();
    match later {
        LaterEdit::Source => {
            assert_eq!(source, LATER);
            assert!(!outcome.rolled_back);
            assert_eq!(lifecycle.reloads.load(Ordering::SeqCst), 1);
            assert_eq!(a["extra"]["upload"], Value::from(200));
            assert_ne!(a["updated"], Value::from(1));
            assert_eq!(a["home"], "https://new.example");
        }
        LaterEdit::Quota | LaterEdit::BindingUrl | LaterEdit::BindingType => {
            assert_eq!(source, NEW);
            assert!(!outcome.rolled_back);
            assert_eq!(lifecycle.reloads.load(Ordering::SeqCst), 1);
            assert_eq!(
                a["extra"]["upload"],
                Value::from(if matches!(later, LaterEdit::Quota) { 999 } else { 200 })
            );
            assert_eq!(a["extra"]["download"], Value::from(300));
            if matches!(later, LaterEdit::Quota) {
                assert_eq!(a["extra"]["late_unknown"], "keep");
            } else if matches!(later, LaterEdit::BindingUrl) {
                assert_eq!(a["url"], "https://later.example/subscription");
            } else {
                assert_eq!(a["type"], "local");
            }
            assert_ne!(a["updated"], Value::from(1));
            assert_eq!(a["home"], "https://new.example");
        }
        LaterEdit::None | LaterEdit::FragmentReference | LaterEdit::FragmentChanges => {
            assert_eq!(source, OLD, "A owns the rejected source despite unrelated edits");
            assert_eq!(a["updated"], Value::from(1));
            assert_eq!(a["home"], "https://old.example");
            assert_eq!(a["extra"]["download"], Value::from(20));
            assert_eq!(a["extra"]["future_usage"], "kept");
            assert_eq!(a["extra"]["upload"], Value::from(10));
            assert!(outcome.rolled_back, "{:?}", outcome.error);
            assert!(a["option"]["merge"].is_null());
            if matches!(later, LaterEdit::FragmentReference) {
                let b = saved["items"]
                    .as_sequence()
                    .unwrap()
                    .iter()
                    .find(|item| item["uid"] == "B")
                    .unwrap();
                let fragment = store
                    .all_items()
                    .into_iter()
                    .find(|item| item.uid.as_deref() == b["option"]["merge"].as_str())
                    .unwrap();
                assert!(
                    home.path()
                        .join("profiles")
                        .join(fragment.file.unwrap().as_str())
                        .is_file()
                );
                assert_eq!(store.all_items().len(), 3, "only the shared generated fragment stays");
            } else if matches!(later, LaterEdit::FragmentChanges) {
                let merge = saved["items"]
                    .as_sequence()
                    .unwrap()
                    .iter()
                    .find(|item| item["type"] == "merge")
                    .unwrap();
                assert_eq!(merge["later_unknown"], "keep");
                assert!(
                    home.path()
                        .join("profiles")
                        .join(merge["file"].as_str().unwrap())
                        .is_file()
                );
                let script = saved["items"]
                    .as_sequence()
                    .unwrap()
                    .iter()
                    .find(|item| item["type"] == "script")
                    .unwrap();
                assert_eq!(
                    std::fs::read_to_string(home.path().join("profiles").join(script["file"].as_str().unwrap()))
                        .unwrap(),
                    "// later script edit\n"
                );
                assert_eq!(
                    store.all_items().len(),
                    4,
                    "only the two modified generated fragments stay"
                );
            } else {
                let expected_items = if import { 8 } else { 2 };
                assert_eq!(
                    store.all_items().len(),
                    expected_items,
                    "A's untouched generated fragments are removed"
                );
                assert_eq!(
                    std::fs::read_dir(home.path().join("profiles")).unwrap().count(),
                    expected_items,
                    "metadata and files remain in sync"
                );
            }
        }
    }
}
