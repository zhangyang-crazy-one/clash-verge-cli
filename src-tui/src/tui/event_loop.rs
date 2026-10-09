//! The interactive TUI's main loop.
//!
//! One `select!` multiplexes terminal input, the action channel, and the
//! timers; what each input means lives in [`super::handlers`]. The loop owns
//! the terminal and the render cadence.

use std::time::Duration;

use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind};
use tokio::sync::mpsc;
use tokio::time;
use tokio_stream::StreamExt as _;

use crate::app::{Action, App, CoreState, InputMode, View};
use crate::i18n::Language;
use crate::tui::TerminalGuard;
use crate::tui::handlers::{self, Ctx, Flow};

pub async fn run(config_dir: std::path::PathBuf) -> anyhow::Result<()> {
    let guard = std::sync::Arc::new(tokio::sync::Mutex::new(TerminalGuard::new()?));
    let (action_tx, mut action_rx) = mpsc::channel::<Action>(256);
    let (traffic_tx, mut traffic_rx) = tokio::sync::watch::channel(None);
    let (log_tx, mut log_rx) = tokio::sync::mpsc::channel(256);
    let dropped_logs = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));

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

    let local_actions = std::sync::Arc::new(parking_lot::Mutex::new(handlers::LocalActionQueue::new()));
    let action_sender = crate::tui::background::EventSender::new(action_tx)
        .with_manager(manager.inner())
        .with_local(local_actions.clone());
    let ctx = Ctx {
        manager,
        tx: action_sender,
        traffic_tx,
        log_tx,
        dropped_logs: dropped_logs.clone(),
        local_actions,
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
    let mut dirty = true;
    let mut work_generation = ctx.tx.current_generation();

    // Detect a core the CLI itself started earlier (standalone socket).
    // The GUI is never probed.
    let api = ctx.manager.api();
    let tx = ctx.tx.for_current();
    ctx.tx.for_current().spawn(async move {
        if api.version().await.is_ok() {
            let _ = tx
                .send(Action::CoreStarted {
                    version: None,
                    binary_path: None,
                    binary_source: None,
                })
                .await;
        }
        // If no controller is available, the user can press s to start one.
    });

    // Read-only TUN capability state for the Settings view (no download,
    // no sudo). Uses the already-resolved binary or the no-download
    // candidate; refreshes again on CoreStarted / after explicit setup.
    {
        let manager = ctx.manager.clone();
        let tx = ctx.tx.for_current();
        ctx.tx.for_current().spawn(async move {
            let binary = manager
                .binary_path()
                .or_else(crate::mihomo_manager::binary::candidate_without_install);
            if let Some(path) = binary {
                let _ = tx
                    .send(Action::TunCapabilityState(
                        crate::commands::privilege::has_tun_capability(&path),
                    ))
                    .await;
            }
        });
    }

    // Error captured from the render/reset paths: `?` inside the loop would
    // return early and skip the owned-core cleanup below, so loop errors are
    // recorded and surfaced AFTER the cleanup runs.
    let mut loop_error: Option<anyhow::Error> = None;

    loop {
        if work_generation != ctx.tx.current_generation() {
            work_generation = ctx.tx.current_generation();
            auto_update_in_flight = false;
        }
        if ctx.local_actions.lock().take_overflow() {
            app.status_msg = Some("Too many local requests; retry the last operation".into());
            dirty = true;
        }
        let mut quit_after_local = false;
        while let Some(action) = ctx.take_local_action() {
            dirty = true;
            if handlers::handle_event(&mut app, &ctx, action).await == Flow::Quit {
                loop_error = None;
                quit_after_local = true;
                break;
            }
        }
        if quit_after_local {
            break;
        }
        tokio::select! {
            changed = traffic_rx.changed() => if changed.is_ok() {
                if let Some((generation, core_generation, traffic)) = traffic_rx.borrow_and_update().clone()
                    && generation == ctx.tx.current_generation()
                    && core_generation == ctx.manager.current_generation() {
                    handlers::handle_event(&mut app, &ctx, Action::TrafficFetched(traffic)).await;
                    dirty = true;
                }
            },
            Some((generation, core_generation, log)) = log_rx.recv() => {
                if generation != ctx.tx.current_generation() || core_generation != ctx.manager.current_generation() { continue; }
                let dropped = dropped_logs.swap(0, std::sync::atomic::Ordering::Relaxed);
                if dropped > 0 {
                    app.status_msg = Some(format!("dropped {dropped} live log entries under load"));
                }
                handlers::handle_event(&mut app, &ctx, Action::LogReceived(log)).await;
                dirty = true;
            },
            maybe_event = events.next() => match maybe_event {
                Some(Ok(Event::Resize(_, _))) => {
                    if let Err(error) = ctx.guard.lock().await.reset_screen() {
                        loop_error = Some(error);
                        break;
                    }
                    dirty = true;
                }
                Some(Ok(Event::Mouse(mouse))) => {
                    let screen = match ctx.guard.lock().await.terminal_mut().size() {
                        Ok(size) => size,
                        Err(error) => {
                            loop_error = Some(error.into());
                            break;
                        }
                    };
                    let screen = ratatui::layout::Rect::new(0, 0, screen.width, screen.height);
                    handlers::handle_mouse(&mut app, &ctx, mouse, screen).await;
                    dirty = true;
                }
                Some(Ok(Event::Key(key))) if key.kind != KeyEventKind::Release => {
                    // Sing-box / rule-edit prompt buffers are typed text —
                    // the key map does not know their grammar, so handle
                    // them here before the global dispatch.
                    dirty = true;
                    if handle_singbox_input_mode(&mut app, &ctx, key.code) {
                        // Input-mode key consumed; do not propagate.
                    } else {
                        let flow = handlers::handle_key(&mut app, &ctx, key).await;
                        if flow == Flow::Quit {
                            break;
                        }
                        dirty = true;
                    }
                }
                Some(Err(_)) | None => break,
                _ => {}
            },

            action = action_rx.recv() => match action {
                // Loop-owned state: the auto-update round guard.
                Some(Action::AutoUpdateFinished) => auto_update_in_flight = false,
                Some(action) => {
                    let Some(action) = ctx.tx.accept(action) else { continue; };
                    if matches!(action, Action::AutoUpdateFinished) { auto_update_in_flight = false; continue; }
                    let flow = handlers::handle_event(&mut app, &ctx, action).await;
                    if flow == Flow::Quit {
                        break;
                    }
                    dirty = true;
                }
                None => break,
            },

            _ = render_tick.tick() => {
                if !dirty {
                    continue;
                }
                if app.view != rendered_view {
                    // Orca's terminal renderer can retain differential cells across
                    // alternate-screen view changes. Force one clean repaint per route.
                    if let Err(error) = ctx.guard.lock().await.reset_screen() {
                        loop_error = Some(error);
                        break;
                    }
                    rendered_view = app.view;
                }
                if let Err(error) = ctx.guard.lock().await.terminal_mut().draw(|f| crate::ui::draw(f, &app)) {
                    loop_error = Some(error.into());
                    break;
                }
                dirty = false;
            }

            _ = runtime_refresh_tick.tick(), if app.core_state == CoreState::Running => {
                ctx.send(Action::TrafficRefresh);
                match app.view {
                    View::Connections => {
                        ctx.send(Action::ConnectionsRefresh);
                    }
                    View::Logs => {
                        ctx.send(Action::LogsRefresh);
                    }
                    _ => {}
                }
            }

            _ = home_refresh_tick.tick(), if app.core_state == CoreState::Running && app.view == View::Home => {
                ctx.send(Action::ProxiesRefresh);
                ctx.send(Action::ConnectionsRefresh);
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
                    dirty = true;
                }
            }
        }
    }

    // A core this TUI spawned is supervised (and its output piped) by this
    // process only: stop it cleanly rather than leave it unsupervised. This
    // runs on EVERY exit path — clean quit AND render/reset errors captured
    // in `loop_error` above. A core adopted from `clash-verge-cli start`
    // keeps running under its own supervisor.
    action_rx.close();
    ctx.tx.cancel_and_wait().await;
    finish_loop(loop_error, async {
        handlers::finish_owned_operation(&mut app).await;
        if ctx.manager.owns_child() {
            ctx.manager.stop().await?;
        }
        Ok(())
    })
    .await
}

