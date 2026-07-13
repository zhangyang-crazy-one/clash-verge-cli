// systemd service management: install/uninstall/status for clash-verge-cli daemon.

use std::path::PathBuf;
use std::process::Command;

const SERVICE_NAME: &str = "clash-verge-cli";
const SERVICE_PATH: &str = "/etc/systemd/system/clash-verge-cli.service";

/// Generate systemd unit file content.
pub fn unit_content(binary_path: &str, config_dir: &str) -> String {
    format!(
        r#"[Unit]
Description=Clash Verge CLI - mihomo proxy daemon
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStart={binary_path} start --config-dir {config_dir}
ExecStop={binary_path} stop
Restart=on-failure
RestartSec=5
LimitNOFILE=65536

[Install]
WantedBy=multi-user.target
"#
    )
}

/// Install systemd service (requires root).
pub fn install_service(binary_path: &str, config_dir: &str) -> std::io::Result<()> {
    let unit = unit_content(binary_path, config_dir);
    let tmp_path = "/tmp/clash-verge-cli.service";

    std::fs::write(tmp_path, unit)?;

    let status = Command::new("sudo").args(["cp", tmp_path, SERVICE_PATH]).status()?;
    if !status.success() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            "sudo cp failed — need root privileges",
        ));
    }

    let _ = Command::new("sudo").args(["systemctl", "daemon-reload"]).status();
    let _ = Command::new("sudo")
        .args(["systemctl", "enable", SERVICE_NAME])
        .status();

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
        return Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            "sudo rm failed — need root privileges",
        ));
    }
    let _ = Command::new("sudo").args(["systemctl", "daemon-reload"]).status();

    Ok(())
}

/// Check service status.
pub fn service_status() -> String {
    let output = Command::new("systemctl").args(["is-active", SERVICE_NAME]).output();

    match output {
        Ok(o) => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        Err(_) => "unknown".to_string(),
    }
}
