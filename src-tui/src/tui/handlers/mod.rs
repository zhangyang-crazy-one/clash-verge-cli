//! Action handlers for the interactive TUI, split by domain.
//!
//! The event loop owns the terminal and the `select!`; everything that turns
//! an input or a background result into a state change lives here:
//!
//! - [`handle_key`]: a terminal key press (text input, filter overlay, then
//!   the key map) becomes an intent;
//! - [`handle_intent`]: a user intent from the key map;
//! - [`handle_event`]: an action received on the action channel — results of
//!   spawned work, lifecycle notices from the mihomo manager, and intents the
//!   key path hands over.
//!
//! Handlers never block the loop on mihomo I/O: they spawn the work through
//! [`Ctx`] and receive its result as another action.

mod connections;
mod lifecycle;
mod navigation;
mod profile;
mod proxy;
mod rules;
mod settings;
mod tun;

use std::future::Future;
use std::sync::Arc;

use crossterm::event::{KeyEvent, MouseEvent};
use tokio::sync::mpsc::UnboundedSender;

use crate::app::{Action, App, Focus, InputMode, Overlay, View};
use crate::mihomo_manager::manager::MihomoManager;
use crate::tui::{TerminalGuard, input};

use navigation::key_context;
pub(crate) use navigation::view_filters;

pub(super) use profile::spawn_auto_update;

/// What the event loop does after a handler returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Flow {
    Continue,
    Quit,
}

/// Shared handles every handler needs.
pub(super) struct Ctx {
    pub manager: MihomoManager,
    pub tx: UnboundedSender<Action>,
    pub guard: Arc<tokio::sync::Mutex<TerminalGuard>>,
    /// Key remaps from `tui.yaml`.
    pub keys: crate::tui::keymap::KeyMap,
}

impl Ctx {
    /// Queue an action for the next loop iteration.
    fn send(&self, action: Action) {
        let _ = self.tx.send(action);
    }

    /// Run `task` in the background with its own sender.
    fn spawn<F, Fut>(&self, task: F)
    where
        F: FnOnce(UnboundedSender<Action>) -> Fut,
        Fut: Future<Output = ()> + Send + 'static,
    {
        tokio::spawn(task(self.tx.clone()));
    }

    /// Run `work` in the background and report its outcome as one action.
    fn spawn_result<T, E, Fut>(
        &self,
        work: Fut,
        on_ok: impl FnOnce(T) -> Action + Send + 'static,
        on_err: impl FnOnce(E) -> Action + Send + 'static,
    ) where
        Fut: Future<Output = Result<T, E>> + Send + 'static,
        T: Send + 'static,
        E: Send + 'static,
    {
        self.spawn(|tx| async move {
            let _ = tx.send(match work.await {
                Ok(value) => on_ok(value),
                Err(error) => on_err(error),
            });
        });
    }
}

/// Handle one key press.
pub(super) async fn handle_key(app: &mut App, ctx: &Ctx, key: KeyEvent) -> Flow {
    if let InputMode::Importing(buffer) = &app.input_mode {
        let buffer = buffer.clone();
        profile::import_input(app, ctx, buffer, key.code);
        return Flow::Continue;
    }
    if app.overlay == Some(Overlay::Filter) {
        navigation::filter_input(app, key);
        return Flow::Continue;
    }
    // Remaps apply to commands, never to typed text such as a password.
    let key = if app.overlay == Some(Overlay::PasswordInput) {
        key
    } else {
        match ctx.keys.translate(key) {
            Some(key) => key,
            None => return Flow::Continue,
        }
    };
    match input::map_key(key, key_context(app)) {
        Some(action) => handle_intent(app, ctx, action).await,
        None => Flow::Continue,
    }
}

