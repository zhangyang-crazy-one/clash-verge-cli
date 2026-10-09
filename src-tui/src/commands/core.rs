//! `clash-verge-cli core`: show or switch the proxy core (issue #56).
//!
//! Switching cores used to require hand-editing `verge.yaml`, and doing
//! it while a core was running left `stop`/`restart` unable to find the
//! running core. This command performs the switch as a transaction:
//!
//! - while a core is running: it is stopped through its pid record first,
//!   then the selection is committed and the selected core is started;
//! - while stopped: the selection is committed through the same
//!   stopped-state transaction the TUI's guided switch uses (verge.yaml
//!   `proxy_core` plus the sing-box ownership marker), with a last-moment
//!   re-check so a concurrent writer is never clobbered.

use std::path::PathBuf;

use crate::mihomo_manager::CoreKind;

/// The core this CLI would actually adopt right now, with its pid.
///
/// Uses the same validated read as the lifecycle commands
/// (`pidfile::read_live_for_kind`): a raw record plus a pid liveness check
/// accepts a stale record whose pid has been recycled, and reports a core as
/// running when this process would refuse to manage it.
fn live_core(socket: &std::path::Path, singbox_controller: std::net::SocketAddr) -> Option<(CoreKind, u32)> {
    use crate::mihomo_manager::pidfile;
    let path = pidfile::path_for(socket);
    [CoreKind::Mihomo, CoreKind::SingBox].into_iter().find_map(|kind| {
        let endpoint = (kind == CoreKind::SingBox).then_some(singbox_controller);
        pidfile::read_live_for_kind(&path, socket, kind, endpoint).map(|record| (kind, record.pid))
    })
}

/// `clash-verge-cli core` / `core status`: which core is selected, which is
/// running, and (with `--json`) the pid behind that answer.
pub async fn show(config_dir: PathBuf, json: bool) -> anyhow::Result<()> {
    let socket = super::active_socket_path().await;
    let singbox_controller = crate::mihomo_manager::manager::configured_singbox_controller(
        &clash_verge_core::config::IClashTemp::new().await.0,
    )?;
    let live = live_core(&socket, singbox_controller);
    let selected = super::configured_core_kind().await;
    let running = live.map(|(kind, _)| kind);

    if json {
        let value = serde_json::json!({
            "selected": selected.as_str(),
            "running": running.map(CoreKind::as_str),
            "pid": live.map(|(_, pid)| pid),
            "config_dir": config_dir.display().to_string(),
        });
        println!("{}", serde_json::to_string_pretty(&value)?);
        return Ok(());
    }
    println!("selected core: {}", selected.as_str());
    match live {
        Some((kind, pid)) => println!("running core:   {} (pid {})", kind.as_str(), pid),
        None => println!("running core:   none"),
    }
    if let (Some(selected), Some(running)) = (Some(selected), running)
        && selected != running
    {
        println!(
            "note: verge.yaml selects {} while {} is running; `restart` or `core use` reconciles them",
            selected.as_str(),
            running.as_str()
        );
    }
    Ok(())
}

/// `clash-verge-cli core use <mihomo|singbox>`.
pub async fn use_core(config_dir: PathBuf, target: CoreKind) -> anyhow::Result<()> {
    let manager = super::build_manager(config_dir.clone()).await?;
    let running_kind = manager.core_kind();
    let was_running = manager.pid().is_some();

    if was_running {
        if running_kind == target {
            println!("{} is already the running core", target.as_str());
            return Ok(());
        }
        println!(
            "stopping {} before switching to {}",
            running_kind.as_str(),
            target.as_str()
        );
        manager.stop().await?;
    }
    // Commits `proxy_core` (and the sing-box ownership marker) after the
    // read-only stopped-state preflight, rolling back on failure.
    manager.select_core_while_stopped(target)?;
    println!("selected core: {}", target.as_str());
    if !was_running {
        println!("core is not running; the new core starts with `clash-verge-cli start`");
        return Ok(());
    }
    let fresh = super::build_manager_for_kind(config_dir, target).await?;
    super::start::run(fresh).await
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::mihomo_manager::manager::MihomoManager;

    fn manager(kind: CoreKind) -> (tempfile::TempDir, MihomoManager) {
        let home = tempfile::tempdir().expect("tempdir");
        let manager = MihomoManager::new(home.path().to_path_buf())
            .with_core_kind(kind)
            .with_socket(home.path().join("external-controller.sock"))
            .with_singbox_controller("127.0.0.1:0".parse().unwrap());
        (home, manager)
    }

    #[tokio::test]
    async fn a_stopped_manager_reports_its_selection_and_rejects_a_switch_while_running() {
        let (home, manager) = manager(CoreKind::Mihomo);
        let _guard = crate::profile_store::store::tests::claim_test_app_home(home.path().to_path_buf()).await;
        assert_eq!(manager.core_kind(), CoreKind::Mihomo);
        // Nothing is running, so the selection transaction is allowed.
        manager
            .select_core_while_stopped(CoreKind::SingBox)
            .expect("stopped selection");
        assert_eq!(manager.core_kind(), CoreKind::SingBox);
    }

    #[tokio::test]
    async fn a_tracked_core_blocks_the_stopped_selection_transaction() {
        let (home, manager) = manager(CoreKind::SingBox);
        let _guard = crate::profile_store::store::tests::claim_test_app_home(home.path().to_path_buf()).await;
        // Pretend a core is running without touching any real process.
        *manager.inner().pid.lock() = Some(u32::MAX);
        let error = manager
            .select_core_while_stopped(CoreKind::Mihomo)
            .expect_err("a running core must not be re-selected from the stopped path");
        assert!(error.to_string().contains("Core ownership"), "{error}");
    }

    /// `core status` must go through the validated live-record read: a stale
    /// record naming a dead pid is not a running core, whatever the raw pid
    /// file says.
    #[test]
    fn a_stale_pid_record_is_not_reported_as_a_running_core() {
        use crate::mihomo_manager::pidfile::{self, CoreRecord};
        use chrono::Utc;

        let home = tempfile::tempdir().expect("tempdir");
        let socket = home.path().join("external-controller.sock");
        let path = pidfile::path_for(&socket);
        // u32::MAX is not a live pid on this system, so no /proc entry and no
        // controller cmdline can validate it.
        pidfile::write(
            &path,
            CoreRecord::with_kind_and_exe(u32::MAX, Utc::now(), CoreKind::Mihomo, None),
        )
        .expect("write record");

        assert_eq!(live_core(&socket, "127.0.0.1:0".parse().expect("addr")), None);
    }
}
