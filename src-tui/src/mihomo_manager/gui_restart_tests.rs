//! Synthetic GUI coexistence regressions: production preparation, real kernel
//! socket ownership, and private validator/lifecycle seams. No proxy core runs.

use super::*;
use std::io::{BufRead, BufReader};
use std::os::unix::fs::PermissionsExt;

struct ListenerProcess(std::process::Child);

impl Drop for ListenerProcess {
    fn drop(&mut self) {
        let pid = self.0.id();
        let _ = self.0.kill();
        self.0.wait().expect("reap controlled listener subprocess");
        assert!(
            !pidfile::is_running(pid),
            "controlled subprocess is gone after fixture cleanup"
        );
        eprintln!("issue89 fixture cleanup reaped pid {pid}");
    }
}

struct Fixture {
    home: tempfile::TempDir,
    manager: MihomoManager,
    listener: ListenerProcess,
    mixed: std::net::SocketAddr,
    _guard: tokio::sync::MutexGuard<'static, ()>,
}

async fn fixture(owned: bool) -> Fixture {
    let (home, guard) = super::conversion_fixture().await;
    std::fs::set_permissions(home.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let runtime = home.path().join("runtime");
    std::fs::create_dir(&runtime).unwrap();
    std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o700)).unwrap();
    let socket = runtime.join("controller.sock");
    let script = home.path().join("listeners.py");
    std::fs::write(&script, r#"import ctypes, json, pathlib, socket, sys, time
root=pathlib.Path(sys.argv[-1]).parent
def pair():
 for _ in range(64):
  tcp=socket.socket(); udp=socket.socket(type=socket.SOCK_DGRAM)
  try:
   tcp.bind(('127.0.0.1',0)); tcp.listen(); udp.bind(tcp.getsockname())
   return tcp,udp
  except OSError:
   tcp.close(); udp.close()
 raise RuntimeError('cannot reserve a TCP/UDP loopback pair')
a,c=pair(); b,d=pair()
with open(sys.argv[-1],'w') as f: json.dump({'experimental':{'clash_api':{'external_controller':'127.0.0.1:'+str(b.getsockname()[1])}},'old':True},f)
print(a.getsockname()[1], b.getsockname()[1],flush=True)
for _ in range(12000):
 if (root/'nondump').exists():
  a.close(); b.close(); c.close(); d.close()
  assert ctypes.CDLL(None).prctl(4,0,0,0,0)==0
  (root/'nondump-ready').touch()
  time.sleep(120)
 time.sleep(0.01)
"#).unwrap();
    let child = std::process::Command::new("/bin/bash")
        .args(["-c", "exec -a sing-box-fixture /usr/bin/python3 \"$@\"", "fixture"])
        .arg(script)
        .args(["run", "-c"])
        .arg(home.path().join("old-core.json"))
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut listener = ListenerProcess(child);
    let mut ready = String::new();
    BufReader::new(listener.0.stdout.take().unwrap())
        .read_line(&mut ready)
        .unwrap();
    let ports: Vec<u16> = ready.split_whitespace().map(|port| port.parse().unwrap()).collect();
    assert_eq!(ports.len(), 2, "listener subprocess must announce both ready ports");
    let mixed = ([127, 0, 0, 1], ports[0]).into();
    let controller = ([127, 0, 0, 1], ports[1]).into();
    let pid = listener.0.id();
    eprintln!("issue89 private listener fixture pid {pid}");
    std::fs::copy(home.path().join("old-core.json"), home.path().join("singbox.json")).unwrap();
    std::fs::write(home.path().join("config.yaml"), format!("mixed-port: {}\nexternal-controller: {controller}\nexternal-controller-unix: {}\nsecret: set-your-secret\ntun: {{enable: false}}\nallow-lan: false\n", ports[0], socket.display())).unwrap();
    std::fs::write(
        home.path().join("verge.yaml"),
        format!(
            "proxy_core: singbox\nverge_mixed_port: {}\nenable_tun_mode: false\nenable_system_proxy: false\n",
            ports[0]
        ),
    )
    .unwrap();
    std::fs::write(
        home.path().join("profiles.yaml"),
        "current: base\nitems:\n  - {uid: base, type: local, file: base.yaml}\n",
    )
    .unwrap();
    std::fs::write(
        home.path().join("profiles/base.yaml"),
        "proxies: []\nrules: ['MATCH,DIRECT']\n",
    )
    .unwrap();
    let validator = home.path().join("validator");
    std::fs::write(&validator, format!("#!/bin/sh\nset -eu\nroot=$(dirname \"$0\")\nkill -0 {pid}\nprintf 'check\\n' >> \"$root/events\"\ncp \"$3\" \"$root/checked\"\n")).unwrap();
    std::fs::set_permissions(&validator, std::fs::Permissions::from_mode(0o700)).unwrap();
    pidfile::write(
        &pidfile::path_for(&socket),
        pidfile::CoreRecord::with_kind_and_exe(
            pid,
            Utc::now(),
            CoreKind::SingBox,
            Some(
                std::fs::read_link(format!("/proc/{pid}/exe"))
                    .unwrap()
                    .to_string_lossy()
                    .into_owned(),
            ),
        ),
    )
    .unwrap();
    let manager = MihomoManager::new(home.path().to_path_buf())
        .with_core_kind(CoreKind::SingBox)
        .with_socket(socket)
        .with_singbox_controller(controller);
    manager.adopt_running_core();
    assert_eq!(manager.pid(), Some(pid), "fixture goes through real validated adoption");
    manager.inner.owns_child.store(owned, Ordering::SeqCst);
    if owned {
        let parent = super::super::process::OwnedProcess::capture(std::process::id()).unwrap();
        *manager.inner.listener_process.lock() = Some(Arc::new(
            super::super::process::OwnedProcess::capture_child(pid, &parent).unwrap(),
        ));
    }
    *manager.inner.resolved_binary.lock() = Some(validator);
    *manager.inner.gui_running.lock() = Some(true);
    assert!(super::super::gui_isolation::owns_listener(pid, mixed, "tcp").unwrap());
    assert!(super::super::gui_isolation::owns_listener(pid, mixed, "udp").unwrap());
    Fixture {
        home,
        manager,
        listener,
        mixed,
        _guard: guard,
    }
}