/// Draw/reset failures still await owned cleanup before returning the error.
async fn finish_loop<F>(loop_error: Option<anyhow::Error>, cleanup: F) -> anyhow::Result<()>
where
    F: std::future::Future<Output = anyhow::Result<()>>,
{
    let cleanup_result = cleanup.await;
    match (loop_error, cleanup_result) {
        (Some(error), Err(cleanup)) => Err(error.context(format!("owned core cleanup also failed: {cleanup}"))),
        (Some(error), Ok(())) => Err(error),
        (None, result) => result,
    }
}

/// Handle a key while a sing-box / rule-edit input-mode prompt is open.
///
/// Returns `true` when the key was consumed by the input-mode machinery
/// (so the caller must NOT forward it to the global key map). The handlers
/// live in this function — they are small per-mode inline state machines
/// because each grammar (`tag|remote|url`, `kind=value>target`,
/// `kind|tag|server|port|detour`) is too narrow to deserve its own module.
fn handle_singbox_input_mode(app: &mut App, ctx: &Ctx, code: KeyCode) -> bool {
    let buffer = match &app.input_mode {
        InputMode::RuleInput(_)
        | InputMode::RuleSetInput(_)
        | InputMode::RuleFormInput(_)
        | InputMode::DnsServerInput(_)
        | InputMode::DnsRuleInput(_)
        | InputMode::DnsResolverInput(_) => match &app.input_mode {
            InputMode::RuleInput(b) => ModeBuffer::RuleInput(b.clone()),
            InputMode::RuleSetInput(b) => ModeBuffer::RuleSet(b.clone()),
            InputMode::RuleFormInput(b) => ModeBuffer::RuleForm(b.clone()),
            InputMode::DnsServerInput(b) => ModeBuffer::DnsServer(b.clone()),
            InputMode::DnsRuleInput(b) => ModeBuffer::DnsRule(b.clone()),
            InputMode::DnsResolverInput(b) => ModeBuffer::DnsResolver(b.clone()),
            _ => unreachable!(),
        },
        _ => return false,
    };

    match buffer {
        ModeBuffer::RuleInput(_) => handle_rule_input_prompt(app, code, buffer),
        ModeBuffer::RuleSet(_) => handle_rule_set_prompt(app, code, buffer),
        ModeBuffer::RuleForm(_) => handle_rule_form_prompt(app, code, buffer),
        ModeBuffer::DnsServer(_) => handle_dns_server_prompt(app, ctx, code, buffer),
        ModeBuffer::DnsRule(_) => handle_dns_rule_prompt(app, ctx, code, buffer),
        ModeBuffer::DnsResolver(_) => handle_dns_resolver_prompt(app, ctx, code, buffer),
    }
    true
}

