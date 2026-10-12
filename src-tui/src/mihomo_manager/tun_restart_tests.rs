use super::*;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;

fn event(home: &Path, label: &str) {
    writeln!(
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(home.join("events"))
            .unwrap(),
        "{label}"
    )
    .unwrap();
}

fn events(home: &Path) -> String {
    std::fs::read_to_string(home.join("events")).unwrap_or_default()
}

fn seed(home: &Path, kind: CoreKind, enabled: bool, reject: bool, cap_reject: bool) -> MihomoManager {
    // The explicit service argument must beat the OPPOSITE persisted value.
    std::fs::write(home.join("verge.yaml"), format!("proxy_core: {}\nverge_mixed_port: 35123\nenable_tun_mode: {}\nenable_system_proxy: false\ncontroller_allowed_origins: ['https://dash.example']\n", kind.as_str(), !enabled)).unwrap();
    std::fs::write(home.join("profiles.yaml"), "current: base\nitems:\n  - {uid: base, type: remote, file: base.yaml, option: {merge: custom, script: hook}}\n  - {uid: custom, type: merge, file: merge.yaml}\n  - {uid: hook, type: script, file: hook.js}\n").unwrap();
    std::fs::write(home.join("profiles/base.yaml"), "proxies: []\nrules: ['DOMAIN,profile.example,DIRECT', 'DOMAIN-REGEX,ads\\..*,REJECT', 'MATCH,DIRECT']\ndns: {enable: true, nameserver: [1.1.1.1]}\n").unwrap();
    std::fs::write(home.join("profiles/merge.yaml"), "hosts: {merge.example: 127.0.0.2}\n").unwrap();
    std::fs::write(home.join("profiles/hook.js"), "function main(config) { config.rules.unshift('DOMAIN,script.example,REJECT'); config['mixed-port'] = 1; return config; }\n").unwrap();
    let source =
        crate::routing::load_profile_rules(&std::fs::read_to_string(home.join("profiles/base.yaml")).unwrap()).unwrap();
    let mut edited = crate::routing::load_profile_rules("rules: ['DOMAIN,overlay.example,REJECT']").unwrap();
    edited.extend(source.clone());
    let overlay = crate::rule_overlay::RuleOverlay::from_edit(&source, &edited).unwrap();
    let item = clash_verge_core::config::PrfItem {
        uid: Some("base".into()),
        file: Some("base.yaml".into()),
        ..Default::default()
    };
    let overlay_path = crate::rule_overlay::path(&home.join("profiles"), &item).unwrap();
    std::fs::write(overlay_path, overlay.serialize().unwrap()).unwrap();
    std::fs::write(home.join("config.yaml"), "# exact previous bytes\nmixed-port: 35123\nexternal-controller: 127.0.0.1:49715\nsecret: set-your-secret\ntun: {enable: false}\n").unwrap();
    std::fs::write(home.join("singbox.json"), "{\"old\":true}\n").unwrap();
    let formal = home.join(if kind == CoreKind::Mihomo {
        "config.yaml"
    } else {
        "singbox.json"
    });
    std::fs::copy(&formal, home.join("previous")).unwrap();
    let validator = home.join("validator");
    // Only a private validator stub is spawned. It checks that validation
    // precedes publication and changes the script source after checking.
    let program = format!(
        "#!/bin/sh\nset -eu\nroot=$(dirname \"$0\")\nprintf '%s\\n' \"$*\" > \"$root/args\"\ncandidate=\nwhile [ \"$#\" -gt 0 ]; do\n case \"$1\" in -f|-c) candidate=$2; shift;; esac\n shift\ndone\ncmp \"$root/{}\" \"$root/previous\"\nprintf 'check\\n' >> \"$root/events\"\ncp \"$candidate\" \"$root/checked\"\n{}\nprintf '%s\\n' \"function main(config) {{ config.rules.unshift('DOMAIN,changed-after-check.example,DIRECT'); return config; }}\" > \"$root/profiles/hook.js\"\n",
        formal.file_name().unwrap().to_str().unwrap(),
        if reject { "exit 1" } else { ":" }
    );
    std::fs::write(&validator, program).unwrap();
    std::fs::set_permissions(&validator, std::fs::Permissions::from_mode(0o700)).unwrap();
    let manager = MihomoManager::new(home.to_path_buf()).with_core_kind(kind);
    *manager.inner.state.lock() = CoreState::Running;
    *manager.inner.pid.lock() = Some(u32::MAX);
    let resolve_home = home.to_path_buf();
    let preflight_home = home.to_path_buf();
    let stop_home = home.to_path_buf();
    let spawn_home = home.to_path_buf();
    *manager.inner.restart_stages.lock() = Some(Arc::new(RestartStages {
        resolve: Box::new(move || {
            event(&resolve_home, "resolve");
            Ok(binary::ResolvedMihomo {
                path: validator.clone(),
                source: binary::MihomoBinarySource::System,
                version: "fixture".into(),
            })
        }),
        preflight: Box::new(move |_, flag| {
            assert_eq!(flag, enabled);
            event(&preflight_home, "preflight");
            anyhow::ensure!(!cap_reject, "injected capability rejection");
            Ok(())
        }),
        stop: Box::new(move || {
            assert!(
                crate::runtime_config::RUNTIME_CONFIG_IO.try_lock().is_err(),
                "no write/restart race window"
            );
            assert_eq!(events(&stop_home), "resolve\npreflight\ncheck\n");
            event(&stop_home, "stop");
            Ok(())
        }),
        spawn: Box::new(move |path| {
            assert!(crate::runtime_config::RUNTIME_CONFIG_IO.try_lock().is_err());
            assert_eq!(path, formal);
            assert_eq!(
                std::fs::read(path).unwrap(),
                std::fs::read(spawn_home.join("checked")).unwrap(),
                "spawn consumes the once-validated full candidate verbatim"
            );
            event(&spawn_home, "spawn");
            Ok(())
        }),
    }));
    manager
}

