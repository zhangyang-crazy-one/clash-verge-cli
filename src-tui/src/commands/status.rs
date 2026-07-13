use crate::app::CoreState;
use crate::mihomo_manager::manager::MihomoManager;

/// Print mihomo status and return exit code: 0=Running, 1=Stopped, 2=Error.
pub async fn run(manager: MihomoManager, json: bool) -> anyhow::Result<i32> {
    let status = manager.status().await;

    if json {
        let s = serde_json::to_string_pretty(&status)?;
        println!("{s}");
    } else {
        match &status.state {
            CoreState::Running => println!("\u{25cf} RUNNING"),
            CoreState::Starting => println!("\u{25cb} STARTING"),
            CoreState::Stopped => println!("\u{25cb} STOPPED"),
            CoreState::Error(msg) => println!("\u{25cf} ERROR: {msg}"),
        }
        if let Some(pid) = status.pid {
            println!("   PID: {pid}");
        }
        if let Some(uptime) = status.uptime_secs {
            let m = uptime / 60;
            let s = uptime % 60;
            println!("   Uptime: {m}m {s}s");
        }
        if let Some(version) = &status.version {
            println!("   Version: {version}");
        }
        println!("   Socket: {}", status.socket_path.display());
        println!("   Config: {}", status.config_dir.display());
    }

    let code = match status.state {
        CoreState::Running | CoreState::Starting => 0,
        CoreState::Stopped => 1,
        CoreState::Error(_) => 2,
    };
    Ok(code)
}