/// Snapshot of the active prompt buffer so the per-mode arms don't have to
/// borrow `app.input_mode` again while mutating it.
enum ModeBuffer {
    RuleInput(String),
    RuleSet(String),
    RuleForm(String),
    DnsServer(String),
    DnsRule(String),
    DnsResolver(String),
}

impl ModeBuffer {
    fn text(&self) -> &str {
        match self {
            ModeBuffer::RuleInput(s)
            | ModeBuffer::RuleSet(s)
            | ModeBuffer::RuleForm(s)
            | ModeBuffer::DnsServer(s)
            | ModeBuffer::DnsRule(s)
            | ModeBuffer::DnsResolver(s) => s,
        }
    }

    /// Drop one character from the buffer and reinstall the trimmed
    /// `InputMode` variant. Esc cancels back to `Normal`.
    fn pop_char(&self, char_count: usize) -> Option<InputMode> {
        let mut text = self.text().to_string();
        for _ in 0..char_count {
            text.pop();
        }
        self.clone_with(text)
    }

    fn push_char(&self, ch: char) -> InputMode {
        let mut text = self.text().to_string();
        text.push(ch);
        self.clone_with(text)
            .expect("ModeBuffer variants always have a matching InputMode variant")
    }

    fn clone_with(&self, text: String) -> Option<InputMode> {
        match self {
            ModeBuffer::RuleInput(_) => Some(InputMode::RuleInput(text)),
            ModeBuffer::RuleSet(_) => Some(InputMode::RuleSetInput(text)),
            ModeBuffer::RuleForm(_) => Some(InputMode::RuleFormInput(text)),
            ModeBuffer::DnsServer(_) => Some(InputMode::DnsServerInput(text)),
            ModeBuffer::DnsRule(_) => Some(InputMode::DnsRuleInput(text)),
            ModeBuffer::DnsResolver(_) => Some(InputMode::DnsResolverInput(text)),
        }
    }
}