/// Handle a mouse event (only reported with `mouse: true` in tui.yaml) on a
/// screen of `screen`: the wheel moves the selection of the pane under the
/// pointer, a click on the menu switches views, a click elsewhere focuses
/// the content.
pub(super) async fn handle_mouse(app: &mut App, ctx: &Ctx, event: MouseEvent, screen: ratatui::layout::Rect) -> Flow {
    use crossterm::event::{MouseButton, MouseEventKind};

    // Dialogs and prompts are keyboard-only.
    if app.overlay.is_some() || !matches!(app.input_mode, InputMode::Normal) {
        return Flow::Continue;
    }
    let areas = crate::ui::shell_areas(app, screen);
    let position = ratatui::layout::Position::new(event.column, event.row);
    let on_menu = areas.menu.contains(position);
    let on_content = areas.content.contains(position);
    match event.kind {
        MouseEventKind::ScrollDown | MouseEventKind::ScrollUp if on_menu || on_content => {
            app.focus = if on_menu { Focus::Menu } else { Focus::Content };
            let forward = event.kind == MouseEventKind::ScrollDown;
            if on_menu {
                // The menu follows the view, as j/k with the menu focused.
                navigation::move_selection(app, forward);
                navigation::switch_view(app, ctx, app.view);
            } else {
                navigation::move_selection(app, forward);
            }
        }
        MouseEventKind::Down(MouseButton::Left) if on_menu => {
            if let Some(view) = crate::ui::menu_view_at(app, screen, event.column, event.row) {
                app.focus = Focus::Menu;
                navigation::switch_view(app, ctx, view);
            }
        }
        MouseEventKind::Down(MouseButton::Left) if on_content => app.focus = Focus::Content,
        _ => {}
    }
    Flow::Continue
}

/// Handle a user intent produced by the key map.
async fn handle_intent(app: &mut App, ctx: &Ctx, action: Action) -> Flow {
    match action {
        Action::Quit => return Flow::Quit,
        Action::StartCore => lifecycle::start(app, ctx),
        Action::StopCore => lifecycle::stop(ctx),
        Action::RestartCore => lifecycle::restart(app, ctx),
        Action::StartImport => app.input_mode = InputMode::Importing(String::new()),
        Action::MoveNext => navigation::move_selection(app, true),
        Action::MovePrevious => navigation::move_selection(app, false),
        Action::Activate => activate(app, ctx).await,
        Action::SwitchView(view) => navigation::switch_view(app, ctx, view),
        Action::CycleFocus => navigation::cycle_focus(app),
        Action::FocusMenu => app.focus = Focus::Menu,
        Action::FocusContent => app.focus = Focus::Content,
        Action::StartFilter => navigation::start_filter(app),
        Action::ToggleHelp => navigation::toggle_help(app),
        Action::DismissOverlay => navigation::dismiss_overlay(app),
        Action::NodeDelayTest => proxy::test_selected_delay(app, ctx),
        Action::NodeDelayAll => proxy::test_all_delays(app, ctx),
        Action::ToggleChainMode => proxy::toggle_chain_mode(app),
        Action::CycleProxySort => proxy::cycle_sort(app),
        Action::ToggleHideFailedProxies => proxy::toggle_hide_failed(app),
        Action::ApplyChain => proxy::apply_chain(app, ctx),
        Action::ClearChain => proxy::clear_chain(app),
        Action::RequestCloseConnection => connections::begin_connection_close(app),
        Action::RequestCloseAllConnections => connections::request_close_all(app),
        Action::ConfirmCloseConnection(id) => connections::confirm_close_from_key(app, ctx, id),
        Action::UpdateProfile => profile::update_selected(app, ctx),
        Action::OpenEditor(target) => settings::open_editor(app, ctx, target).await,
        // Everything else (password and prompt keys, mode cycling, the
        // close-all confirmation, rules refresh) is handled exactly like the
        // same action arriving on the channel.
        other => return handle_event(app, ctx, other).await,
    }
    Flow::Continue
}

/// `Enter`: act on the selection of the focused view.
async fn activate(app: &mut App, ctx: &Ctx) {
    if app.focus == Focus::Menu {
        app.focus = Focus::Content;
        return;
    }
    match app.view {
        View::Profiles => profile::switch_selected(app, ctx),
        View::Proxies => proxy::activate_selected(app, ctx),
        View::Connections => connections::begin_connection_close(app),
        View::Rules => rules::update_selected_provider(app, ctx),
        View::Settings => settings::activate_row(app, ctx).await,
        _ => {}
    }
}

