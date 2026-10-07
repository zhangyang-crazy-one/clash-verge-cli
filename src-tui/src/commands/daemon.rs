//! Foreground daemon mode: the core's supervisor under systemd, and the
//! detached supervisor `clash-verge-cli start` launches.
//!
//! Starts mihomo and hosts the subscription auto-update scheduler (the same
//! 30 s cadence the interactive TUI uses), then blocks on SIGTERM / SIGINT.
//! On signal, any in-flight refresh is cancelled before mihomo stops cleanly.
//! The daemon also exits when its core stops for good: stopped on purpose by
//! another process (`clash-verge-cli stop`), or crashed past the auto-restart
//! limit (then with an error, so systemd's restart policy applies).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::signal;
use tokio::sync::Mutex;
use tokio::time;

use crate::commands;
use crate::subscribe::lifecycle::{ManagerLifecycle, reload_with_lifecycle};
use crate::subscribe::scheduler::AutoUpdateScheduler;

pub async fn run(config_dir: PathBuf) -> anyhow::Result<()> {
    let manager = commands::build_manager(config_dir).await?;
    let (lifecycle_tx, mut lifecycle_rx) = tokio::sync::mpsc::channel(64);
    manager.set_action_tx(lifecycle_tx);
    // TUN capability preflight happens inside the manager, after binary
    // resolution and before spawn — never sudo/setcap/askpass on this path.
    manager.start().await?;
    let manager = Arc::new(manager);

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
            Some(action) = lifecycle_rx.recv() => {
                let action = match action {
                    crate::app::Action::CoreGeneration { generation, action } if generation == manager.current_generation() => *action,
                    crate::app::Action::CoreGeneration { .. } => continue,
                    other => other,
                };
                match action {
                crate::app::Action::CoreExited(_) if manager.state() == crate::app::CoreState::Stopped => {
                    tracing::info!(target: "daemon", "mihomo was stopped, exiting");
                    if let Some(handle) = in_flight.take() {
                        handle.abort();
                        let _ = handle.await;
                    }
                    return Ok(());
                }
                crate::app::Action::CoreError(error) => {
                    if let Some(handle) = in_flight.take() {
                        handle.abort();
                        let _ = handle.await;
                    }
                    anyhow::bail!("mihomo stopped: {error}");
                }
                _ => {}
                }
            },
            _ = term.recv() => {
                tracing::info!(target: "daemon", "received SIGTERM, stopping");
                break;
            }
            _ = int.recv() => {
                tracing::info!(target: "daemon", "received SIGINT, stopping");
                break;
            }
            completed = async {
                match in_flight.as_mut() {
                    Some(handle) => Some(handle.await),
                    None => std::future::pending().await,
                }
            } => {
                in_flight = None;
                if let Some(Err(error)) = completed {
                    tracing::error!(target: "auto_update", "background refresh task failed: {error}");
                }
            }
            _ = auto_update_tick.tick(), if in_flight.is_none() => {
                let sched = scheduler.clone();
                let manager = Arc::clone(&manager);
                let handle = tokio::spawn(async move {
                    let (outcome, probe) = {
                        let mut scheduler = sched.lock().await;
                        (
                            scheduler.tick().await,
                            scheduler.probe_with_manager(&manager, enable_tun, true).await,
                        )
                    };
                    for (uid, is_current) in outcome.updated {
                        tracing::info!(target: "auto_update", "refreshed {uid} (current={is_current})");
                        if is_current {
                            let lifecycle = ManagerLifecycle::new(&manager);
                            match reload_with_lifecycle(&lifecycle, &uid, enable_tun).await {
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
        let _ = handle.await;
    }
    manager.stop().await?;
    tracing::info!(target: "daemon", "mihomo stopped, exiting");
    Ok(())
}
