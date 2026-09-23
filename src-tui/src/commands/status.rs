use std::time::Duration;

use crate::app::CoreState;
use crate::exit;
use crate::mihomo_manager::manager::MihomoManager;

/// Print the core's status. Exit code: 0 running, 3 stopped (or still
/// starting), 1 error. With `wait`, poll until the core runs or `wait`
/// elapses, then report as usual (3 on timeout).
pub async fn run(manager: MihomoManager, json: bool, wait: Option<Duration>) -> anyhow::Result<i32> {
    let mut status = manager.status().await;
    if let Some(wait) = wait {
        let deadline = tokio::time::Instant::now() + wait;
        while status.state != CoreState::Running && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(250)).await;
            // A core started meanwhile by another process is adopted here.
            manager.adopt_running_core();
            status = manager.status().await;
        }
    }

    if json {
        println!("{}", serde_json::to_string_pretty(&status)?);
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
            println!("   Uptime: {}m {}s", uptime / 60, uptime % 60);
        }
        if let Some(version) = &status.version {
            println!("   Version: {version}");
        }
        println!("   Socket: {}", status.socket_path.display());
        println!("   Config: {}", status.config_dir.display());
    }

    Ok(exit_code(&status.state))
}

const fn exit_code(state: &CoreState) -> i32 {
    match state {
        CoreState::Running => exit::SUCCESS,
        CoreState::Starting | CoreState::Stopped => exit::NOT_RUNNING,
        CoreState::Error(_) => exit::FAILURE,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_codes_follow_the_documented_convention() {
        assert_eq!(exit_code(&CoreState::Running), 0);
        assert_eq!(exit_code(&CoreState::Stopped), 3);
        assert_eq!(exit_code(&CoreState::Starting), 3, "not ready yet; use --wait");
        assert_eq!(exit_code(&CoreState::Error("x".into())), 1);
    }
}