fn arm_lifecycle(fixture: &Fixture) {
    let home = fixture.home.path().to_path_buf();
    let validator = fixture.manager.binary_path().unwrap();
    let stop_home = home.clone();
    let spawn_home = home;
    let pid = fixture.listener.0.id();
    let mixed = fixture.mixed;
    *fixture.manager.inner.restart_stages.lock() = Some(Arc::new(RestartStages {
        resolve: Box::new(move || {
            Ok(binary::ResolvedMihomo {
                path: validator.clone(),
                source: binary::MihomoBinarySource::System,
                version: "fixture".into(),
            })
        }),
        preflight: Box::new(|_, tun| {
            assert!(!tun);
            Ok(())
        }),
        stop: Box::new(move || {
            assert!(pidfile::is_running(pid));
            assert!(super::super::gui_isolation::owns_listener(pid, mixed, "tcp").unwrap());
            assert_eq!(std::fs::read_to_string(stop_home.join("events")).unwrap(), "check\n");
            use std::io::Write;
            writeln!(
                std::fs::OpenOptions::new()
                    .append(true)
                    .open(stop_home.join("events"))
                    .unwrap(),
                "stop"
            )
            .unwrap();
            Ok(())
        }),
        spawn: Box::new(move |path| {
            assert_eq!(
                std::fs::read(path).unwrap(),
                std::fs::read(spawn_home.join("checked")).unwrap()
            );
            use std::io::Write;
            writeln!(
                std::fs::OpenOptions::new()
                    .append(true)
                    .open(spawn_home.join("events"))
                    .unwrap(),
                "spawn"
            )
            .unwrap();
            Ok(())
        }),
    }));
}

