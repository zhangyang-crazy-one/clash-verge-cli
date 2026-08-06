//! Non-interactive service CLI commands.

use crate::service_cmd;

pub fn install(binary_path: &str, config_dir: &str, start_now: bool) -> anyhow::Result<()> {
    service_cmd::install_service(binary_path, config_dir, start_now)?;
    Ok(())
}

pub fn uninstall() -> anyhow::Result<()> {
    service_cmd::uninstall_service()?;
    Ok(())
}

pub fn status(json: bool) -> anyhow::Result<()> {
    let active = service_cmd::service_active_state();
    let enabled = service_cmd::service_enabled_state();
    if json {
        let payload = serde_json::json!({
            "active": active,
            "enabled": enabled,
        });
        println!("{}", serde_json::to_string_pretty(&payload).unwrap());
    } else {
        println!("active: {active}");
        println!("enabled: {enabled}");
    }
    Ok(())
}