/// Handle an action received on the action channel.
pub(super) async fn handle_event(app: &mut App, ctx: &Ctx, action: Action) -> Flow {
    match action {
        Action::Quit => return Flow::Quit,

        // Core lifecycle.
        Action::CoreStarted {
            version,
            binary_path,
            binary_source,
        } => lifecycle::note_started(app, ctx, version, binary_path, binary_source),
        Action::CoreExited(0) => lifecycle::note_stopped(app),
        Action::CoreError(msg) => lifecycle::note_error(app, msg),
        Action::ResumeCoreStart { enable_tun } => lifecycle::resume_start(ctx, enable_tun),

        // Profiles and subscriptions.
        Action::ConfirmImport(url) => profile::confirm_import(ctx, url),
        Action::ImportNeedsTrust { url, host } => profile::begin_import_trust(app, url, host),
        Action::UpdateNeedsTrust { uid, host } => profile::begin_update_trust(app, uid, host),
        Action::ConfirmTrustImport => profile::handle_confirm_trust(app, &ctx.tx),
        Action::CancelTrustImport => profile::handle_cancel_trust(app),
        Action::ProfileImported => profile::note_imported(app, ctx).await,
        Action::ProfileImportFailed(error) => app.status_msg = Some(format!("Import failed: {error}")),
        Action::ProfileUpdated { uid, is_current } => profile::note_updated(app, ctx, uid, is_current).await,
        Action::ProfileUpdateFailed(error) => app.status_msg = Some(format!("Update failed: {error}")),
        Action::ProfileSwitched(uid) => {
            app.current_profile_uid = Some(uid);
            if app.core_state == crate::app::CoreState::Running {
                ctx.send(Action::ProxiesRefresh);
            }
        }

        // Proxies, delay tests, chains, and mode.
        Action::ProxiesRefresh if !app.runtime_loading.proxies => proxy::refresh(app, ctx),
        Action::ProxiesFetched(groups) => proxy::note_fetched(app, groups),
        Action::ProxiesFailed(error) => {
            app.runtime_loading.proxies = false;
            app.runtime_errors.proxies = Some(error);
        }
        Action::DelayResult(name, delay) => proxy::note_delay_result(app, name, delay),
        Action::DelayFailed(name, error) => proxy::note_delay_failed(app, name, error),
        Action::BatchDelayResult(name, delay) => proxy::note_batch_delay_result(app, name, delay),
        Action::BatchDelayFailed(name, error) => proxy::note_batch_delay_failed(app, name, error),
        Action::ChainApplied(nodes) => proxy::note_chain_applied(app, nodes),
        Action::ChainFailed(error) => app.status_msg = Some(format!("Chain not applied: {error}")),
        Action::CycleClashMode => proxy::cycle_clash_mode(app, ctx),
        Action::ModeChanged { mode, announce } => proxy::note_mode_changed(app, mode, announce).await,
        Action::ModeChangeFailed(error) => {
            app.status_msg = Some(format!("{}: {error}", app.tr("common.failed")));
        }

        // Traffic, connections, and logs.
        Action::TrafficRefresh if !app.runtime_loading.traffic => connections::refresh_traffic(app, ctx),
        Action::TrafficFetched(traffic) => {
            app.runtime_errors.traffic = None;
            app.traffic = Some(traffic);
        }
        Action::TrafficFailed(error) => {
            app.runtime_loading.traffic = false;
            app.runtime_errors.traffic = Some(error);
        }
        Action::ConnectionsRefresh if !app.runtime_loading.connections => connections::refresh_connections(app, ctx),
        Action::ConnectionsFetched(data) => connections::note_connections(app, data),
        Action::ConnectionsFailed(error) => {
            app.runtime_loading.connections = false;
            app.runtime_errors.connections = Some(error);
        }
        Action::ConfirmCloseConnection(id) if connections::close_confirmation_is_current(app, &id) => {
            connections::close_connection(app, ctx, id);
        }
        Action::ConnectionClosed(id) => {
            app.status_msg = Some(format!("Closed connection {id}"));
            app.selected_connection_id = None;
            ctx.send(Action::ConnectionsRefresh);
        }
        Action::CloseConnectionFailed { id, error } => {
            app.runtime_errors.connections = Some(format!("Could not close {id}: {error}"));
        }
        Action::ConfirmCloseAllConnections if app.overlay == Some(Overlay::CloseAllConnectionsConfirmation) => {
            connections::close_all(app, ctx);
        }
        Action::AllConnectionsClosed => {
            app.status_msg = Some("All connections closed".into());
            ctx.send(Action::ConnectionsRefresh);
        }
        Action::CloseAllConnectionsFailed(error) => app.status_msg = Some(format!("Close all failed: {error}")),
        Action::LogsRefresh if !app.runtime_loading.logs => connections::refresh_logs(app, ctx),
        Action::LogReceived(log) => connections::note_log(app, log),
        Action::LogsFailed(error) => {
            app.runtime_loading.logs = false;
            app.runtime_errors.logs = Some(error);
        }
        Action::CycleLogLevel => connections::cycle_log_level(app, ctx),
        Action::LogLevelChanged(level) => connections::note_log_level(app, ctx, level),
        Action::LogLevelFailed(error) => {
            app.status_msg = Some(format!("{}: {error}", app.tr("logs.level_failed")));
        }

        // Rules and rule providers.
        Action::RulesRefresh if !app.rules_loading => rules::refresh_rules(app, ctx),
        Action::RulesFetched(list) => rules::note_rules(app, list),
        Action::RulesFailed(error) => {
            app.rules_loading = false;
            app.rules_error = Some(error);
        }
        Action::RuleProvidersRefresh if !app.rule_providers_loading => rules::refresh_providers(app, ctx),
        Action::RuleProvidersFetched(providers) => rules::note_providers(app, providers),
        Action::RuleProvidersFailed(error) => {
            app.rule_providers_loading = false;
            app.rule_providers_error = Some(error);
        }
        Action::RuleProviderUpdated(name) => {
            app.status_msg = Some(format!("Rule provider updated: {name}"));
            ctx.send(Action::RuleProvidersRefresh);
        }
        Action::RuleProviderUpdateFailed { name, error } => {
            app.status_msg = Some(format!("Failed to update {name}: {error}"));
        }

        // TUN setup and the password popup.
        Action::TunSetupPrompt {
            binary,
            enable_tun,
            reason,
        } => tun::begin_tun_setup_confirm(app, binary, enable_tun, reason),
        Action::ConfirmTunSetup => tun::confirm_tun_setup(app),
        Action::SkipTunSetupStart => tun::skip_tun_setup_start(app, &ctx.tx),
        Action::TunSetupSucceeded { resume_start } => tun::note_tun_setup_succeeded(app, resume_start, &ctx.tx),
        Action::TunCapabilityState(privileged) => app.tun_privileged = privileged,
        Action::TunSetupRequested(binary) => tun::open_password_prompt(app, binary),
        Action::PasswordChar(c) => app.password_buffer.push(c),
        Action::PasswordBackspace => {
            app.password_buffer.pop();
        }
        Action::PasswordCancel => tun::handle_password_cancel(app),
        Action::PasswordSubmit => tun::handle_password_submit(app, &ctx.tx),

        Action::ProbeNotice(message) => app.status_msg = Some(message),
        _ => {}
    }
    Flow::Continue
}