fn assert_preserved(fixture: &Fixture, old_config: &[u8], old_json: &[u8]) {
    assert_eq!(
        std::fs::read(fixture.home.path().join("config.yaml")).unwrap(),
        old_config
    );
    assert_eq!(
        std::fs::read(fixture.home.path().join("singbox.json")).unwrap(),
        old_json
    );
    assert_eq!(fixture.manager.pid(), Some(fixture.listener.0.id()));
    assert_eq!(fixture.manager.state(), CoreState::Running);
    assert!(pidfile::is_running(fixture.listener.0.id()));
    assert!(!pidfile::stop_intent_path_for(fixture.manager.socket_path()).exists());
    assert!(!crate::runtime_config::singbox_conversion_path().unwrap().exists());
    assert!(
        !std::fs::read_to_string(fixture.home.path().join("events"))
            .unwrap_or_default()
            .contains("stop")
    );
}

#[tokio::test]
async fn issue89_owned_tui_ordinary_restart_prepares_before_stop_and_exact_spawn() {
    let fixture = fixture(true).await;
    arm_lifecycle(&fixture);
    fixture.manager.restart().await.unwrap();
    assert_eq!(
        std::fs::read_to_string(fixture.home.path().join("events")).unwrap(),
        "check\nstop\nspawn\n"
    );
}

#[tokio::test]
async fn issue89_owned_tun_off_uses_same_full_candidate_owner_context() {
    let fixture = fixture(true).await;
    arm_lifecycle(&fixture);
    let io = crate::runtime_config::RUNTIME_CONFIG_IO.lock().await;
    fixture.manager.restart_tun_unlocked(&io, false).await.unwrap();
    assert_eq!(
        std::fs::read_to_string(fixture.home.path().join("events")).unwrap(),
        "check\nstop\nspawn\n"
    );
}

#[tokio::test]
async fn issue89_foreign_tcp_or_udp_refuses_before_install_or_stop() {
    for udp in [false, true] {
        let fixture = fixture(true).await;
        arm_lifecycle(&fixture);
        let foreign_udp = udp.then(|| std::net::UdpSocket::bind("127.0.0.1:0").unwrap());
        let foreign_tcp = (!udp).then(|| std::net::TcpListener::bind("127.0.0.1:0").unwrap());
        let port = foreign_udp.as_ref().map_or_else(
            || foreign_tcp.as_ref().unwrap().local_addr().unwrap().port(),
            |socket| socket.local_addr().unwrap().port(),
        );
        let config = std::fs::read_to_string(fixture.home.path().join("config.yaml"))
            .unwrap()
            .replace(
                &format!("mixed-port: {}", fixture.mixed.port()),
                &format!("mixed-port: {port}"),
            );
        std::fs::write(fixture.home.path().join("config.yaml"), config).unwrap();
        let verge = std::fs::read_to_string(fixture.home.path().join("verge.yaml"))
            .unwrap()
            .replace(
                &format!("verge_mixed_port: {}", fixture.mixed.port()),
                &format!("verge_mixed_port: {port}"),
            );
        std::fs::write(fixture.home.path().join("verge.yaml"), verge).unwrap();
        let old_config = std::fs::read(fixture.home.path().join("config.yaml")).unwrap();
        let old_json = std::fs::read(fixture.home.path().join("singbox.json")).unwrap();
        let error = fixture.manager.restart().await.unwrap_err();
        assert!(
            format!("{error:#}").contains(if udp {
                "UDP listener belongs to another process"
            } else {
                "TCP listener belongs to another process"
            }),
            "{error:#}"
        );
        assert_preserved(&fixture, &old_config, &old_json);
        drop((foreign_udp, foreign_tcp));
    }
}

#[tokio::test]
async fn issue89_replaced_record_during_validator_refuses_before_install_or_stop() {
    for owned in [false, true] {
        let fixture = fixture(owned).await;
        arm_lifecycle(&fixture);
        let old_config = std::fs::read(fixture.home.path().join("config.yaml")).unwrap();
        let old_json = std::fs::read(fixture.home.path().join("singbox.json")).unwrap();
        let mut record = pidfile::read_record(&pidfile::path_for(fixture.manager.socket_path())).unwrap();
        record.exe = Some("/unrelated/replaced-executable".into());
        pidfile::write(&fixture.home.path().join("replacement-record"), record).unwrap();
        let validator = fixture.manager.binary_path().unwrap();
        let mut script = std::fs::read_to_string(&validator).unwrap();
        script.push_str("cp \"$root/replacement-record\" \"$root/runtime/mihomo.pid\"\n");
        std::fs::write(validator, script).unwrap();
        let error = fixture
            .manager
            .prepare_mihomo_launch(crate::commands::start::SupervisorLaunch::Regenerate)
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains("identity/ownership changed"), "{error:#}");
        assert_preserved(&fixture, &old_config, &old_json);
    }
}