/// Persist the in-memory DNS spec edit to disk. Mirrors
/// `handlers::rules::persist_dns_spec` but is duplicated here because the
/// `handlers` module is private and the input-mode arms live in the loop.
fn persist_dns_spec_inline(app: &App) -> Result<(), String> {
    let home = clash_verge_core::utils::dirs::app_home_dir().map_err(|e| e.to_string())?;
    crate::singbox::save_dns_spec(&home, &app.dns_spec_edit)
}

/// Task 7.2: a free-form clash rule string is parsed and inserted into the
/// edit buffer.
fn handle_rule_input_prompt(app: &mut App, code: KeyCode, buffer: ModeBuffer) {
    match code {
        KeyCode::Esc => app.input_mode = InputMode::Normal,
        KeyCode::Enter => {
            // Task 7.3: a `{...}` payload is parsed as a sing-box native
            // route rule (logical rules have no clash string form);
            // anything else goes through the clash string parser.
            let raw = buffer.text().trim().to_string();
            if !raw.is_empty() && app.rules_edit_mode {
                let parsed = if raw.starts_with('{') {
                    serde_json::from_str::<serde_json::Value>(&raw)
                        .ok()
                        .as_ref()
                        .and_then(crate::routing::from_singbox_json)
                        .ok_or_else(|| "invalid sing-box rule JSON".to_string())
                } else {
                    Ok(crate::routing::from_clash_rule_str(&raw))
                };
                match parsed {
                    Ok(parsed) => {
                        let at = (app.rules_selected_index + 1).min(app.rules_edit_buffer.len());
                        app.rules_edit_buffer.insert(at, parsed);
                        app.rules_edit_dirty = true;
                        app.rules_selected_index = at;
                        app.status_msg = Some(format!("rule inserted: {raw}"));
                    }
                    Err(error) => {
                        app.status_msg = Some(format!("rule rejected: {error}"));
                    }
                }
            }
            app.input_mode = InputMode::Normal;
        }
        KeyCode::Backspace => {
            app.input_mode = buffer.pop_char(1).unwrap_or(InputMode::Normal);
        }
        KeyCode::Char(c) => {
            app.input_mode = buffer.push_char(c);
        }
        _ => {}
    }
}

/// Task 7.4: `tag|remote|url` or `tag|local|path`.
fn handle_rule_set_prompt(app: &mut App, code: KeyCode, buffer: ModeBuffer) {
    match code {
        KeyCode::Esc => app.input_mode = InputMode::Normal,
        KeyCode::Enter => {
            let spec = buffer.text().trim().to_string();
            let parts: Vec<&str> = spec.split('|').map(str::trim).collect();
            if parts.len() == 3 && app.rules_edit_mode {
                let (tag, rtype, loc) = (parts[0], parts[1], parts[2]);
                let mut entry = serde_json::json!({
                    "type": rtype,
                    "tag": tag,
                });
                if rtype == "remote" {
                    entry["url"] = serde_json::json!(loc);
                    entry["format"] = serde_json::json!("binary");
                } else {
                    entry["path"] = serde_json::json!(loc);
                }
                if let Ok(home) = clash_verge_core::utils::dirs::app_home_dir() {
                    match crate::singbox::load_rule_sets(&home) {
                        Ok(mut sets) => {
                            sets.push(entry);
                            match crate::singbox::save_rule_sets(&home, &sets) {
                                Ok(()) => {
                                    app.rule_sets_edit = sets;
                                    app.rules_edit_dirty = true;
                                    app.status_msg = Some(format!("rule-set added: {tag}"));
                                }
                                Err(error) => app.status_msg = Some(format!("rule-set save failed: {error}")),
                            }
                        }
                        Err(error) => app.status_msg = Some(format!("rule-set load failed: {error}")),
                    }
                }
            } else {
                app.status_msg = Some("expected tag|remote|url or tag|local|path".into());
            }
            app.input_mode = InputMode::Normal;
        }
        KeyCode::Backspace => {
            app.input_mode = buffer.pop_char(1).unwrap_or(InputMode::Normal);
        }
        KeyCode::Char(c) => {
            app.input_mode = buffer.push_char(c);
        }
        _ => {}
    }
}

