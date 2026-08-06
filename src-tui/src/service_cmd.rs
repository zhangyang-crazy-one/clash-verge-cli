//! systemd service management: install/uninstall/status for clash-verge-cli daemon.

use std::process::Command;

const SERVICE_NAME: &str = "clash-verge-cli";
const SERVICE_PATH: &str = "/etc/systemd/system/clash-verge-cli.service";

/// Generate systemd unit file content.
/// The binary runs in foreground mode so systemd can track the PID properly.
pub fn unit_content(binary_path: &str, config_dir: &str) -> String {
    format!(
        r#"[Unit]
Description=Clash Verge CLI - mihomo proxy daemon
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStart={binary_path} start --foreground --config-dir {config_dir}
ExecStop=/bin/kill -TERM $MAINPID
Restart=on-failure
RestartSec=5
LimitNOFILE=65536

[Install]
WantedBy=multi-user.target
"#
    )
}

/// Install systemd service (requires root).
pub fn install_service(binary_path: &str, config_dir: &str, start_now: bool) -> std::io::Result<()> {
    let unit = unit_content(binary_path, config_dir);
    // Write to a temp dir under /tmp with a unique name — avoids the
    // well-known /tmp/clash-verge-cli.service symlink race.
    let dir = tempfile::tempdir().map_err(std::io::Error::other)?;
    let tmp_path = dir.path().join("clash-verge-cli.service");
    std::fs::write(&tmp_path, unit)?;

    let status = Command::new("sudo")
        .args(["cp", &tmp_path.to_string_lossy(), SERVICE_PATH])
        .status()?;
    if !status.success() {
        return Err(std::io::Error::other("sudo cp failed — need root privileges"));
    }
    // Temp dir is cleaned up when `dir` is dropped.

    let _ = Command::new("sudo").args(["systemctl", "daemon-reload"]).status();
    let _ = Command::new("sudo")
        .args(["systemctl", "enable", SERVICE_NAME])
        .status();

    if start_now {
        let _ = Command::new("sudo").args(["systemctl", "start", SERVICE_NAME]).status();
        println!("service installed and started");
    } else {
        println!("service installed (not started)");
    }

    Ok(())
}

/// Uninstall systemd service.
pub fn uninstall_service() -> std::io::Result<()> {
    let _ = Command::new("sudo").args(["systemctl", "stop", SERVICE_NAME]).status();
    let _ = Command::new("sudo")
        .args(["systemctl", "disable", SERVICE_NAME])
        .status();
    let status = Command::new("sudo").args(["rm", "-f", SERVICE_PATH]).status()?;
    if !status.success() {
        return Err(std::io::Error::other("sudo rm failed — need root privileges"));
    }
    let _ = Command::new("sudo").args(["systemctl", "daemon-reload"]).status();
    println!("service uninstalled");

    Ok(())
}

/// Check service active state.
pub fn service_active_state() -> String {
    let output = Command::new("systemctl").args(["is-active", SERVICE_NAME]).output();
    match output {
        Ok(o) => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        Err(_) => "unknown".to_string(),
    }
}

/// Check service enabled state.
pub fn service_enabled_state() -> String {
    let output = Command::new("systemctl").args(["is-enabled", SERVICE_NAME]).output();
    match output {
        Ok(o) => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        Err(_) => "unknown".to_string(),
    }
}