async fn apply(manager: &MihomoManager, enabled: bool) -> Result<(), String> {
    tokio::time::timeout(
        Duration::from_secs(2),
        crate::services::tun::apply_tun_runtime(manager, true, enabled),
    )
    .await
    .expect("production TUN restart must not self-wait on IO")
}

async fn successful(kind: CoreKind, enabled: bool) {
    let (home, _home_guard) = super::conversion_fixture().await;
    let manager = seed(home.path(), kind, enabled, false, false);
    apply(&manager, enabled).await.unwrap();
    assert_eq!(events(home.path()), "resolve\npreflight\ncheck\nstop\nspawn\n");
    let args = std::fs::read_to_string(home.path().join("args")).unwrap();
    assert!(args.contains(if kind == CoreKind::Mihomo { "-t -d" } else { "check -c" }));
    assert!(crate::runtime_config::RUNTIME_CONFIG_IO.try_lock().is_ok());
    assert!(!manager.inner.restarting.load(Ordering::SeqCst));
    let bytes = std::fs::read(home.path().join("checked")).unwrap();
    if kind == CoreKind::Mihomo {
        let config: serde_yaml_ng::Mapping = serde_yaml_ng::from_slice(&bytes).unwrap();
        assert_eq!(config["tun"]["enable"].as_bool(), Some(enabled));
        assert_eq!(config["mixed-port"].as_u64(), Some(35123));
        assert_eq!(config["hosts"]["merge.example"].as_str(), Some("127.0.0.2"));
        assert_eq!(config["dns"]["enable"].as_bool(), Some(true));
        assert!(
            config["external-controller-cors"]["allow-origins"]
                .as_sequence()
                .unwrap()
                .contains(&"https://dash.example".into())
        );
        let rules = config["rules"].as_sequence().unwrap();
        for rule in [
            "DOMAIN,script.example,REJECT",
            "DOMAIN,profile.example,DIRECT",
            "DOMAIN,overlay.example,REJECT",
        ] {
            assert!(rules.contains(&rule.into()), "full candidate retains {rule}");
        }
    } else {
        assert!(args.contains("--disable-color"));
        let config: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            config["inbounds"]
                .as_array()
                .unwrap()
                .iter()
                .any(|inbound| inbound["type"] == "tun"),
            enabled
        );
        let json = String::from_utf8(bytes).unwrap();
        assert!(json.contains("script.example"));
        assert!(json.contains("overlay.example"));
        assert!(crate::runtime_config::last_singbox_degradation().await.is_some());
    }
}