/// Task 7.2: structured rule form `kind=value>target`.
fn handle_rule_form_prompt(app: &mut App, code: KeyCode, buffer: ModeBuffer) {
    match code {
        KeyCode::Esc => app.input_mode = InputMode::Normal,
        KeyCode::Enter => {
            let spec = buffer.text().trim().to_string();
            if !spec.is_empty() && app.rules_edit_mode {
                match crate::routing::build_simple_rule(&spec) {
                    Ok(rule) => {
                        let at = (app.rules_selected_index + 1).min(app.rules_edit_buffer.len());
                        app.rules_edit_buffer.insert(at, rule);
                        app.rules_edit_dirty = true;
                        app.rules_selected_index = at;
                        app.status_msg = Some(format!("rule built: {spec}"));
                    }
                    Err(error) => {
                        app.status_msg = Some(format!("invalid form: {error}"));
                    }
                }
            }
            app.input_mode = InputMode::Normal;
        }
        KeyCode::Backspace => {
            app.input_mode = buffer.pop_char(1).unwrap_or(InputMode::Normal);
        }
        KeyCode::Char(c) => {
            app.input_mode = buffer.push_char(c);
        }
        _ => {}
    }
}

/// Task 8.1: `kind|tag|server|port|detour`.
fn handle_dns_server_prompt(app: &mut App, ctx: &Ctx, code: KeyCode, buffer: ModeBuffer) {
    match code {
        KeyCode::Esc => app.input_mode = InputMode::Normal,
        KeyCode::Enter => {
            let spec = buffer.text().trim().to_string();
            if !spec.is_empty() {
                match crate::singbox::dns::parse_server_spec(&spec) {
                    Ok(server) => {
                        let tag = server.tag.clone();
                        app.dns_spec_edit.servers.push(server);
                        match persist_dns_spec_inline(app) {
                            Ok(()) => {
                                app.status_msg = Some(format!("dns server added: {tag}"));
                            }
                            Err(error) => {
                                app.dns_spec_edit.servers.pop();
                                app.status_msg = Some(format!("persist dns: {error}"));
                            }
                        }
                    }
                    Err(error) => {
                        app.status_msg = Some(format!("invalid dns server: {error}"));
                    }
                }
            }
            app.input_mode = InputMode::Normal;
        }
        KeyCode::Backspace => {
            app.input_mode = buffer.pop_char(1).unwrap_or(InputMode::Normal);
        }
        KeyCode::Char(c) => {
            app.input_mode = buffer.push_char(c);
        }
        _ => {
            // Touch ctx so the lint is satisfied without affecting behavior.
            let _ = ctx;
        }
    }
}

/// Task 8.1: `tag|suffix=a,b|keyword=x|cidr=c`.
fn handle_dns_rule_prompt(app: &mut App, ctx: &Ctx, code: KeyCode, buffer: ModeBuffer) {
    match code {
        KeyCode::Esc => app.input_mode = InputMode::Normal,
        KeyCode::Enter => {
            let spec = buffer.text().trim().to_string();
            if !spec.is_empty() {
                match crate::singbox::dns::parse_rule_spec(&spec) {
                    Ok(rule) => {
                        let target = rule.server.clone();
                        app.dns_spec_edit.rules.push(rule);
                        match persist_dns_spec_inline(app) {
                            Ok(()) => {
                                app.status_msg = Some(format!("dns rule added -> {target}"));
                            }
                            Err(error) => {
                                app.dns_spec_edit.rules.pop();
                                app.status_msg = Some(format!("persist dns: {error}"));
                            }
                        }
                    }
                    Err(error) => {
                        app.status_msg = Some(format!("invalid dns rule: {error}"));
                    }
                }
            }
            app.input_mode = InputMode::Normal;
        }
        KeyCode::Backspace => {
            app.input_mode = buffer.pop_char(1).unwrap_or(InputMode::Normal);
        }
        KeyCode::Char(c) => {
            app.input_mode = buffer.push_char(c);
        }
        _ => {
            let _ = ctx;
        }
    }
}