#[tokio::test]
async fn issue89_invalid_bound_process_identity_cannot_exempt_a_matching_bare_pid() {
    let fixture = fixture(false).await;
    let old_config = std::fs::read(fixture.home.path().join("config.yaml")).unwrap();
    let old_json = std::fs::read(fixture.home.path().join("singbox.json")).unwrap();
    *fixture.manager.inner.listener_process.lock() = Some(Arc::new(
        super::super::process::OwnedProcess::capture(std::process::id()).unwrap(),
    ));
    assert!(
        fixture
            .manager
            .prepare_mihomo_launch(crate::commands::start::SupervisorLaunch::Regenerate)
            .await
            .unwrap_err()
            .to_string()
            .contains("validated process generation")
    );
    assert_preserved(&fixture, &old_config, &old_json);
    assert!(!fixture.home.path().join("checked").exists());
}

#[tokio::test]
async fn issue89_manager_generation_change_during_validator_refuses_before_install() {
    let fixture = fixture(false).await;
    let old_config = std::fs::read(fixture.home.path().join("config.yaml")).unwrap();
    let old_json = std::fs::read(fixture.home.path().join("singbox.json")).unwrap();
    let validator = fixture.manager.binary_path().unwrap();
    let mut script = std::fs::read_to_string(&validator).unwrap();
    script.push_str("while [ ! -f \"$root/release\" ]; do sleep 0.01; done\n");
    std::fs::write(validator, script).unwrap();
    let prepare = fixture
        .manager
        .prepare_mihomo_launch(crate::commands::start::SupervisorLaunch::Regenerate);
    let change = async {
        tokio::time::timeout(Duration::from_secs(2), async {
            while !fixture.home.path().join("checked").exists() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        fixture.manager.inner.generation.fetch_add(1, Ordering::SeqCst);
        std::fs::write(fixture.home.path().join("release"), "").unwrap();
    };
    let (result, ()) = tokio::join!(prepare, change);
    assert!(result.unwrap_err().to_string().contains("ownership/generation changed"));
    assert_preserved(&fixture, &old_config, &old_json);
}

#[tokio::test]
async fn issue89_cold_prepare_and_use_existing_keep_their_exact_semantics() {
    let mut fixture = fixture(false).await;
    fixture.listener.0.kill().unwrap();
    fixture.listener.0.wait().unwrap();
    *fixture.manager.inner.pid.lock() = None;
    *fixture.manager.inner.listener_process.lock() = None;
    *fixture.manager.inner.state.lock() = CoreState::Stopped;
    let validator = fixture.manager.binary_path().unwrap();
    std::fs::write(&validator, "#!/bin/sh\nexit 0\n").unwrap();
    fixture
        .manager
        .prepare_mihomo_launch(crate::commands::start::SupervisorLaunch::Regenerate)
        .await
        .unwrap();
    let config = std::fs::read(fixture.home.path().join("config.yaml")).unwrap();
    let json = std::fs::read(fixture.home.path().join("singbox.json")).unwrap();
    let report = std::fs::read(crate::runtime_config::singbox_conversion_path().unwrap()).ok();
    std::fs::write(fixture.home.path().join("profiles/base.yaml"), "invalid: [").unwrap();
    fixture
        .manager
        .prepare_mihomo_launch(crate::commands::start::SupervisorLaunch::UseExistingConfig)
        .await
        .unwrap();
    assert_eq!(std::fs::read(fixture.home.path().join("config.yaml")).unwrap(), config);
    assert_eq!(std::fs::read(fixture.home.path().join("singbox.json")).unwrap(), json);
    assert_eq!(
        std::fs::read(crate::runtime_config::singbox_conversion_path().unwrap()).ok(),
        report
    );
}

#[tokio::test]
async fn issue89_spawn_bound_nondumpable_owned_process_keeps_free_port_preparation() {
    use std::os::unix::fs::MetadataExt;
    if std::fs::metadata("/proc/self").unwrap().uid() == 0 {
        eprintln!("SKIP: nondumpable executable visibility requires a non-root UID");
        return;
    }
    let fixture = fixture(true).await;
    std::fs::write(fixture.home.path().join("nondump"), "").unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while !fixture.home.path().join("nondump-ready").exists() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    assert!(
        std::fs::read_link(format!("/proc/{}/exe", fixture.listener.0.id())).is_err(),
        "fixture must hide executable without setcap"
    );
    assert!(
        fixture
            .manager
            .inner
            .listener_process
            .lock()
            .as_ref()
            .unwrap()
            .matches_process()
    );
    fixture
        .manager
        .prepare_mihomo_launch(crate::commands::start::SupervisorLaunch::Regenerate)
        .await
        .unwrap();
    assert!(pidfile::is_running(fixture.listener.0.id()));
}

#[tokio::test]
async fn issue89_listener_exemption_revalidates_after_kernel_inode_probe() {
    let fixture = fixture(false).await;
    let calls = std::cell::Cell::new(0);
    let pid = fixture.listener.0.id();
    let owner = || {
        calls.set(calls.get() + 1);
        Ok(if calls.get() < 3 { Some(pid) } else { None })
    };
    let error =
        super::super::gui_isolation::check_with_owner(fixture.home.path(), fixture.manager.socket_path(), owner, None)
            .unwrap_err();
    assert!(error.to_string().contains("ownership changed during verification"));
    assert_eq!(calls.get(), 3, "entry, before inode probe, after inode probe");
    assert!(pidfile::is_running(pid));
}

#[tokio::test]
async fn issue89_current_watcher_releases_listener_process_handle_on_exit() {
    let home = tempfile::tempdir().unwrap();
    let inner = Arc::new(ManagerInner::new());
    let child = tokio::process::Command::new("/bin/sleep").arg("0.1").spawn().unwrap();
    let pid = child.id().unwrap();
    let process = Arc::new(super::super::process::OwnedProcess::spawned(&child).unwrap());
    let released = Arc::downgrade(&process);
    *inner.listener_process.lock() = Some(process);
    *inner.pid.lock() = Some(pid);
    inner.expected_exit.store(true, Ordering::SeqCst);
    spawn_watcher(
        child,
        Arc::clone(&inner),
        home.path(),
        &home.path().join("controller.sock"),
        0,
    )
    .await
    .unwrap();
    assert!(inner.listener_process.lock().is_none());
    assert!(
        released.upgrade().is_none(),
        "last cached process handle and pidfd must be released"
    );
    assert!(inner.pid.lock().is_none());
    assert!(!pidfile::is_running(pid));
}

#[tokio::test]
async fn issue89_adopted_cli_regenerate_accepts_old_actual_listeners_before_stop() {
    let fixture = fixture(false).await;
    let mode = fixture
        .manager
        .prepare_mihomo_launch(crate::commands::start::SupervisorLaunch::Regenerate)
        .await;
    assert!(
        mode.is_ok(),
        "validated predecessor listeners must pass production preparation: {mode:?}"
    );
    assert_eq!(
        mode.unwrap(),
        crate::commands::start::SupervisorLaunch::UseExistingConfig
    );
    assert_eq!(fixture.manager.pid(), Some(fixture.listener.0.id()));
    assert!(pidfile::is_running(fixture.listener.0.id()));
    assert!(super::super::gui_isolation::owns_listener(fixture.listener.0.id(), fixture.mixed, "tcp").unwrap());
    assert_eq!(
        std::fs::read_to_string(fixture.home.path().join("events")).unwrap(),
        "check\n"
    );
    assert_eq!(
        std::fs::read(fixture.home.path().join("singbox.json")).unwrap(),
        std::fs::read(fixture.home.path().join("checked")).unwrap()
    );
}
