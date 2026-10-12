//! Production profile intent → service → validated transaction → restart
//! stages → background delivery → reducer. Only process effects are replaced.
use super::*;
use crate::app::CoreState;
use crate::mihomo_manager::manager::{RestartStages, rollback_failed_spawn};
use crate::mihomo_manager::{CoreKind, binary};
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

struct Children(parking_lot::Mutex<Vec<std::process::Child>>);
impl Children {
    fn spawn(&self) -> u32 {
        let child = std::process::Command::new("/bin/sleep").arg("30").spawn().unwrap();
        let pid = child.id();
        self.0.lock().push(child);
        pid
    }
    fn stop(&self) {
        for mut child in self.0.lock().drain(..) {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}
impl Drop for Children {
    fn drop(&mut self) {
        self.stop();
    }
}

async fn receive(rx: &mut mpsc::Receiver<Action>) -> Action {
    tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
        .await
        .expect("bounded profile apply")
        .unwrap()
}

fn seed(home: &Path) {
    std::fs::write(
        home.join("verge.yaml"),
        "enable_tun_mode: false\nenable_system_proxy: false\nverge_mixed_port: 35123\n",
    )
    .unwrap();
    std::fs::write(
        home.join("config.yaml"),
        "mixed-port: 35123\nexternal-controller: 127.0.0.1:49715\nsecret: fixture\n",
    )
    .unwrap();
    std::fs::write(home.join("profiles.yaml"), "current: old\nitems:\n  - {uid: old, type: local, file: old.yaml}\n  - {uid: next, type: local, file: next.yaml}\n").unwrap();
    std::fs::write(home.join("profiles/old.yaml"), "proxies: []\nrules: ['MATCH,DIRECT']\n").unwrap();
    std::fs::write(
        home.join("profiles/next.yaml"),
        "proxies: []\nrules: ['DOMAIN,next.issue88.test,DIRECT', 'MATCH,DIRECT']\n",
    )
    .unwrap();
    // The previous service file must be valid enough for the recovery's
    // UseExistingConfig preparation, but distinct from the full candidate.
    std::fs::write(
        home.join("singbox.json"),
        "{\"outbounds\":[{\"type\":\"direct\",\"tag\":\"DIRECT\"}]}\n",
    )
    .unwrap();
}

fn ctx_for(manager: MihomoManager) -> (Ctx, mpsc::Receiver<Action>) {
    let (sender, rx) = mpsc::channel(256);
    manager.set_action_tx(sender.clone());
    let (mut ctx, _) = tests::ctx();
    ctx.tx = crate::tui::background::EventSender::new(sender).with_manager(manager.inner());
    ctx.manager = manager;
    (ctx, rx)
}

async fn running_app(ctx: &Ctx, old_pid: u32) -> App {
    let mut app = App::new();
    app.core_state = CoreState::Running;
    app.core_pid = Some(old_pid);
    app.view = View::Profiles;
    app.focus = Focus::Content;
    app.gui_config.enable_system_proxy = Some(false);
    app.gui_config.enable_tun_mode = Some(false);
    app.load_profiles(&crate::profile_store::store::ProfileStore::snapshot().await.unwrap());
    app.selected_index = 1;
    app.proxy_groups = serde_json::from_str(r#"{"old":{"type":"Selector","now":"DIRECT","all":["DIRECT"]}}"#).unwrap();
    app.rules = serde_json::from_str(r#"[{"type":"Domain","payload":"old.test","proxy":"DIRECT"}]"#).unwrap();
    app.connections = serde_json::from_str(r#"[{"id":"old","upload":1,"download":2,"start":"old"}]"#).unwrap();
    app.traffic = Some(TrafficData { up: 12, down: 34 });
    app.traffic_totals = Some((100, 200));
    app.delay_map.insert("old".into(), Some(20));
    let stream = ctx.tx.spawn(std::future::pending::<()>());
    app.traffic_stream = Some(stream.abort_handle());
    app.log_stream = Some(stream.abort_handle());
    ctx.traffic_tx.send_replace(Some((
        ctx.tx.current_generation(),
        ctx.manager.current_generation(),
        TrafficData { up: 12, down: 34 },
    )));
    app
}

struct Fixture {
    ctx: Ctx,
    rx: mpsc::Receiver<Action>,
    children: Arc<Children>,
    attempts: Arc<AtomicUsize>,
    stops: Arc<AtomicUsize>,
    old_pid: u32,
    recover: Arc<AtomicBool>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.ctx.cancel_background();
        self.children.stop();
    }
}

fn fixture(home: &Path, recovery: bool, reject: bool) -> Fixture {
    seed(home);
    let validator = home.join("validator");
    std::fs::write(&validator, format!("#!/bin/sh\nset -eu\nroot=$(dirname \"$0\")\n[ \"$1\" = check ]\n[ \"$2\" = -c ]\ncmp \"$root/singbox.json\" \"$root/previous\"\ncp \"$3\" \"$root/checked\"\n{}\n", if reject { "exit 1" } else { "exit 0" })).unwrap();
    std::fs::set_permissions(&validator, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::copy(home.join("singbox.json"), home.join("previous")).unwrap();
    let manager = MihomoManager::new(home.into()).with_core_kind(CoreKind::SingBox);
    let children = Arc::new(Children(parking_lot::Mutex::new(Vec::new())));
    let old_pid = children.spawn();
    let inner = manager.inner();
    *inner.pid.lock() = Some(old_pid);
    *inner.state.lock() = CoreState::Running;
    *inner.resolved_binary.lock() = Some(validator.clone());
    inner.owns_child.store(true, Ordering::SeqCst);
    inner.generation.store(10, Ordering::SeqCst);
    let (ctx, rx) = ctx_for(manager);
    let attempts = Arc::new(AtomicUsize::new(0));
    let recover = Arc::new(AtomicBool::new(recovery));
    let recovery = recover.clone();
    let stops = Arc::new(AtomicUsize::new(0));
    let stop_inner = Arc::downgrade(&inner);
    let stop_children = children.clone();
    let stop_count = stops.clone();
    let spawn_inner = Arc::downgrade(&inner);
    let spawn_children = children.clone();
    let count = attempts.clone();
    let root = home.to_path_buf();
    let socket = ctx.manager.socket_path().to_path_buf();
    inner.set_restart_stages_for_test(RestartStages {
        resolve: Box::new(move || {
            Ok(binary::ResolvedMihomo {
                path: validator.clone(),
                source: binary::MihomoBinarySource::System,
                version: "fixture".into(),
            })
        }),
        preflight: Box::new(|_, flag| {
            assert!(!flag);
            Ok(())
        }),
        stop: Box::new(move || {
            let stop_inner = stop_inner.upgrade().unwrap();
            assert!(
                stop_inner.restarting.load(Ordering::SeqCst),
                "predecessor exit is suppressed during replacement"
            );
            stop_inner
                .expected_exit_gen
                .store(stop_inner.generation.load(Ordering::SeqCst), Ordering::SeqCst);
            stop_children.stop();
            *stop_inner.pid.lock() = None;
            *stop_inner.state.lock() = CoreState::Stopped;
            stop_count.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }),
        spawn: Box::new(move |path| {
            let spawn_inner = spawn_inner.upgrade().unwrap();
            assert_eq!(path, root.join("singbox.json"));
            let attempt = count.fetch_add(1, Ordering::SeqCst);
            assert_eq!(
                std::fs::read(path).unwrap(),
                std::fs::read(root.join(if attempt == 0 { "checked" } else { "previous" })).unwrap(),
                "each launch uses the transaction's exact file"
            );
            let pid = spawn_children.spawn();
            *spawn_inner.pid.lock() = Some(pid);
            *spawn_inner.state.lock() = CoreState::Running;
            *spawn_inner.started_at.lock() = Some(chrono::Utc::now());
            spawn_inner.owns_child.store(true, Ordering::SeqCst);
            spawn_inner.generation.fetch_add(1, Ordering::SeqCst);
            spawn_inner.expected_exit_gen.store(u64::MAX, Ordering::SeqCst);
            if attempt == 0 || !recovery.load(Ordering::SeqCst) {
                spawn_children.stop();
                rollback_failed_spawn(&spawn_inner, &socket, pid, "fixture readiness failed".into());
                anyhow::bail!("fixture readiness failed");
            }
            Ok(())
        }),
    });
    Fixture {
        ctx,
        rx,
        children,
        attempts,
        stops,
        old_pid,
        recover,
    }
}

#[tokio::test]
async fn issue88_owned_profile_double_failure_clears_terminal_state_for_all_entrypoints() {
    for entry in [0, 1, 2] {
        let home = tempfile::tempdir().unwrap();
        let _home = crate::profile_store::store::tests::claim_test_app_home(home.path().into()).await;
        let mut fixture = fixture(home.path(), false, false);
        let mut app = running_app(&fixture.ctx, fixture.old_pid).await;
        app.rules_edit_buffer = vec![crate::routing::from_clash_rule_str("DOMAIN,draft.issue88.test,REJECT")];
        app.rules_edit_dirty = true;
        app.rules_edit_mode = true;
        let draft = app.rules_edit_buffer.clone();
        let background_generation = fixture.ctx.tx.current_generation();
        let stream = app.traffic_stream.clone().unwrap();
        match entry {
            0 => {
                handle_intent(&mut app, &fixture.ctx, Action::Activate).await;
            }
            1 => {
                handle_event(&mut app, &fixture.ctx, Action::ProfileImported).await;
            }
            _ => {
                handle_event(
                    &mut app,
                    &fixture.ctx,
                    Action::ProfileUpdated {
                        uid: "next".into(),
                        is_current: true,
                    },
                )
                .await;
            }
        }
        let event = receive(&mut fixture.rx).await;
        assert_eq!(fixture.attempts.load(Ordering::SeqCst), 2);
        assert_eq!(fixture.stops.load(Ordering::SeqCst), 2);
        assert!(fixture.ctx.manager.pid().is_none());
        assert!(!fixture.ctx.manager.owns_child());
        assert_eq!(fixture.ctx.manager.current_generation(), 12);
        assert!(
            std::fs::read_to_string(home.path().join("checked"))
                .unwrap()
                .contains("next.issue88.test"),
            "full candidate consumes the selected profile before restart"
        );
        handle_event(&mut app, &fixture.ctx, event).await;
        assert!(
            matches!(app.core_state, CoreState::Error(_)),
            "manager is dead, TUI must not retain Running: {:?}",
            app.core_state
        );
        assert!(app.core_pid.is_none());
        assert!(app.proxy_groups.is_empty() && app.rules.is_empty() && app.connections.is_empty());
        assert_eq!(app.rules_edit_buffer, draft);
        assert!(
            app.rules_edit_dirty && app.rules_edit_mode,
            "terminal snapshots are independent of unsaved edits"
        );
        assert!(app.traffic.is_none() && app.traffic_totals.is_none() && app.delay_map.is_empty());
        assert!(app.log_stream.is_none() && app.traffic_stream.is_none());
        assert!(fixture.ctx.traffic_tx.borrow().is_none());
        assert!(fixture.ctx.tx.current_generation() > background_generation);
        tokio::task::yield_now().await;
        assert!(stream.is_finished());
        assert!(app.status_msg.as_deref().unwrap().contains("fallback restart failed"));
        assert_eq!(
            std::fs::read(home.path().join("singbox.json")).unwrap(),
            std::fs::read(home.path().join("previous")).unwrap()
        );
        assert!(fixture.children.0.lock().is_empty());
    }
}

#[tokio::test]
async fn issue88_precheck_rejection_preserves_live_owned_state_and_snapshots() {
    let home = tempfile::tempdir().unwrap();
    let _home = crate::profile_store::store::tests::claim_test_app_home(home.path().into()).await;
    let mut fixture = fixture(home.path(), false, true);
    let mut app = running_app(&fixture.ctx, fixture.old_pid).await;
    handle_intent(&mut app, &fixture.ctx, Action::Activate).await;
    handle_event(&mut app, &fixture.ctx, receive(&mut fixture.rx).await).await;
    assert_eq!(fixture.attempts.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.stops.load(Ordering::SeqCst), 0);
    assert_eq!(app.core_state, CoreState::Running);
    assert_eq!(app.core_pid, Some(fixture.old_pid));
    assert_eq!(fixture.ctx.manager.pid(), Some(fixture.old_pid));
    assert_eq!(fixture.ctx.manager.current_generation(), 10);
    assert_eq!(app.proxy_groups.len(), 1);
    assert_eq!(app.rules.len(), 1);
    assert_eq!(app.connections.len(), 1);
    assert!(app.traffic.is_some() && app.log_stream.is_some());
    assert_eq!(app.current_profile_uid.as_deref(), Some("old"));
    assert!(app.status_msg.as_deref().unwrap().contains("rejected"));
    fixture.ctx.cancel_background();
}

#[tokio::test]
async fn issue88_queued_terminal_error_cannot_undo_later_recovery_generation() {
    let home = tempfile::tempdir().unwrap();
    let _home = crate::profile_store::store::tests::claim_test_app_home(home.path().into()).await;
    let mut fixture = fixture(home.path(), false, false);
    let mut app = running_app(&fixture.ctx, fixture.old_pid).await;
    handle_intent(&mut app, &fixture.ctx, Action::Activate).await;
    let terminal = receive(&mut fixture.rx).await;
    assert_eq!(fixture.ctx.manager.current_generation(), 12);
    assert!(fixture.ctx.manager.pid().is_none());
    // A subsequent restart succeeds before the previous error is consumed.
    fixture.recover.store(true, Ordering::SeqCst);
    fixture
        .ctx
        .manager
        .restart_with(crate::commands::start::SupervisorLaunch::UseExistingConfig)
        .await
        .unwrap();
    assert_eq!(fixture.ctx.manager.current_generation(), 13);
    handle_event(&mut app, &fixture.ctx, receive(&mut fixture.rx).await).await;
    let recovered_pid = fixture.ctx.manager.pid().unwrap();
    assert_eq!(app.core_pid, Some(recovered_pid));
    let generation = fixture.ctx.tx.current_generation();
    handle_event(&mut app, &fixture.ctx, terminal).await;
    assert_eq!(app.core_state, CoreState::Running);
    assert_eq!(app.core_pid, Some(recovered_pid));
    assert_eq!(fixture.ctx.tx.current_generation(), generation);
    fixture.ctx.cancel_background();
}

#[test]
fn issue88_live_pid_excludes_terminal_classification_even_with_stale_error_state() {
    let manager = MihomoManager::new(std::env::temp_dir());
    *manager.inner().state.lock() = CoreState::Error("older operation".into());
    *manager.inner().pid.lock() = Some(42);
    assert!(matches!(
        lifecycle::apply_error(&manager, true, "apply rejected".into()),
        Action::OperationError(_)
    ));
}

#[tokio::test]
async fn issue88_mihomo_rejection_and_uncertainty_preserve_owned_and_untracked_attached_health() {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    for tracked in [true, false] {
        for reject in [true, false] {
            let home = tempfile::tempdir().unwrap();
            let _home = crate::profile_store::store::tests::claim_test_app_home(home.path().into()).await;
            seed(home.path());
            let socket = home.path().join("controller.sock");
            let listener = tokio::net::UnixListener::bind(&socket).unwrap();
            let puts = Arc::new(AtomicUsize::new(0));
            let counter = puts.clone();
            let server = tokio::spawn(async move {
                loop {
                    let (mut stream, _) = listener.accept().await.unwrap();
                    let mut request = Vec::new();
                    let mut buffer = [0; 4096];
                    loop {
                        let size = stream.read(&mut buffer).await.unwrap();
                        if size == 0 {
                            break;
                        }
                        request.extend_from_slice(&buffer[..size]);
                        if let Some(end) = request.windows(4).position(|b| b == b"\r\n\r\n") {
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
                    let put = request.starts_with(b"PUT /configs?force=true ");
                    let response = if put {
                        counter.fetch_add(1, Ordering::SeqCst);
                        if reject {
                            "HTTP/1.1 400 Bad Request\r\nContent-Length: 3\r\nConnection: close\r\n\r\nbad"
                        } else {
                            // An accepted but truncated response is uncertain
                            // immediately; subsequent health still succeeds.
                            "HTTP/1.1 200 OK\r\nContent-Length: 100\r\nConnection: close\r\n\r\nx"
                        }
                    } else {
                        assert!(request.starts_with(b"GET /version "));
                        "HTTP/1.1 200 OK\r\nContent-Length: 22\r\nConnection: close\r\n\r\n{\"version\":\"v1.19.32\"}"
                    };
                    stream.write_all(response.as_bytes()).await.unwrap();
                }
            });
            let manager = MihomoManager::new(home.path().into())
                .with_socket(socket)
                .with_secret("fixture".into());
            let children = Children(parking_lot::Mutex::new(Vec::new()));
            let pid = children.spawn();
            if tracked {
                *manager.inner().pid.lock() = Some(pid);
                *manager.inner().state.lock() = CoreState::Running;
                manager.inner().owns_child.store(true, Ordering::SeqCst);
            }
            // An untracked attachment intentionally leaves manager's default
            // Stopped, while the TUI has a healthy external controller.
            let (ctx, mut rx) = ctx_for(manager);
            let mut app = running_app(&ctx, pid).await;
            if !tracked {
                app.core_pid = None;
            }
            let generation = ctx.tx.current_generation();
            handle_intent(&mut app, &ctx, Action::Activate).await;
            handle_event(&mut app, &ctx, receive(&mut rx).await).await;
            assert_eq!(app.core_state, CoreState::Running);
            assert_eq!(app.core_pid, tracked.then_some(pid));
            assert_eq!(ctx.manager.pid(), tracked.then_some(pid));
            assert_eq!(ctx.tx.current_generation(), generation);
            assert_eq!(app.proxy_groups.len(), 1);
            assert_eq!(app.rules.len(), 1);
            assert_eq!(app.connections.len(), 1);
            assert!(app.traffic.is_some() && app.log_stream.is_some());
            assert_eq!(puts.load(Ordering::SeqCst), 1, "no rollback PUT for uncertainty");
            assert_eq!(
                app.current_profile_uid.as_deref(),
                Some(if reject { "old" } else { "next" })
            );
            assert!(app.status_msg.as_deref().unwrap().contains(if reject {
                "rejected"
            } else {
                "candidate retained"
            }));
            assert_eq!(ctx.manager.api().version().await.unwrap().version, "v1.19.32");
            ctx.cancel_background();
            server.abort();
            let _ = server.await;
        }
    }
}

#[tokio::test]
async fn issue88_successful_fallback_reports_new_pid_without_false_terminal_event() {
    let home = tempfile::tempdir().unwrap();
    let _home = crate::profile_store::store::tests::claim_test_app_home(home.path().into()).await;
    let mut fixture = fixture(home.path(), true, false);
    let mut app = running_app(&fixture.ctx, fixture.old_pid).await;
    handle_intent(&mut app, &fixture.ctx, Action::Activate).await;
    // Successful fallback publishes ready before returning the apply error.
    handle_event(&mut app, &fixture.ctx, receive(&mut fixture.rx).await).await;
    let recovered_pid = fixture.ctx.manager.pid().unwrap();
    assert_ne!(recovered_pid, fixture.old_pid);
    assert_eq!(app.core_state, CoreState::Running);
    assert_eq!(app.core_pid, Some(recovered_pid));
    handle_event(&mut app, &fixture.ctx, receive(&mut fixture.rx).await).await;
    assert_eq!(app.core_state, CoreState::Running);
    assert_eq!(app.core_pid, Some(recovered_pid));
    assert!(app.status_msg.as_deref().unwrap().contains("profile reload"));
    assert_eq!(app.current_profile_uid.as_deref(), Some("old"));
    handle_event(
        &mut app,
        &fixture.ctx,
        Action::CoreGeneration {
            generation: 10,
            action: Box::new(Action::CoreExited(0)),
        },
    )
    .await;
    assert_eq!(app.core_state, CoreState::Running);
    assert_eq!(app.core_pid, Some(recovered_pid));
    fixture.ctx.cancel_background();
}
