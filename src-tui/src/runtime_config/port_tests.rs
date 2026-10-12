//! Regeneration uses persisted listener intent, including explicit zero ports.

use super::*;
use clash_verge_core::config::PrfItem;
use serde_yaml_ng::{Mapping, Value};
use std::path::Path;

const PARTIAL_VERGE: &str = "proxy_core: mihomo\nenable_tun_mode: false\nenable_system_proxy: false\n";
const PORTS: [&str; 4] = ["port", "socks-port", "redir-port", "tproxy-port"];
const BASE_RULES: &str = "proxies: []\nrules: ['MATCH,DIRECT']\n";

fn fixture() -> tempfile::TempDir {
    use std::os::unix::fs::PermissionsExt as _;
    let home = tempfile::tempdir().unwrap();
    std::fs::set_permissions(home.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    home
}

fn seed(home: &Path, secret: &str, profile: bool) -> PrfItem {
    std::fs::write(home.join("verge.yaml"), PARTIAL_VERGE).unwrap();
    std::fs::write(home.join("config.yaml"), format!("# preserve until install\nsecret: {secret}\nmixed-port: 35123\nport: 0\nsocks-port: 0\nredir-port: 0\ntproxy-port: 0\nallow-lan: false\ntun: {{enable: false, stack: gvisor, app-extra: retained}}\nfuture: {{app: retained}}\n{BASE_RULES}")).unwrap();
    let item: PrfItem = serde_yaml_ng::from_str(
        "uid: base\ntype: local\nname: fixture\nfile: base.yaml\noption: {merge: custom, script: hook}\n",
    )
    .unwrap();
    if profile {
        std::fs::write(home.join("profiles.yaml"), "current: base\nitems:\n  - {uid: base, type: local, name: fixture, file: base.yaml, option: {merge: custom, script: hook}}\n  - {uid: custom, type: merge, file: merge.yaml}\n  - {uid: hook, type: script, file: hook.js}\n").unwrap();
        std::fs::write(home.join("profiles/base.yaml"), "port: 30001\nsocks-port: 30002\nredir-port: 30003\ntproxy-port: 30004\nfuture: {profile: retained}\ntun: {mtu: 1350}\nproxies: []\nrules: ['MATCH,DIRECT']\n").unwrap();
        std::fs::write(home.join("profiles/merge.yaml"), "hosts: {merge.example: 127.0.0.2}\n").unwrap();
        std::fs::write(home.join("profiles/hook.js"), "function main(config) { config.rules.unshift('DOMAIN,' + (config['socks-port'] === 0 ? 'disabled' : 'enabled') + '.example,REJECT'); config['port'] = 30005; config.tun.stack = 'system'; config['script-observed-port'] = config['socks-port']; return config; }").unwrap();
        let source =
            crate::routing::load_profile_rules("rules: ['DOMAIN,disabled.example,REJECT', 'MATCH,DIRECT']\n").unwrap();
        let mut edited = source.clone();
        edited.insert(1, crate::routing::from_clash_rule_str("DOMAIN,overlay.example,DIRECT"));
        let overlay = crate::rule_overlay::RuleOverlay::from_edit(&source, &edited).unwrap();
        std::fs::write(
            crate::rule_overlay::path(&home.join("profiles"), &item).unwrap(),
            overlay.serialize().unwrap(),
        )
        .unwrap();
    }
    item
}

fn read(path: &Path) -> Mapping {
    serde_yaml_ng::from_slice(&std::fs::read(path).unwrap()).unwrap()
}

fn assert_zero_ports(config: &Mapping) {
    for port in PORTS {
        assert_eq!(config[port].as_u64(), Some(0), "{port} must remain disabled");
    }
}

#[tokio::test]
async fn issue85_regeneration_preserves_zero_ports_with_strong_and_weak_secrets_and_full_chain() {
    for profile in [false, true] {
        for secret in ["fixture-strong-secret", "set-your-secret"] {
            let home = fixture();
            let _home = crate::profile_store::store::tests::claim_test_app_home(home.path().to_path_buf()).await;
            seed(home.path(), secret, profile);
            let formal = home.path().join("config.yaml");
            let previous = std::fs::read(&formal).unwrap();
            let candidate = tempfile::NamedTempFile::new_in(home.path()).unwrap();
            write_mihomo_candidate(true, candidate.path()).await.unwrap();
            assert_eq!(
                std::fs::read(&formal).unwrap(),
                previous,
                "secret preparation must not publish"
            );
            let config = read(candidate.path());
            assert_zero_ports(&config);
            assert_eq!(config["mixed-port"].as_u64(), Some(35123));
            assert_eq!(config["tun"]["enable"].as_bool(), Some(true));
            assert_eq!(config["tun"]["stack"].as_str(), Some("gvisor"));
            assert_eq!(config["tun"]["app-extra"].as_str(), Some("retained"));
            assert_eq!(config["dns"]["enable"].as_bool(), Some(true));
            assert_eq!(
                config["external-controller-cors"]["allow-private-network"].as_bool(),
                Some(false)
            );
            assert!(!crate::enhance::is_placeholder_secret(
                config["secret"].as_str().unwrap()
            ));
            if secret == "fixture-strong-secret" {
                assert_eq!(config["secret"].as_str(), Some(secret));
            }
            if profile {
                assert_eq!(config["future"]["profile"].as_str(), Some("retained"));
                assert_eq!(config["hosts"]["merge.example"].as_str(), Some("127.0.0.2"));
                assert_eq!(config["script-observed-port"].as_u64(), Some(0));
                assert_eq!(config["rules"][0].as_str(), Some("DOMAIN,disabled.example,REJECT"));
                assert_eq!(config["rules"][1].as_str(), Some("DOMAIN,overlay.example,DIRECT"));
                assert_eq!(config["tun"]["mtu"].as_u64(), Some(1350));
            } else {
                assert_eq!(config["future"]["app"].as_str(), Some("retained"));
            }
        }
    }
}

#[tokio::test]
async fn issue85_explicit_verge_enable_flags_override_raw_ports() {
    for profile in [false, true] {
        for enabled in [false, true] {
            let home = fixture();
            let _home = crate::profile_store::store::tests::claim_test_app_home(home.path().to_path_buf()).await;
            seed(home.path(), "fixture-strong-secret", profile);
            std::fs::write(home.path().join("verge.yaml"), format!("{PARTIAL_VERGE}verge_http_enabled: {enabled}\nverge_port: 35201\nverge_socks_enabled: {enabled}\nverge_socks_port: 35202\nverge_redir_enabled: {enabled}\nverge_redir_port: 35203\nverge_tproxy_enabled: {enabled}\nverge_tproxy_port: 35204\n")).unwrap();
            let candidate = tempfile::NamedTempFile::new_in(home.path()).unwrap();
            write_mihomo_candidate(false, candidate.path()).await.unwrap();
            let config = read(candidate.path());
            for (port, expected) in PORTS.into_iter().zip(35201..=35204) {
                assert_eq!(config.get(port).and_then(Value::as_u64), enabled.then_some(expected));
            }
        }
    }
}

#[tokio::test]
async fn issue85_missing_controls_and_files_inherit_templates_without_normalizing_existing_zero() {
    for profile in [false, true] {
        let home = fixture();
        let _home = crate::profile_store::store::tests::claim_test_app_home(home.path().to_path_buf()).await;
        seed(home.path(), "fixture-strong-secret", profile);
        let formal = home.path().join("config.yaml");
        let candidate = tempfile::NamedTempFile::new_in(home.path()).unwrap();
        for missing in [false, true] {
            if missing {
                std::fs::remove_file(&formal).unwrap();
            } else {
                std::fs::write(&formal, "mixed-port: 35123\nsecret: fixture-strong-secret\n").unwrap();
            }
            write_mihomo_candidate(false, candidate.path()).await.unwrap();
            let config = read(candidate.path());
            for (port, default) in PORTS.into_iter().zip([7899, 7898, 7895, 7896]) {
                assert_eq!(config[port].as_u64(), Some(default));
            }
            assert_eq!(config["mixed-port"].as_u64(), Some(if missing { 7897 } else { 35123 }));
            assert_eq!(
                formal.exists(),
                !missing,
                "missing-file composition only writes candidate"
            );
        }
        seed(home.path(), "fixture-strong-secret", profile);
        std::fs::remove_file(home.path().join("verge.yaml")).unwrap();
        write_mihomo_candidate(false, candidate.path()).await.unwrap();
        let config = read(candidate.path());
        assert_eq!(config["mixed-port"].as_u64(), Some(7897));
        for port in PORTS {
            assert!(!config.contains_key(port), "fresh verge template disables {port}");
        }
        std::fs::remove_file(&formal).unwrap();
        write_mihomo_candidate(false, candidate.path()).await.unwrap();
        assert!(!formal.exists());
        assert_eq!(read(candidate.path())["mixed-port"].as_u64(), Some(7897));
    }
}

#[tokio::test]
async fn issue85_rule_editor_and_runtime_chain_observe_the_same_raw_controls() {
    let home = fixture();
    let _home = crate::profile_store::store::tests::claim_test_app_home(home.path().to_path_buf()).await;
    let item = seed(home.path(), "fixture-strong-secret", true);
    let substituted =
        crate::routing::load_profile_rules("rules: ['DOMAIN,edited.example,DIRECT', 'MATCH,DIRECT']\n").unwrap();
    let composed = compose_profile_rules(&item, &substituted).await.unwrap();
    assert_eq!(
        composed[0],
        crate::routing::from_clash_rule_str("DOMAIN,disabled.example,REJECT")
    );
    assert_eq!(&composed[1..], &substituted);
    let runtime = load_remote_profile_with_rules(&item).await.unwrap();
    assert_eq!(runtime["rules"][0].as_str(), Some("DOMAIN,disabled.example,REJECT"));
    assert_zero_ports(&runtime);
}

#[tokio::test]
async fn issue85_unreadable_or_malformed_existing_config_fails_without_publication() {
    for profile in [false, true] {
        let home = fixture();
        let _home = crate::profile_store::store::tests::claim_test_app_home(home.path().to_path_buf()).await;
        let item = seed(home.path(), "fixture-strong-secret", profile);
        let formal = home.path().join("config.yaml");
        let candidate = home.path().join("candidate.yaml");
        for invalid in ["invalid: [", "[]", ""] {
            std::fs::write(&formal, invalid).unwrap();
            std::fs::write(&candidate, "unpublished").unwrap();
            assert!(write_mihomo_candidate(false, &candidate).await.is_err());
            assert_eq!(std::fs::read_to_string(&formal).unwrap(), invalid);
            assert_eq!(std::fs::read_to_string(&candidate).unwrap(), "unpublished");
            if profile {
                assert!(load_remote_profile_with_rules(&item).await.is_err());
                assert!(compose_profile_rules(&item, &[]).await.is_err());
            }
        }
        std::fs::remove_file(&formal).unwrap();
        std::fs::create_dir(&formal).unwrap();
        assert!(write_mihomo_candidate(false, &candidate).await.is_err());
        assert!(formal.is_dir(), "a read error must not be treated as fresh install");
        std::fs::remove_dir(&formal).unwrap();
        std::os::unix::fs::symlink(home.path().join("missing-target.yaml"), &formal).unwrap();
        assert!(write_mihomo_candidate(false, &candidate).await.is_err());
        assert!(std::fs::symlink_metadata(&formal).unwrap().file_type().is_symlink());
        assert_eq!(std::fs::read_to_string(&candidate).unwrap(), "unpublished");
        if profile {
            assert!(load_remote_profile_with_rules(&item).await.is_err());
            assert!(compose_profile_rules(&item, &[]).await.is_err());
        }
    }
}

#[tokio::test]
async fn issue85_raw_loader_preserves_yaml_merges_and_unknown_values() {
    let home = fixture();
    let _home = crate::profile_store::store::tests::claim_test_app_home(home.path().to_path_buf()).await;
    std::fs::write(home.path().join("verge.yaml"), PARTIAL_VERGE).unwrap();
    std::fs::write(home.path().join("config.yaml"), "defaults: &listeners {port: 0, socks-port: 0, redir-port: 0, tproxy-port: 0}\n<<: *listeners\nsecret: fixture-secret\nfuture: {nested: [1, custom, false]}\n").unwrap();
    let candidate = tempfile::NamedTempFile::new_in(home.path()).unwrap();
    write_mihomo_candidate(false, candidate.path()).await.unwrap();
    let config = read(candidate.path());
    assert_zero_ports(&config);
    assert_eq!(
        config["future"]["nested"],
        serde_yaml_ng::from_str::<Value>("[1, custom, false]").unwrap()
    );
    assert_eq!(config["defaults"]["port"].as_u64(), Some(0));
}

#[tokio::test]
async fn issue85_strong_and_weak_secret_candidates_validate_before_exact_atomic_install() {
    use std::os::unix::fs::PermissionsExt as _;
    for profile in [false, true] {
        for secret in ["fixture-strong-secret", "set-your-secret"] {
            let home = fixture();
            let _home = crate::profile_store::store::tests::claim_test_app_home(home.path().to_path_buf()).await;
            seed(home.path(), secret, profile);
            let formal = home.path().join("config.yaml");
            let previous = std::fs::read(&formal).unwrap();
            std::fs::write(home.path().join("previous"), &previous).unwrap();
            let validator = home.path().join("validator");
            std::fs::write(&validator, "#!/bin/sh\nset -eu\n[ \"$1\" = -t ] && [ \"$2\" = -d ] && [ \"$4\" = -f ]\ncmp \"$3/previous\" \"$3/config.yaml\"\n[ \"$(stat -c %a \"$5\")\" = 600 ]\ncp \"$5\" \"$3/checked\"\n[ -f \"$3/allow\" ]\n").unwrap();
            std::fs::set_permissions(&validator, std::fs::Permissions::from_mode(0o700)).unwrap();
            let manager = crate::mihomo_manager::MihomoManager::new(home.path().to_path_buf());
            *manager.inner().resolved_binary.lock() = Some(validator);
            let mode = crate::commands::start::SupervisorLaunch::Regenerate;
            assert!(
                manager
                    .prepare_mihomo_launch(mode)
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains("rejected")
            );
            assert_eq!(std::fs::read(&formal).unwrap(), previous);
            assert_zero_ports(&read(&home.path().join("checked")));
            std::fs::write(home.path().join("allow"), "").unwrap();
            assert_eq!(
                manager.prepare_mihomo_launch(mode).await.unwrap(),
                crate::commands::start::SupervisorLaunch::UseExistingConfig
            );
            assert_eq!(
                std::fs::read(&formal).unwrap(),
                std::fs::read(home.path().join("checked")).unwrap()
            );
            let config = read(&formal);
            assert_zero_ports(&config);
            assert!(!crate::enhance::is_placeholder_secret(
                config["secret"].as_str().unwrap()
            ));
            assert_eq!(std::fs::metadata(&formal).unwrap().permissions().mode() & 0o777, 0o600);
            assert!(
                !std::fs::read_dir(home.path()).unwrap().any(|entry| entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".tmp")),
                "private candidates cleaned after acceptance and rejection"
            );
        }
    }
}
