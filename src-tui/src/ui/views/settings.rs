use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph, Wrap};

use crate::app::App;

pub fn draw(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(10), Constraint::Min(3)])
        .split(area);
    let mode = app
        .core_config
        .0
        .get("mode")
        .and_then(|value| value.as_str())
        .unwrap_or(app.tr("common.unknown"));
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
        Line::from(vec![
            Span::raw(format!("{}: ", app.tr("settings.language"))),
            Span::styled(
                app.language.display_name(),
                if app.focus == crate::app::Focus::Content {
                    Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::White)
                },
            ),
        ]),
        Line::from(Span::styled(
            app.tr("settings.change_language"),
            Style::default().fg(Color::DarkGray),
        )),
    ])
    .block(Block::bordered().title(app.tr("settings.runtime")));
    frame.render_widget(core, rows[0]);

    let support = Paragraph::new(vec![
        Line::from(Span::styled(
            app.tr("settings.gui_config"),
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        )),
        Line::from(format!(
            "{}: {} | TUN: {} | {}: {}",
            app.tr("settings.system_proxy"),
            enabled(app, app.gui_config.enable_system_proxy),
            enabled(app, app.gui_config.enable_tun_mode),
            app.tr("settings.dns_config"),
            enabled(app, app.gui_config.enable_dns_settings)
        )),
        Line::from(format!(
            "{}: {}",
            app.tr("settings.proxy_host"),
            app.gui_config
                .proxy_host
                .as_deref()
                .unwrap_or(app.tr("settings.not_configured"))
        )),
        Line::from(Span::styled(
            app.tr("settings.readonly_note"),
            Style::default().fg(Color::DarkGray),
        )),
    ])
    .block(Block::bordered().title(app.tr("settings.system")))
    .wrap(Wrap { trim: true });
    frame.render_widget(support, rows[1]);
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