#[cfg(test)]
mod tests {
    use crossterm::event::KeyCode;

    use super::*;

    fn ctx() -> (Ctx, tokio::sync::mpsc::UnboundedReceiver<Action>) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let ctx = Ctx {
            manager: MihomoManager::new(std::env::temp_dir()),
            tx,
            guard: Arc::new(tokio::sync::Mutex::new(TerminalGuard::detached())),
            keys: crate::tui::keymap::KeyMap::default(),
        };
        (ctx, rx)
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, crossterm::event::KeyModifiers::NONE)
    }

    #[tokio::test]
    async fn quit_key_ends_the_loop_and_other_keys_continue() {
        let (ctx, _rx) = ctx();
        let mut app = App::new();
        assert_eq!(
            handle_key(&mut app, &ctx, key(KeyCode::Char('j'))).await,
            Flow::Continue
        );
        assert_eq!(handle_key(&mut app, &ctx, key(KeyCode::Char('q'))).await, Flow::Quit);
    }

    #[tokio::test]
    async fn close_all_confirmation_key_starts_closing() {
        // Enter on the close-all overlay used to be dropped by the key path.
        let (ctx, _rx) = ctx();
        let mut app = App::new();
        app.view = View::Connections;
        app.focus = Focus::Content;
        handle_key(
            &mut app,
            &ctx,
            KeyEvent::new(KeyCode::Char('D'), crossterm::event::KeyModifiers::SHIFT),
        )
        .await;
        assert_eq!(app.overlay, Some(Overlay::CloseAllConnectionsConfirmation));

        handle_key(&mut app, &ctx, key(KeyCode::Enter)).await;
        assert_eq!(app.overlay, None);
        assert_eq!(app.status_msg.as_deref(), Some("Closing all connections..."));
    }

    #[tokio::test]
    async fn rules_refresh_key_starts_loading() {
        // `r` on the Rules view used to be dropped by the key path.
        let (ctx, _rx) = ctx();
        let mut app = App::new();
        app.view = View::Rules;
        handle_key(&mut app, &ctx, key(KeyCode::Char('r'))).await;
        assert!(app.rules_loading);
    }

    #[tokio::test]
    async fn import_mode_edits_the_buffer_and_enter_queues_the_import() {
        let (ctx, mut rx) = ctx();
        let mut app = App::new();
        app.input_mode = InputMode::Importing(String::new());
        for c in "ab".chars() {
            handle_key(&mut app, &ctx, key(KeyCode::Char(c))).await;
        }
        handle_key(&mut app, &ctx, key(KeyCode::Backspace)).await;
        assert!(matches!(&app.input_mode, InputMode::Importing(buffer) if buffer == "a"));

        handle_key(&mut app, &ctx, key(KeyCode::Enter)).await;
        assert!(matches!(app.input_mode, InputMode::Normal));
        assert!(matches!(rx.try_recv(), Ok(Action::ConfirmImport(url)) if url == "a"));
    }

    #[tokio::test]
    async fn filter_overlay_collects_text_and_submit_applies_it() {
        let (ctx, _rx) = ctx();
        let mut app = App::new();
        app.view = View::Logs;
        handle_key(&mut app, &ctx, key(KeyCode::Char('/'))).await;
        for c in "err".chars() {
            handle_key(&mut app, &ctx, key(KeyCode::Char(c))).await;
        }
        handle_key(&mut app, &ctx, key(KeyCode::Enter)).await;
        assert_eq!(app.log_filter.as_deref(), Some("err"));
        assert_eq!(app.overlay, None);
    }

    fn rule(payload: &str) -> crate::mihomo_api::types::Rule {
        crate::mihomo_api::types::Rule {
            rule_type: "DOMAIN".to_string(),
            payload: payload.to_string(),
            proxy: "Proxy".to_string(),
            size: None,
        }
    }

    async fn type_filter(app: &mut App, ctx: &Ctx, text: &str) {
        handle_key(app, ctx, key(KeyCode::Char('/'))).await;
        // Clear the prefilled filter first.
        for _ in 0..app.filter.as_deref().map_or(0, str::len) {
            handle_key(app, ctx, key(KeyCode::Backspace)).await;
        }
        for c in text.chars() {
            handle_key(app, ctx, key(KeyCode::Char(c))).await;
        }
        handle_key(app, ctx, key(KeyCode::Enter)).await;
    }

    #[tokio::test]
    async fn rules_view_moves_through_the_filtered_rules() {
        let (ctx, _rx) = ctx();
        let mut app = App::new();
        app.view = View::Rules;
        app.focus = Focus::Content;
        app.rules = vec![rule("a.example"), rule("b.test"), rule("c.example")];

        handle_key(&mut app, &ctx, key(KeyCode::Char('j'))).await;
        assert_eq!(app.rules_selected_index, 1, "j moves on Rules");

        type_filter(&mut app, &ctx, "example").await;
        assert_eq!(app.rule_filter.as_deref(), Some("example"));
        assert_eq!(app.rules_selected_index, 0);
        handle_key(&mut app, &ctx, key(KeyCode::Char('j'))).await;
        handle_key(&mut app, &ctx, key(KeyCode::Char('j'))).await;
        assert_eq!(app.rules_selected_index, 0, "wraps within the 2 matches");
        assert_eq!(app.visible_rules()[1].payload, "c.example");

        // Reopening the prompt shows the current filter.
        handle_key(&mut app, &ctx, key(KeyCode::Char('/'))).await;
        assert_eq!(app.filter.as_deref(), Some("example"));
    }

    #[tokio::test]
    async fn profile_filter_moves_over_matches_and_hides_the_selection_from_u() {
        let (ctx, _rx) = ctx();
        let mut app = App::new();
        app.view = View::Profiles;
        app.focus = Focus::Content;
        app.profiles = ["Work HK", "Home", "Work JP"]
            .iter()
            .map(|name| clash_verge_core::config::PrfItem {
                uid: Some(format!("uid-{name}").into()),
                name: Some((*name).into()),
                ..Default::default()
            })
            .collect();
        app.selected_index = 1;

        type_filter(&mut app, &ctx, "work").await;
        assert_eq!(app.selected_index, 0, "the hidden selection moves to the first match");
        handle_key(&mut app, &ctx, key(KeyCode::Char('j'))).await;
        assert_eq!(app.selected_index, 2, "skips the hidden profile");

        type_filter(&mut app, &ctx, "nothing").await;
        handle_key(&mut app, &ctx, key(KeyCode::Char('u'))).await;
        assert_eq!(app.status_msg.as_deref(), Some("No profile selected"));
    }

    #[tokio::test]
    async fn sorting_and_hiding_keep_the_cursor_on_the_same_node() {
        let (ctx, _rx) = ctx();
        let mut app = App::new();
        app.view = View::Proxies;
        app.focus = Focus::Content;
        app.proxy_groups.insert(
            "Proxy".to_string(),
            crate::mihomo_api::types::ProxyGroup {
                group_type: "Selector".to_string(),
                now: Some("b".to_string()),
                all: Some(vec!["c".to_string(), "a".to_string(), "b".to_string()]),
                history: None,
            },
        );
        app.expanded_proxy_group = Some("Proxy".to_string());
        app.delay_map.insert("a".to_string(), None);
        app.node_selected_index = 3; // Group row, then c, a, b.
        assert_eq!(proxy::selected_node(&app).map(|(_, node)| node).as_deref(), Some("b"));

        handle_key(&mut app, &ctx, key(KeyCode::Char('o'))).await;
        handle_key(&mut app, &ctx, key(KeyCode::Char('o'))).await;
        assert_eq!(app.proxy_sort, crate::app::ProxySort::Name);
        assert_eq!(app.node_selected_index, 2, "a, b, c: b is second");

        handle_key(
            &mut app,
            &ctx,
            KeyEvent::new(KeyCode::Char('H'), crossterm::event::KeyModifiers::SHIFT),
        )
        .await;
        assert!(app.hide_failed_proxies);
        assert_eq!(proxy::selected_node(&app).map(|(_, node)| node).as_deref(), Some("b"));
        assert_eq!(app.proxy_rows().len(), 3, "the failed node a is hidden");
    }

    #[tokio::test]
    async fn log_level_needs_a_running_core() {
        let (ctx, _rx) = ctx();
        let mut app = App::new();
        app.view = View::Logs;
        handle_key(
            &mut app,
            &ctx,
            KeyEvent::new(KeyCode::Char('L'), crossterm::event::KeyModifiers::SHIFT),
        )
        .await;
        assert_eq!(app.log_level, "info");
        assert_eq!(
            app.status_msg.as_deref(),
            Some("Start the core to change its log level")
        );
    }

    fn mouse(kind: crossterm::event::MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: crossterm::event::KeyModifiers::NONE,
        }
    }

    #[tokio::test]
    async fn mouse_clicks_the_menu_and_scrolls_the_pane_under_the_pointer() {
        use crossterm::event::{MouseButton, MouseEventKind};

        let (ctx, _rx) = ctx();
        let mut app = App::new();
        let screen = ratatui::layout::Rect::new(0, 0, 120, 32);
        // Status bar (row 0), menu border (row 1), then one row per view.
        handle_mouse(
            &mut app,
            &ctx,
            mouse(MouseEventKind::Down(MouseButton::Left), 3, 2 + 4),
            screen,
        )
        .await;
        assert_eq!(app.view, View::Rules);

        app.rules = vec![rule("a"), rule("b"), rule("c")];
        handle_mouse(&mut app, &ctx, mouse(MouseEventKind::ScrollDown, 60, 10), screen).await;
        assert_eq!(app.focus, Focus::Content);
        assert_eq!(app.rules_selected_index, 1);

        handle_mouse(&mut app, &ctx, mouse(MouseEventKind::ScrollDown, 3, 10), screen).await;
        assert_eq!(
            (app.focus, app.view),
            (Focus::Menu, View::Logs),
            "wheel on the menu changes view"
        );

        // Dialogs are keyboard-only.
        app.overlay = Some(Overlay::Help);
        handle_mouse(
            &mut app,
            &ctx,
            mouse(MouseEventKind::Down(MouseButton::Left), 3, 2),
            screen,
        )
        .await;
        assert_eq!(app.view, View::Logs);
    }

    #[tokio::test]
    async fn remapped_keys_act_as_their_target_but_not_while_typing_a_password() {
        let (mut ctx, _rx) = ctx();
        ctx.keys = crate::tui::keymap::TuiConfig::parse_for_test("keys:\n  x: j\n  q: none\n").keys;
        let mut app = App::new();
        app.focus = Focus::Menu;

        handle_key(&mut app, &ctx, key(KeyCode::Char('x'))).await;
        assert_eq!(app.view, View::Proxies, "x moved the menu like j");
        assert_eq!(
            handle_key(&mut app, &ctx, key(KeyCode::Char('q'))).await,
            Flow::Continue
        );

        app.overlay = Some(Overlay::PasswordInput);
        handle_key(&mut app, &ctx, key(KeyCode::Char('x'))).await;
        assert_eq!(app.password_buffer, ['x']);
    }

    #[tokio::test]
    async fn delay_results_do_not_move_the_cursor_to_another_node() {
        let (ctx, _rx) = ctx();
        let mut app = App::new();
        app.view = View::Proxies;
        app.proxy_sort = crate::app::ProxySort::Delay;
        app.proxy_groups.insert(
            "Proxy".to_string(),
            crate::mihomo_api::types::ProxyGroup {
                group_type: "Selector".to_string(),
                now: Some("a".to_string()),
                all: Some(vec!["a".to_string(), "b".to_string()]),
                history: None,
            },
        );
        app.expanded_proxy_group = Some("Proxy".to_string());
        app.node_selected_index = 1; // a (both untested: profile order)

        // b becomes the fastest and moves to the top.
        handle_event(&mut app, &ctx, Action::BatchDelayResult("b".to_string(), Some(10))).await;
        assert_eq!(proxy::selected_node(&app).map(|(_, node)| node).as_deref(), Some("a"));
        handle_event(
            &mut app,
            &ctx,
            Action::DelayFailed("a".to_string(), "timeout".to_string()),
        )
        .await;
        assert_eq!(proxy::selected_node(&app).map(|(_, node)| node).as_deref(), Some("a"));
    }
}
