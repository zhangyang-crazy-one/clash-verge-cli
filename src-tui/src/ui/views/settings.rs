use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph, Wrap};

use crate::app::App;

pub const SETTINGS_ROW_COUNT: usize = 4;

pub fn draw(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(8), Constraint::Min(6)])
        .split(area);
    let mode = if app.clash_mode.is_empty() {
        app.core_config.get_mode().unwrap_or_else(|| app.tr("common.unknown").into())
    } else {
        app.clash_mode.clone()
    };
    let core = Paragraph::new(vec![
        Line::from(Span::styled(
            app.tr("settings.runtime_heading"),
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        )),
        Line::from(format!("{}: {}", app.tr("settings.core"), core_state_label(app))),
        Line::from(format!("{}: {}", app.tr("settings.profile_count"), app.profiles.len())),
        Line::from(format!("{}: {mode}", app.tr("settings.mihomo_mode"))),
        Line::from(format!(
            "{}: mixed {} | socks {} | http {}",
            app.tr("settings.ports"),
            app.core_config.get_mixed_port(),
            app.core_config.get_socks_port(),
            app.core_config.get_port()
        )),
        Line::from(format!("{}: {}", app.tr("settings.core_pid"), core_owner_label(app))),
    ])
    .block(Block::bordered().title(app.tr("settings.runtime")));
    frame.render_widget(core, rows[0]);

    let cursor = app.settings_selected_index.min(SETTINGS_ROW_COUNT - 1);
    let support = Paragraph::new(vec![
        Line::from(Span::styled(
            app.tr("settings.gui_config"),
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        )),
        settings_row(
            app,
            0,
            cursor,
            format!(
                "{}: {}",
                app.tr("settings.language"),
                app.language.display_name()
            ),
        ),
        settings_row(
            app,
            1,
            cursor,
            format!(
                "{}: {}",
                app.tr("settings.system_proxy"),
                enabled(app, app.gui_config.enable_system_proxy)
            ),
        ),
        settings_row(
            app,
            2,
            cursor,
            format!(
                "TUN: {}",
                enabled(app, app.gui_config.enable_tun_mode)
            ),
        ),
        settings_row(
            app,
            3,
            cursor,
            format!("{}: {mode}", app.tr("settings.mihomo_mode")),
        ),
        Line::from(format!(
            "{}: {}",
            app.tr("settings.proxy_host"),
            app.gui_config
                .proxy_host
                .as_deref()
                .unwrap_or(app.tr("settings.not_configured"))
        )),
        Line::from(Span::styled(
            app.tr("settings.writable_hint"),
            Style::default().fg(Color::DarkGray),
        )),
    ])
    .block(Block::bordered().title(app.tr("settings.system")))
    .wrap(Wrap { trim: true });
    frame.render_widget(support, rows[1]);
}

fn settings_row(app: &App, index: usize, cursor: usize, text: String) -> Line<'static> {
    let focused = app.focus == crate::app::Focus::Content && index == cursor;
    let style = if focused {
        Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::White)
    };
    let prefix = if focused { "> " } else { "  " };
    Line::from(Span::styled(format!("{prefix}{text}"), style))
}

fn core_owner_label(app: &App) -> String {
    match (app.core_pid, &app.core_state) {
        (Some(pid), _) => pid.to_string(),
        (None, crate::app::CoreState::Running) => app.tr("settings.gui_managed").into(),
        (None, _) => app.tr("settings.not_running").into(),
    }
}

fn enabled(app: &App, value: Option<bool>) -> &str {
    match value {
        Some(true) => app.tr("settings.on"),
        Some(false) => app.tr("settings.off"),
        None => app.tr("common.unknown"),
    }
}

fn core_state_label(app: &App) -> String {
    match &app.core_state {
        crate::app::CoreState::Running => app.tr("status.running").into(),
        crate::app::CoreState::Starting => app.tr("status.starting").into(),
        crate::app::CoreState::Stopped => app.tr("status.stopped").into(),
        crate::app::CoreState::Error(message) => format!("{}: {message}", app.tr("status.error")),
    }
}

#[cfg(test)]
mod tests {
    use super::core_owner_label;
    use crate::app::{App, CoreState};

    #[test]
    fn running_external_core_is_labeled_as_gui_managed() {
        let mut app = App::new();
        app.core_state = CoreState::Running;

        assert_eq!(core_owner_label(&app), "GUI-managed");
    }
}
