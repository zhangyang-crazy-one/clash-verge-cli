use std::time::Duration;

use crate::app::CoreState;
use crate::exit;
use crate::mihomo_manager::manager::{CoreStatus, MihomoManager};

/// Print the core's status. Exit code: 0 running, 3 stopped (or still
/// starting), 1 error. With `wait`, poll until the controller answers or
/// `wait` elapses, then report as usual (3 on timeout).
pub async fn run(manager: MihomoManager, json: bool, wait: Option<Duration>) -> anyhow::Result<i32> {
    let deadline = wait.map(|wait| tokio::time::Instant::now() + wait);
    let mut status = probe(&manager, deadline).await;
    if let Some(deadline) = deadline {
        while status.state != CoreState::Running && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(
                Duration::from_millis(250).min(deadline.saturating_duration_since(tokio::time::Instant::now())),
            )
            .await;
            // A core started meanwhile by another process is adopted here.
            manager.adopt_running_core();
            status = probe(&manager, Some(deadline)).await;
        }
    }

    if json {
        // The conversion losses of the last sing-box apply ride along with
        // the core status: `status --json` is what scripts read, and until
        // now a degraded run was only visible as a count in the apply log.
        let mut value = serde_json::to_value(&status)?;
        if let Some(record) = crate::runtime_config::last_singbox_degradation().await {
            value["singbox_conversion_degradations"] = serde_json::to_value(&record)?;
        }
        println!("{}", serde_json::to_string_pretty(&value)?);
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
        if let Some(record) = crate::runtime_config::last_singbox_degradation().await {
            println!(
                "   sing-box: {} conversion categories, {} notes, {} nodes skipped, {} fields degraded",
                record.digest.category_count(),
                record.digest.notes,
                record.digest.nodes_skipped,
                record.digest.fields_degraded
            );
        }
    }

    Ok(exit_code(&status.state))
}

/// The core's status, as ready only once its controller answers: a live
/// pid whose controller does not answer (yet) is reported as starting. With
/// a deadline, a probe that stalls past it counts as no answer.
async fn probe(manager: &MihomoManager, deadline: Option<tokio::time::Instant>) -> CoreStatus {
    let status = match deadline {
        Some(deadline) => tokio::time::timeout_at(deadline, manager.status())
            .await
            .unwrap_or_else(|_| manager.local_status()),
        None => manager.status().await,
    };
    ready(status)
}

fn ready(mut status: CoreStatus) -> CoreStatus {
    if status.state == CoreState::Running && status.version.is_none() {
        status.state = CoreState::Starting;
    }
    status
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

    #[tokio::test]
    async fn status_json_carries_the_last_apply_singbox_degradations() {
        // The digest is only in memory (the last apply of *this* process),
        // so `status --json` documents it under a dedicated key instead of
        // pretending the core status owns the field.
        let record = crate::runtime_config::SingboxDegradationRecord {
            seq: 7,
            digest: crate::runtime_config::SingboxDegradationDigest {
                notes: 13,
                nodes_skipped: 10,
                fields_degraded: 0,
                categories: std::collections::BTreeMap::from([(crate::runtime_config::DegradationCategory::Nodes, 10)]),
            },
        };
        let json = serde_json::to_value(&record).expect("digest serializes");
        assert_eq!(json["seq"], 7);
        assert_eq!(json["digest"]["nodes_skipped"], 10);
        assert_eq!(json["digest"]["categories"]["nodes"], 10);
        assert_eq!(
            json["digest"]["categories"]["dns"],
            serde_json::Value::Null,
            "absent categories stay absent instead of becoming zeroes"
        );
    }

    #[test]
    fn status_codes_follow_the_documented_convention() {
        assert_eq!(exit_code(&CoreState::Running), 0);
        assert_eq!(exit_code(&CoreState::Stopped), 3);
        assert_eq!(exit_code(&CoreState::Starting), 3, "not ready yet; use --wait");
        assert_eq!(exit_code(&CoreState::Error("x".into())), 1);
    }

    #[test]
    fn a_live_pid_is_not_ready_until_the_controller_answers() {
        let status = |version: Option<&str>| CoreStatus {
            state: CoreState::Running,
            pid: Some(42),
            uptime_secs: None,
            version: version.map(Into::into),
            socket_path: "/run/x.sock".into(),
            config_dir: "/cfg".into(),
        };
        assert_eq!(ready(status(None)).state, CoreState::Starting);
        assert_eq!(ready(status(Some("v1"))).state, CoreState::Running);
    }
}
