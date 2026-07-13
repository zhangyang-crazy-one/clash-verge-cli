use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::app::{App, CoreState};

/// Draw the compact status strip at the top of the screen.
pub fn draw(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let (state_text, color) = match &app.core_state {
        CoreState::Running => (app.tr("status.running"), Color::Green),
        CoreState::Starting => (app.tr("status.starting"), Color::Yellow),
        CoreState::Stopped => (app.tr("status.stopped"), Color::Gray),
        CoreState::Error(_) => (app.tr("status.error"), Color::Red),
    };

    let mut spans = vec![
        Span::styled(
            " clash-verge-cli ",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(ratatui::style::Modifier::BOLD),
        ),
        Span::styled(
            format!(" {} ", app.view.localized_label(app.language)),
            Style::default().fg(Color::White),
        ),
        Span::styled(format!("● {state_text}"), Style::default().fg(color)),
    ];

    if let Some(profile) = app.profiles.get(app.selected_index)
        && let Some(name) = profile.name.as_deref()
    {
        spans.push(Span::styled(
            format!(" | {}: ", app.tr("status.profile")),
            Style::default().fg(Color::DarkGray),
        ));
        spans.push(Span::styled(
            crate::ui::terminal_text::display(name),
            Style::default().fg(Color::White),
        ));
    }

    if let Some(pid) = app.core_pid {
        spans.push(Span::styled(
            format!(" | pid: {pid}"),
            Style::default().fg(Color::DarkGray),
        ));
    }
    if let Some(ref version) = app.core_version {
        spans.push(Span::styled(
            format!(" | {version}"),
            Style::default().fg(Color::DarkGray),
        ));
    }
    if area.width >= 100 {
        let chain = if app.chain_mode {
            app.tr("status.on")
        } else {
            app.tr("status.off")
        };
        spans.push(Span::styled(
            format!(" | {}: ", app.tr("status.chain")),
            Style::default().fg(Color::DarkGray),
        ));
        spans.push(Span::styled(chain, Style::default().fg(Color::Yellow)));
    }
    if area.width >= 120
        && let Some(traffic) = &app.traffic
    {
        spans.push(Span::styled(
            format!(" | up: {} B/s", traffic.up),
            Style::default().fg(Color::Cyan),
        ));
        spans.push(Span::styled(
            format!(" down: {} B/s", traffic.down),
            Style::default().fg(Color::Magenta),
        ));
    }

    let line = Line::from(spans);
    let para = Paragraph::new(line).style(Style::default());
    frame.render_widget(para, area);
}
