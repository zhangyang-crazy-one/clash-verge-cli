//! Foreground daemon mode for systemd / process supervisor.
//!
//! Starts mihomo and hosts the subscription auto-update scheduler (the same
//! 30 s cadence the interactive TUI uses), then blocks on SIGTERM / SIGINT.
//! On signal, any in-flight refresh is cancelled before mihomo stops cleanly.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::signal;
use tokio::sync::Mutex;
use tokio::time;

use crate::commands;
use crate::subscribe::scheduler::{AutoUpdateScheduler, reload_current_profile};

pub async fn run(config_dir: PathBuf) -> anyhow::Result<()> {
    let manager = commands::build_manager(config_dir).await?;
    // TUN capability preflight happens inside the manager, after binary
    // resolution and before spawn — never sudo/setcap/askpass on this path.
    manager.start().await?;

    // Reload target state for current-profile refreshes (owned core → running).
    let gui = clash_verge_core::config::IVerge::new().await;
    let enable_tun = gui.enable_tun_mode.unwrap_or(false);

    let scheduler = Arc::new(Mutex::new(AutoUpdateScheduler::new()));
    let mut auto_update_tick = time::interval(Duration::from_secs(30));
    auto_update_tick.set_missed_tick_behavior(time::MissedTickBehavior::Skip);

    // Wait for SIGTERM (systemd stop) or SIGINT (Ctrl-C).
    let mut term = signal::unix::signal(signal::unix::SignalKind::terminate())?;
    let mut int = signal::unix::signal(signal::unix::SignalKind::interrupt())?;
    let mut in_flight: Option<tokio::task::JoinHandle<()>> = None;

    loop {
        tokio::select! {
            _ = term.recv() => {
                tracing::info!(target: "daemon", "received SIGTERM, stopping");
                break;
            }
            _ = int.recv() => {
                tracing::info!(target: "daemon", "received SIGINT, stopping");
                break;
            }
            _ = auto_update_tick.tick(), if in_flight.is_none() => {
                let sched = scheduler.clone();
                let api = manager.api();
                let handle = tokio::spawn(async move {
                    let (outcome, probe) = {
                        let mut scheduler = sched.lock().await;
                        (
                            scheduler.tick().await,
                            scheduler.probe(&api, enable_tun, true).await,
                        )
                    };
                    for (uid, is_current) in outcome.updated {
                        tracing::info!(target: "auto_update", "refreshed {uid} (current={is_current})");
                        if is_current {
                            match reload_current_profile(&api, &uid, enable_tun, true).await {
                                Ok(()) => {
                                    tracing::info!(target: "auto_update", "reloaded current profile {uid}")
                                }
                                Err(error) => {
                                    tracing::error!(target: "auto_update", "reload {uid} failed: {error}")
                                }
                            }
                        }
                    }
                    for (uid, error) in outcome.failed {
                        tracing::warn!(target: "auto_update", "update {uid} failed: {error}");
                    }
                    if let Some(error) = outcome.errored {
                        tracing::error!(target: "auto_update", "auto-update batch failed: {error}");
                    }
                    if probe.forced_refresh {
                        if probe.rolled_back {
                            tracing::warn!(target: "probe", "selected node vanished — refresh rolled back");
                        } else if probe.may_be_down {
                            tracing::error!(target: "probe", "subscription may be down after forced refresh");
                        } else {
                            tracing::info!(target: "probe", "node recovered — subscription refreshed");
                        }
                    }
                    if let Some(error) = probe.error {
                        tracing::warn!(target: "probe", "probe error: {error}");
                    }
                });
                in_flight = Some(handle);
            }
        }
    }

    // Cancel any in-flight refresh before stopping the core.
    if let Some(handle) = in_flight.take() {
        handle.abort();
    }
    manager.stop().await?;
    tracing::info!(target: "daemon", "mihomo stopped, exiting");
    Ok(())
}
