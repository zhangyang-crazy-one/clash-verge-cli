//! The interactive TUI's main loop.
//!
//! One `select!` multiplexes terminal input, the action channel, and the
//! timers; what each input means lives in [`super::handlers`]. The loop owns
//! the terminal and the render cadence.

use std::time::Duration;

use crossterm::event::{Event, EventStream, KeyEventKind};
use tokio::sync::mpsc;
use tokio::time;
use tokio_stream::StreamExt as _;

use crate::app::{Action, App, CoreState, View};
use crate::i18n::Language;
use crate::tui::TerminalGuard;
use crate::tui::handlers::{self, Ctx, Flow};

pub async fn run(config_dir: std::path::PathBuf) -> anyhow::Result<()> {
    let guard = std::sync::Arc::new(tokio::sync::Mutex::new(TerminalGuard::new()?));
    let (action_tx, mut action_rx) = mpsc::unbounded_channel::<Action>();

    let manager = crate::commands::build_manager(config_dir).await?;
    manager.set_action_tx(action_tx.clone());

    let mut app = App::new();
    app.gui_config = clash_verge_core::config::IVerge::new().await;
    app.language = Language::from_config(app.gui_config.language.as_deref());
    app.core_config = clash_verge_core::config::IClashTemp::new().await;
    app.clash_mode = app.core_config.get_mode().unwrap_or_else(|| "rule".into());
    app.log_level = app.configured_log_level();

    // Load profiles on start
    if let Ok(store) = crate::profile_store::store::ProfileStore::snapshot().await {
        app.selected_index = store.selected_index();
        app.load_profiles(&store);
        app.status_msg = Some(format!("{} profiles loaded", app.profiles.len()));
    }

    // Optional key remaps and mouse support; a broken file is reported and
    // ignored rather than keeping the TUI from starting.
    let tui_config = match crate::tui::keymap::TuiConfig::path().map(|path| crate::tui::keymap::TuiConfig::load(&path))
    {
        Some(Ok(config)) => config,
        Some(Err(error)) => {
            app.config_warning = Some(format!("{error:#} (ignored)"));
            crate::tui::keymap::TuiConfig::default()
        }
        None => crate::tui::keymap::TuiConfig::default(),
    };
    if tui_config.mouse {
        guard.lock().await.enable_mouse()?;
    }

    let ctx = Ctx {
        manager,
        tx: action_tx,
        guard,
        keys: tui_config.keys,
    };

    let mut events = EventStream::new();
    let mut render_tick = time::interval(Duration::from_millis(100));
    let mut runtime_refresh_tick = time::interval(Duration::from_secs(1));
    // Home's exit node and traffic totals: proxies and connections, slower.
    let mut home_refresh_tick = time::interval(Duration::from_secs(5));
    home_refresh_tick.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
    let mut auto_update_tick = time::interval(Duration::from_secs(30));
    auto_update_tick.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
    let mut auto_update_in_flight = false;
    // Re-read profiles.yaml every 5 min so external interval edits (GUI/user)
    // take effect without restarting the TUI.
    let mut profiles_refresh_tick = time::interval(Duration::from_secs(300));
    profiles_refresh_tick.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
    let auto_update_scheduler = std::sync::Arc::new(tokio::sync::Mutex::new(
        crate::subscribe::scheduler::AutoUpdateScheduler::new(),
    ));
    let mut rendered_view = app.view;

    // Detect a core the CLI itself started earlier (standalone socket).
    // The GUI is never probed.
    let api = ctx.manager.api();
    let tx = ctx.tx.clone();
    tokio::spawn(async move {
        if api.version().await.is_ok() {
            let _ = tx.send(Action::CoreStarted {
                version: None,
                binary_path: None,
                binary_source: None,
            });
        }
        // If no controller is available, the user can press s to start one.
    });

    // Read-only TUN capability state for the Settings view (no download,
    // no sudo). Uses the already-resolved binary or the no-download
    // candidate; refreshes again on CoreStarted / after explicit setup.
    {
        let manager = ctx.manager.clone();
        let tx = ctx.tx.clone();
        tokio::spawn(async move {
            let binary = manager
                .binary_path()
                .or_else(crate::mihomo_manager::binary::candidate_without_install);
            if let Some(path) = binary {
                let _ = tx.send(Action::TunCapabilityState(
                    crate::commands::privilege::has_tun_capability(&path),
                ));
            }
        });
    }

    loop {
        tokio::select! {
            maybe_event = events.next() => match maybe_event {
                Some(Ok(Event::Resize(_, _))) => ctx.guard.lock().await.reset_screen()?,
                Some(Ok(Event::Mouse(mouse))) => {
                    let screen = ctx.guard.lock().await.terminal_mut().size()?;
                    let screen = ratatui::layout::Rect::new(0, 0, screen.width, screen.height);
                    handlers::handle_mouse(&mut app, &ctx, mouse, screen).await;
                }
                Some(Ok(Event::Key(key))) if key.kind != KeyEventKind::Release => {
                    let flow = handlers::handle_key(&mut app, &ctx, key).await;
                    if flow == Flow::Quit {
                        break;
                    }
                }
                Some(Err(_)) | None => break,
                _ => {}
            },

            action = action_rx.recv() => match action {
                // Loop-owned state: the auto-update round guard.
                Some(Action::AutoUpdateFinished) => auto_update_in_flight = false,
                Some(action) => {
                    let flow = handlers::handle_event(&mut app, &ctx, action).await;
                    if flow == Flow::Quit {
                        break;
                    }
                }
                None => break,
            },

            _ = render_tick.tick() => {
                if app.view != rendered_view {
                    // Orca's terminal renderer can retain differential cells across
                    // alternate-screen view changes. Force one clean repaint per route.
                    ctx.guard.lock().await.reset_screen()?;
                    rendered_view = app.view;
                }
                ctx.guard.lock().await.terminal_mut().draw(|f| crate::ui::draw(f, &app))?;
            }

            _ = runtime_refresh_tick.tick(), if app.core_state == CoreState::Running => {
                let _ = ctx.tx.send(Action::TrafficRefresh);
                match app.view {
                    View::Connections => {
                        let _ = ctx.tx.send(Action::ConnectionsRefresh);
                    }
                    View::Logs => {
                        let _ = ctx.tx.send(Action::LogsRefresh);
                    }
                    _ => {}
                }
            }

            _ = home_refresh_tick.tick(), if app.core_state == CoreState::Running && app.view == View::Home => {
                let _ = ctx.tx.send(Action::ProxiesRefresh);
                let _ = ctx.tx.send(Action::ConnectionsRefresh);
            }

            _ = auto_update_tick.tick(), if !auto_update_in_flight => {
                auto_update_in_flight = true;
                handlers::spawn_auto_update(&app, &ctx, auto_update_scheduler.clone());
            }

            _ = profiles_refresh_tick.tick() => {
                // Re-read profiles.yaml so external interval edits (GUI/user)
                // take effect without restarting the TUI.
                if let Ok(store) = crate::profile_store::store::ProfileStore::snapshot().await {
                    app.load_profiles(&store);
                }
            }
        }
    }

    // A core this TUI spawned is supervised (and its output piped) by this
    // process only: stop it cleanly rather than leave it unsupervised. A core
    // adopted from `clash-verge-cli start` keeps running under its own
    // supervisor.
    if ctx.manager.owns_child() {
        let _ = ctx.manager.stop().await;
    }

    Ok(())
}