/// Task 8.1: bootstrap resolver server tag (empty clears).
fn handle_dns_resolver_prompt(app: &mut App, ctx: &Ctx, code: KeyCode, buffer: ModeBuffer) {
    match code {
        KeyCode::Esc => app.input_mode = InputMode::Normal,
        KeyCode::Enter => {
            let tag = buffer.text().trim().to_string();
            if tag.is_empty() {
                app.dns_spec_edit.domain_resolver = None;
            } else {
                app.dns_spec_edit.domain_resolver = Some(tag.clone());
            }
            match persist_dns_spec_inline(app) {
                Ok(()) => {
                    app.status_msg = Some(if tag.is_empty() {
                        "dns resolver cleared".into()
                    } else {
                        format!("dns resolver set: {tag}")
                    });
                }
                Err(error) => {
                    app.status_msg = Some(format!("persist dns: {error}"));
                }
            }
            app.input_mode = InputMode::Normal;
        }
        KeyCode::Backspace => {
            app.input_mode = buffer.pop_char(1).unwrap_or(InputMode::Normal);
        }
        KeyCode::Char(c) => {
            app.input_mode = buffer.push_char(c);
        }
        _ => {
            let _ = ctx;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn render_failure_waits_for_cleanup_and_preserves_both_errors() {
        let cleaned = std::sync::atomic::AtomicBool::new(false);
        let result = finish_loop(Some(anyhow::anyhow!("draw failed")), async {
            cleaned.store(true, std::sync::atomic::Ordering::SeqCst);
            Err(anyhow::anyhow!("cleanup failed"))
        })
        .await;
        assert!(cleaned.load(std::sync::atomic::Ordering::SeqCst));
        let message = format!("{:#}", result.unwrap_err());
        assert!(message.contains("draw failed") && message.contains("cleanup failed"));
    }

    #[test]
    fn mode_buffer_pop_and_push_round_trip() {
        let buffer = ModeBuffer::RuleSet("a|b".to_string());
        let next = buffer.push_char('c');
        match next {
            InputMode::RuleSetInput(s) => assert_eq!(s, "a|bc"),
            _ => panic!("expected RuleSetInput"),
        }
        let popped = buffer.pop_char(1).unwrap();
        match popped {
            InputMode::RuleSetInput(s) => assert_eq!(s, "a|"),
            _ => panic!("expected RuleSetInput"),
        }
    }

    #[test]
    fn mode_buffer_clone_with_preserves_variant() {
        let buffer = ModeBuffer::DnsServer("hello".to_string());
        let next = buffer.clone_with("hello!".to_string()).unwrap();
        match next {
            InputMode::DnsServerInput(s) => assert_eq!(s, "hello!"),
            _ => panic!("expected DnsServerInput"),
        }
    }

    #[test]
    fn handle_singbox_input_mode_passes_through_normal_mode() {
        // No active input mode: handler must return `false` so the global
        // key map (handlers::handle_key) can dispatch the key normally.
        let mut app = App::new();
        app.input_mode = InputMode::Normal;
        assert!(!handle_singbox_input_mode(&mut app, &dummy_ctx(), KeyCode::Char('q')));
    }

    #[test]
    fn handle_singbox_input_mode_eats_chars_in_rule_set_prompt() {
        let mut app = App::new();
        app.input_mode = InputMode::RuleSetInput(String::new());
        assert!(handle_singbox_input_mode(&mut app, &dummy_ctx(), KeyCode::Char('a')));
        match &app.input_mode {
            InputMode::RuleSetInput(s) => assert_eq!(s, "a"),
            _ => panic!("expected RuleSetInput"),
        }
        // Backspace pops; Esc cancels.
        assert!(handle_singbox_input_mode(&mut app, &dummy_ctx(), KeyCode::Backspace));
        match &app.input_mode {
            InputMode::RuleSetInput(s) => assert_eq!(s, ""),
            _ => panic!("expected RuleSetInput"),
        }
        assert!(handle_singbox_input_mode(&mut app, &dummy_ctx(), KeyCode::Esc));
        assert!(matches!(app.input_mode, InputMode::Normal));
    }

    fn dummy_ctx() -> Ctx {
        let (tx, _rx) = tokio::sync::mpsc::channel(256);
        Ctx {
            manager: crate::mihomo_manager::MihomoManager::new(std::env::temp_dir()),
            tx: tx.into(),
            traffic_tx: tokio::sync::watch::channel(None).0,
            log_tx: tokio::sync::mpsc::channel(8).0,
            dropped_logs: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
            local_actions: std::sync::Arc::new(parking_lot::Mutex::new(handlers::LocalActionQueue::new())),
            guard: std::sync::Arc::new(tokio::sync::Mutex::new(TerminalGuard::detached())),
            keys: crate::tui::keymap::KeyMap::default(),
        }
    }
}
