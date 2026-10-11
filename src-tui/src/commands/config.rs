//! Deliberate user authorization for browser origins on both controllers.

use anyhow::Context as _;
use clash_verge_core::config::IVerge;
use serde_yaml_ng::Mapping;
use std::path::Path;

const ORIGINS_KEY: &str = "controller_allowed_origins";

fn check_key(key: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        key == ORIGINS_KEY,
        "unsupported config key; supported key: {ORIGINS_KEY}"
    );
    Ok(())
}

fn normalized_origins(value: &str) -> anyhow::Result<Vec<String>> {
    let values: Vec<String> = serde_yaml_ng::from_str(value)
        .context("controller_allowed_origins must be a YAML/JSON list of http(s) origins")?;
    let mut normalized = Vec::new();
    for value in values {
        let origin = crate::enhance::canonical_controller_origin(&value).map_err(anyhow::Error::msg)?;
        if !normalized.contains(&origin) {
            normalized.push(origin);
        }
    }
    Ok(normalized)
}

fn output(configured: &[String], effective: &[String], json: bool) -> anyhow::Result<()> {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "key": ORIGINS_KEY, "configured": configured, "effective": effective,
            }))?
        );
    } else {
        println!("{ORIGINS_KEY}: {}", serde_json::to_string(configured)?);
        println!("effective controller origins: {}", serde_json::to_string(effective)?);
    }
    Ok(())
}

pub async fn get(key: &str, json: bool) -> anyhow::Result<()> {
    check_key(key)?;
    let settings = IVerge::try_new().await?;
    let configured: Vec<String> = settings
        .controller_allowed_origins
        .unwrap_or_default()
        .into_iter()
        .map(|origin| origin.to_string())
        .collect();
    let effective = crate::enhance::controller_origins(&configured).map_err(anyhow::Error::msg)?;
    output(&configured, &effective, json)
}

pub async fn set(key: &str, value: &str, json: bool) -> anyhow::Result<()> {
    check_key(key)?;
    let origins = normalized_origins(value)?;
    let path = clash_verge_core::utils::dirs::verge_path()?;
    persist_origins(&path, &origins).await?;
    // Reload the actual file, rather than printing only the value we tried to
    // save. The shared typed schema must retain the new field too.
    get(key, json).await?;
    if !json {
        println!("Restart the proxy core to apply the controller origins.");
    }
    Ok(())
}

async fn persist_origins(path: &Path, origins: &[String]) -> anyhow::Result<()> {
    crate::enhance::controller_origins(origins).map_err(anyhow::Error::msg)?;
    let original = match tokio::fs::read(path).await {
        Ok(bytes) => Some(bytes),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error).context("cannot read verge.yaml"),
    };
    let mut settings: Mapping = match &original {
        Some(bytes) => serde_yaml_ng::from_slice(bytes).context("cannot modify malformed verge.yaml")?,
        None => Mapping::new(),
    };
    settings.insert(ORIGINS_KEY.into(), serde_yaml_ng::to_value(origins)?);
    let typed: IVerge = serde_yaml_ng::from_value(settings.clone().into()).context("invalid verge.yaml settings")?;
    anyhow::ensure!(
        typed
            .controller_allowed_origins
            .as_ref()
            .map(|values| values.iter().map(|value| value.as_str()).collect::<Vec<_>>())
            == Some(origins.iter().map(String::as_str).collect()),
        "controller_allowed_origins was not retained by the settings schema"
    );
    let body = serde_yaml_ng::to_string(&settings)?;
    if let Some(original) = original {
        // Owner-private backup of the complete original mapping before replacement.
        write_private_atomic(&path.with_extension("yaml.bak"), &original)?;
    }
    write_private_atomic(path, body.as_bytes())?;
    Ok(())
}

