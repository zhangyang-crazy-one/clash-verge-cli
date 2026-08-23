//! Quasi-end-to-end test against a REAL sing-box binary (task 9.3 slice).
//!
//! Tagged `#[ignore]`: spawns an actual core process and binds ports.
//! Run explicitly with:
//! ```bash
//! cargo test -p clash-verge-cli -- --ignored
//! ```
//! Skips gracefully (Ok) when no sing-box binary can be resolved.

use crate::mihomo_api::{MihomoApi, Transport};
use crate::mihomo_manager::singbox_binary;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

/// Build a minimal valid sing-box config for the probe test.
fn write_probe_config(dir: &PathBuf, controller: SocketAddr) -> anyhow::Result<PathBuf> {
    let input = crate::singbox::ConfigInput {
        outbounds: Vec::new(),
        groups: Vec::new(),
        mixed_port: controller.port() + 1,
        enable_tun: false,
        tun: crate::singbox::TunSettings {
            stack: "gvisor".into(),
            mtu: 9000,
        },
        clash_api: crate::singbox::ClashApiSettings {
            listen: controller,
            secret: "e2e-secret".into(),
        },
        rule_sets: Vec::new(),
    };
    let config = crate::singbox::generate_config(&input).map_err(anyhow::Error::msg)?;
    let path = dir.join("e2e-singbox.json");
    std::fs::write(&path, serde_json::to_string_pretty(&config)?)?;
    Ok(path)
}

#[tokio::test]
#[ignore = "spawns a real sing-box process; run: cargo test -p clash-verge-cli -- --ignored"]
async fn real_sing_box_spawns_and_answers_controller() {
    // Binary resolution must succeed now that the managed build exists.
    let Some(binary) = singbox_binary::candidate_without_install() else {
        eprintln!("skipping: no sing-box binary found");
        return;
    };

    // Reserve an ephemeral port for the controller.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let controller = listener.local_addr().expect("addr");
    drop(listener);

    let dir = std::env::temp_dir().join(format!("sb-e2e-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("mkdir");
    let config_path = write_probe_config(&dir, controller).expect("config");

    // Layer 1: static validation via the core itself.
    let check = std::process::Command::new(&binary)
        .arg("check")
        .arg("-c")
        .arg(&config_path)
        .output()
        .expect("run sing-box check");
    assert!(
        check.status.success(),
        "sing-box check failed: {}",
        String::from_utf8_lossy(&check.stderr)
    );

    // Layer 2: real spawn.
    let mut child = tokio::process::Command::new(&binary)
        .arg("run")
        .arg("-c")
        .arg(&config_path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn sing-box");

    // Layer 3: readiness probe through our own client stack. On failure
    // the child MUST be killed — a leaked orphan holds the ports and
    // poisons every later run.
    let api = MihomoApi::with_transport(Transport::Tcp(controller), "e2e-secret").expect("api");
    let version =
        match super::manager::probe_readiness(&api, crate::mihomo_manager::CoreKind::SingBox, Duration::from_secs(15))
            .await
        {
            Ok(v) => v,
            Err(error) => {
                let _ = child.kill().await;
                let _ = child.wait().await;
                panic!("readiness probe failed: {error}");
            }
        };
    assert!(version.starts_with("sing-box"), "unexpected version: {version}");

    // Layer 4: monitoring endpoint answers through the same client.
    let proxies = api.get_proxies().await.expect("proxies");
    assert!(
        proxies.proxies.contains_key("direct"),
        "generated config should expose the direct outbound"
    );

    // Layer 5: graceful teardown.
    child.kill().await.expect("kill");
    let _ = child.wait().await;

    let _ = std::fs::remove_dir_all(&dir);
}