#[tokio::test]
async fn owned_tun_off_mihomo_full_candidate_once_and_exact_spawn() {
    successful(CoreKind::Mihomo, false).await;
}
#[tokio::test]
async fn owned_tun_on_mihomo_explicit_flag_and_capability_stub() {
    successful(CoreKind::Mihomo, true).await;
}
#[tokio::test]
async fn owned_tun_off_singbox_full_candidate_and_report() {
    successful(CoreKind::SingBox, false).await;
}
#[tokio::test]
async fn owned_tun_on_singbox_explicit_flag_and_capability_stub() {
    successful(CoreKind::SingBox, true).await;
}

#[tokio::test]
async fn owned_tun_validation_failure_preserves_old_runtime_and_releases_lock() {
    for kind in [CoreKind::Mihomo, CoreKind::SingBox] {
        for enabled in [false, true] {
            let (home, _home_guard) = super::conversion_fixture().await;
            let manager = seed(home.path(), kind, enabled, true, false);
            let previous = std::fs::read(home.path().join("config.yaml")).unwrap();
            let previous_json = std::fs::read(home.path().join("singbox.json")).unwrap();
            let report_path = crate::runtime_config::singbox_conversion_path().unwrap();
            std::fs::write(&report_path, b"previous report bytes").unwrap();
            let error = apply(&manager, enabled).await.unwrap_err();
            assert!(error.contains("rejected"), "{error}");
            assert_eq!(events(home.path()), "resolve\npreflight\ncheck\n");
            assert_eq!(std::fs::read(home.path().join("config.yaml")).unwrap(), previous);
            assert_eq!(std::fs::read(home.path().join("singbox.json")).unwrap(), previous_json);
            assert_eq!(std::fs::read(report_path).unwrap(), b"previous report bytes");
            assert_eq!(manager.state(), CoreState::Running);
            assert_eq!(manager.pid(), Some(u32::MAX));
            let _lock = tokio::time::timeout(
                Duration::from_millis(100),
                crate::runtime_config::RUNTIME_CONFIG_IO.lock(),
            )
            .await
            .unwrap();
            assert!(!manager.inner.restarting.load(Ordering::SeqCst));
        }
    }
}

#[tokio::test]
async fn owned_tun_capability_failure_does_not_publish_or_stop() {
    let (home, _home_guard) = super::conversion_fixture().await;
    let manager = seed(home.path(), CoreKind::Mihomo, true, false, true);
    let previous = std::fs::read(home.path().join("config.yaml")).unwrap();
    assert!(
        apply(&manager, true)
            .await
            .unwrap_err()
            .contains("TUN capability preflight failed")
    );
    assert_eq!(events(home.path()), "resolve\npreflight\n");
    assert_eq!(std::fs::read(home.path().join("config.yaml")).unwrap(), previous);
    assert!(crate::runtime_config::RUNTIME_CONFIG_IO.try_lock().is_ok());
}

#[tokio::test]
async fn owned_tun_composition_failure_does_not_publish_or_stop() {
    let (home, _home_guard) = super::conversion_fixture().await;
    let manager = seed(home.path(), CoreKind::Mihomo, false, false, false);
    let previous = std::fs::read(home.path().join("config.yaml")).unwrap();
    std::fs::write(home.path().join("profiles/base.yaml"), "invalid: [").unwrap();
    assert!(apply(&manager, false).await.is_err());
    assert_eq!(events(home.path()), "resolve\npreflight\n");
    assert_eq!(std::fs::read(home.path().join("config.yaml")).unwrap(), previous);
    assert!(crate::runtime_config::RUNTIME_CONFIG_IO.try_lock().is_ok());
}

#[tokio::test]
async fn stopped_tun_without_pid_only_persists_and_releases_io() {
    let (home, _home_guard) = super::conversion_fixture().await;
    let manager = seed(home.path(), CoreKind::Mihomo, false, false, false);
    *manager.inner.pid.lock() = None;
    *manager.inner.state.lock() = CoreState::Stopped;
    for enabled in [true, false] {
        let path = crate::services::tun::write_tun_runtime(enabled).await.unwrap();
        let config: serde_yaml_ng::Mapping = serde_yaml_ng::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert_eq!(config["tun"]["enable"].as_bool(), Some(enabled));
        assert_eq!(manager.state(), CoreState::Stopped);
        assert_eq!(manager.pid(), None);
        assert!(events(home.path()).is_empty());
        assert!(crate::runtime_config::RUNTIME_CONFIG_IO.try_lock().is_ok());
    }
}