fn write_private_atomic(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    use std::io::Write as _;
    let parent = path.parent().context("settings file has no parent")?;
    std::fs::create_dir_all(parent)?;
    let mut staged = tempfile::NamedTempFile::new_in(parent)?;
    staged.write_all(bytes)?;
    staged.as_file().sync_all()?;
    staged.persist(path).map_err(|error| error.error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn controller_origins_normalize_and_reject_non_origins() {
        assert_eq!(
            normalized_origins("['HTTPS://Example.COM:443', 'https://example.com']").unwrap(),
            vec!["https://example.com"]
        );
        for value in [
            "null",
            "'*'",
            "[123]",
            "['*']",
            "['https://*.example.com']",
            "['https://%2a.example.com']",
            "['https://user:pass@example.com']",
            "['https://example.com/']",
            "['https://example.com/path']",
            "['https://example.com?x=1']",
            "['https://example.com#fragment']",
            "['ftp://example.com']",
        ] {
            assert!(normalized_origins(value).is_err(), "{value}");
        }
        assert!(check_key("secret").is_err());
    }

    #[tokio::test]
    async fn controller_origins_save_reload_preserves_unknown_fields_and_private_backup() {
        let home = tempfile::tempdir().unwrap();
        let _guard = crate::profile_store::store::tests::claim_test_app_home(home.path().to_path_buf()).await;
        let path = home.path().join("verge.yaml");
        let original = "proxy_core: singbox\nlanguage: zh\nprovider_extension: {nested: [1, 2]}\n";
        std::fs::write(&path, original).unwrap();
        persist_origins(&path, &["https://dashboard.example:8443".into()])
            .await
            .unwrap();
        let settings = IVerge::try_new().await.unwrap();
        assert_eq!(
            settings.controller_allowed_origins.unwrap()[0],
            "https://dashboard.example:8443"
        );
        assert_eq!(settings.proxy_core.as_deref(), Some("singbox"));
        assert!(settings.extra.contains_key("provider_extension"));
        // The typed settings save used by the TUI must retain the new field.
        IVerge::try_new().await.unwrap().save_file().await.unwrap();
        assert_eq!(
            IVerge::try_new().await.unwrap().controller_allowed_origins.unwrap()[0],
            "https://dashboard.example:8443"
        );
        assert_eq!(
            std::fs::read_to_string(path.with_extension("yaml.bak")).unwrap(),
            original
        );
        assert!(
            crate::enhance::configured_controller_origins()
                .await
                .unwrap()
                .contains(&"https://dashboard.example:8443".into())
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            for file in [&path, &path.with_extension("yaml.bak")] {
                assert_eq!(std::fs::metadata(file).unwrap().permissions().mode() & 0o077, 0);
            }
        }
        persist_origins(&path, &[]).await.unwrap();
        assert_eq!(
            crate::enhance::configured_controller_origins().await.unwrap(),
            crate::singbox::config_gen::CLASH_API_ALLOW_ORIGINS
        );
        let before = std::fs::read(&path).unwrap();
        assert!(
            persist_origins(&path, &["https://example.com/path".into()])
                .await
                .is_err()
        );
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    #[tokio::test]
    async fn controller_origins_apply_to_both_cores_without_trusting_provider_cors() {
        let home = tempfile::tempdir().unwrap();
        let _guard = crate::profile_store::store::tests::claim_test_app_home(home.path().to_path_buf()).await;
        let path = home.path().join("verge.yaml");
        let native = r#"{"outbounds":[{"type":"direct","tag":"direct"}],"route":{},"experimental":{"clash_api":{"access_control_allow_origin":["*","https://provider.example"]}}}"#;
        let expected_defaults = crate::enhance::controller_origins(&[]).unwrap();
        for configured in [
            None,
            Some(vec![]),
            Some(vec!["https://dashboard.example:8443".to_string()]),
        ] {
            if let Some(additional) = &configured {
                persist_origins(&path, additional).await.unwrap();
            }
            let additional = configured.unwrap_or_default();
            let expected = crate::enhance::controller_origins(&additional).unwrap();
            // Runtime writing always replaces provider CORS from user settings.
            let mapping: Mapping = serde_yaml_ng::from_str("secret: isolated-fixture\nexternal-controller-cors: {allow-origins: ['*', 'https://provider.example']}\n").unwrap();
            crate::runtime_config::write_runtime_config_unlocked(mapping, false)
                .await
                .unwrap();
            let mihomo: Mapping =
                serde_yaml_ng::from_str(&std::fs::read_to_string(home.path().join("config.yaml")).unwrap()).unwrap();
            assert_eq!(
                mihomo["external-controller-cors"]["allow-origins"],
                serde_yaml_ng::to_value(&expected).unwrap()
            );
            assert_eq!(
                mihomo["external-controller-cors"]["allow-private-network"],
                serde_yaml_ng::Value::Bool(!additional.is_empty())
            );
            for (name, source) in [("converted", None), ("native", Some(native))] {
                let output = home.path().join(format!("{name}.json"));
                crate::mihomo_manager::manager::ManagerInner::write_singbox_assembled_to(
                    home.path(),
                    source,
                    false,
                    &output,
                )
                .await
                .unwrap();
                let singbox: serde_json::Value = serde_json::from_slice(&std::fs::read(&output).unwrap()).unwrap();
                assert_eq!(
                    singbox["experimental"]["clash_api"]["access_control_allow_origin"],
                    serde_json::json!(&expected)
                );
                assert_eq!(
                    singbox["experimental"]["clash_api"]["access_control_allow_private_network"],
                    !additional.is_empty()
                );
                assert!(
                    !serde_json::to_string(&singbox)
                        .unwrap()
                        .contains("https://provider.example")
                );
            }
            assert!(expected_defaults.iter().all(|origin| expected.contains(origin)));
        }
        // Hand-edited invalid settings fail before replacing a generated file.
        std::fs::write(&path, "controller_allowed_origins: ['*']\n").unwrap();
        let output = home.path().join("unchanged.json");
        std::fs::write(&output, "untouched").unwrap();
        assert!(
            crate::mihomo_manager::manager::ManagerInner::write_singbox_assembled_to(home.path(), None, false, &output)
                .await
                .is_err()
        );
        assert_eq!(std::fs::read_to_string(output).unwrap(), "untouched");
        assert!(crate::enhance::configured_controller_origins().await.is_err());
    }

    /// Explicit-only loopback integration. Copies the supplied pinned binaries
    /// into the private fixture directory (no capabilities or GUI config).
    #[tokio::test]
    #[ignore = "requires explicit CLASH_TEST_SINGBOX_BIN and CLASH_TEST_MIHOMO_BIN; starts loopback-only fixture cores"]
    async fn pinned_cores_controller_origins_return_only_authorized_cors_headers() {
        use std::process::{Child, Command, Stdio};
        use std::time::{Duration, Instant};

        struct CoreGuard(Child);
        impl Drop for CoreGuard {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        fn free_port() -> u16 {
            std::net::TcpListener::bind("127.0.0.1:0")
                .unwrap()
                .local_addr()
                .unwrap()
                .port()
        }
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(1))
            .build()
            .unwrap();
        let allowed = "https://dashboard.example:8443";
        let unlisted = "https://unlisted.example";
        assert!(normalized_origins("['*']").is_err());
        for (kind, variable, expected_version) in [
            ("singbox", "CLASH_TEST_SINGBOX_BIN", "1.14.2"),
            ("mihomo", "CLASH_TEST_MIHOMO_BIN", "v1.19.32"),
        ] {
            let source = std::env::var_os(variable)
                .unwrap_or_else(|| panic!("{variable} must name the explicit pinned test binary"));
            assert!(Path::new(&source).is_file(), "{variable} binary does not exist");
            let home = tempfile::tempdir().unwrap();
            let _guard = crate::profile_store::store::tests::claim_test_app_home(home.path().to_path_buf()).await;
            let copied = home.path().join(format!("fixture-{kind}"));
            std::fs::copy(source, &copied).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                std::fs::set_permissions(&copied, std::fs::Permissions::from_mode(0o700)).unwrap();
            }
            let version = Command::new(&copied)
                .arg(if kind == "singbox" { "version" } else { "-v" })
                .output()
                .unwrap();
            assert!(version.status.success());
            assert!(
                String::from_utf8_lossy(&version.stdout).contains(expected_version),
                "wrong {kind} version"
            );
            for extra in [None, Some(vec![allowed.to_string()])] {
                let controller_port = free_port();
                let mut mixed_port = free_port();
                while mixed_port == controller_port {
                    mixed_port = free_port();
                }
                let verge_path = home.path().join("verge.yaml");
                std::fs::write(
                    &verge_path,
                    format!("verge_mixed_port: {mixed_port}\nenable_tun_mode: false\nenable_system_proxy: false\n"),
                )
                .unwrap();
                if let Some(origins) = &extra {
                    persist_origins(&verge_path, origins).await.unwrap();
                }
                let reloaded = crate::enhance::configured_controller_origins().await.unwrap();
                assert_eq!(reloaded.contains(&allowed.to_string()), extra.is_some());
                let mapping: Mapping = serde_yaml_ng::from_str(&format!(
                    "mode: rule\nmixed-port: {mixed_port}\nport: 0\nsocks-port: 0\nredir-port: 0\ntproxy-port: 0\nallow-lan: false\nbind-address: 127.0.0.1\nsecret: isolated-cors-fixture\nexternal-controller: 127.0.0.1:{controller_port}\nproxies: []\nproxy-groups: []\nrules: ['MATCH,DIRECT']\nexternal-controller-cors: {{allow-origins: ['*']}}\n"
                )).unwrap();
                let mihomo_path = crate::runtime_config::write_runtime_config_unlocked(mapping, false)
                    .await
                    .unwrap();
                let config = if kind == "singbox" {
                    let output = home.path().join("singbox.json");
                    crate::mihomo_manager::manager::ManagerInner::write_singbox_assembled_to(
                        home.path(),
                        Some(r#"{"outbounds":[{"type":"direct","tag":"direct"}],"route":{"final":"direct"}}"#),
                        false,
                        &output,
                    )
                    .await
                    .unwrap();
                    output
                } else {
                    mihomo_path
                };
                let log_path = home.path().join("core.log");
                let log = std::fs::File::create(&log_path).unwrap();
                let mut command = Command::new(&copied);
                if kind == "singbox" {
                    command.args(["run", "-D"]).arg(home.path()).arg("-c").arg(&config);
                } else {
                    command.arg("-d").arg(home.path()).arg("-f").arg(&config);
                }
                let mut core = CoreGuard(
                    command
                        .stdin(Stdio::null())
                        .stdout(log.try_clone().unwrap())
                        .stderr(log)
                        .spawn()
                        .unwrap(),
                );
                let url = format!("http://127.0.0.1:{controller_port}/version");
                let deadline = Instant::now() + Duration::from_secs(10);
                loop {
                    if let Some(status) = core.0.try_wait().unwrap() {
                        panic!(
                            "{kind} exited {status}: {}",
                            std::fs::read_to_string(&log_path).unwrap()
                        );
                    }
                    if client
                        .get(&url)
                        .bearer_auth("isolated-cors-fixture")
                        .send()
                        .await
                        .is_ok_and(|response| response.status().is_success())
                    {
                        break;
                    }
                    assert!(
                        Instant::now() < deadline,
                        "{kind} readiness failed: {}",
                        std::fs::read_to_string(&log_path).unwrap()
                    );
                    tokio::time::sleep(Duration::from_millis(30)).await;
                }
                for origin in [allowed, unlisted] {
                    let preflight = client
                        .request(reqwest::Method::OPTIONS, &url)
                        .header("Origin", origin)
                        .header("Access-Control-Request-Method", "GET")
                        .header("Access-Control-Request-Headers", "Authorization")
                        .header("Access-Control-Request-Private-Network", "true")
                        .send()
                        .await
                        .unwrap();
                    let preflight_origin = preflight
                        .headers()
                        .get("access-control-allow-origin")
                        .map(|value| value.to_str().unwrap());
                    let private_network = preflight
                        .headers()
                        .get("access-control-allow-private-network")
                        .map(|value| value.to_str().unwrap());
                    if origin == allowed && extra.is_some() {
                        assert!(
                            preflight.status().is_success(),
                            "{kind} refused authorized PNA preflight"
                        );
                        assert_eq!(preflight_origin, Some(origin), "{kind}: preflight origin");
                        assert_eq!(private_network, Some("true"), "{kind}: PNA preflight");
                    } else {
                        assert!(
                            preflight_origin.is_none(),
                            "{kind} admitted unlisted preflight origin {origin}"
                        );
                        assert_ne!(
                            private_network,
                            Some("true"),
                            "{kind} enabled PNA for unlisted origin {origin}"
                        );
                    }
                    let response = client
                        .get(&url)
                        .bearer_auth("isolated-cors-fixture")
                        .header("Origin", origin)
                        .send()
                        .await
                        .unwrap();
                    let header = response
                        .headers()
                        .get("access-control-allow-origin")
                        .map(|value| value.to_str().unwrap());
                    if origin == allowed && extra.is_some() {
                        assert!(response.status().is_success(), "{kind} rejected allowed origin");
                        assert_eq!(header, Some(origin), "{kind}: {origin}");
                    } else {
                        assert!(
                            header.is_none(),
                            "{kind} admitted an unlisted origin {origin}: {header:?}"
                        );
                    }
                }
                if extra.is_some() {
                    let unauthorized = client.get(&url).header("Origin", allowed).send().await.unwrap();
                    assert_eq!(
                        unauthorized.status(),
                        reqwest::StatusCode::UNAUTHORIZED,
                        "{kind}: whitelist must not bypass controller authentication"
                    );
                }
                println!(
                    "{kind} {expected_version}: {} CORS policy and unlisted-origin refusal verified",
                    if extra.is_some() {
                        "explicit whitelist"
                    } else {
                        "default local-only"
                    }
                );
                drop(core);
            }
        }
    }
}
