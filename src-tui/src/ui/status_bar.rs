use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::app::{App, CoreState};
use crate::ui::theme;

/// Draw the compact status strip at the top of the screen.
pub fn draw(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let (state_text, color) = match &app.core_state {
        CoreState::Running => (app.tr("status.running"), theme::ok()),
        CoreState::Starting => (app.tr("status.starting"), theme::warn()),
        CoreState::Stopped => (app.tr("status.stopped"), theme::dim()),
        CoreState::Error(_) => (app.tr("status.error"), theme::danger()),
    };

    let mut spans = vec![
        Span::styled(" clash-verge-cli ", theme::bold(theme::accent())),
        Span::styled(
            format!(" {} ", app.view.localized_label(app.language)),
            Style::new().fg(theme::text()),
        ),
        Span::styled(format!("● {state_text}"), Style::new().fg(color)),
    ];

    if let Some(profile) = app.profiles.get(app.selected_index)
        && let Some(name) = profile.name.as_deref()
    {
        spans.push(Span::styled(
            format!(" | {}: ", app.tr("status.profile")),
            Style::new().fg(theme::dim()),
        ));
        spans.push(Span::styled(
            crate::ui::terminal_text::display(name),
            Style::new().fg(theme::text()),
        ));
    }

    if let Some(pid) = app.core_pid {
        spans.push(Span::styled(format!(" | pid: {pid}"), Style::new().fg(theme::dim())));
    }
    if let Some(ref version) = app.core_version {
        spans.push(Span::styled(format!(" | {version}"), Style::new().fg(theme::dim())));
    }
    if area.width >= 100 {
        let chain = if app.chain_mode {
            app.tr("status.on")
        } else {
            app.tr("status.off")
        };
        spans.push(Span::styled(
            format!(" | {}: ", app.tr("status.chain")),
            Style::new().fg(theme::dim()),
        ));
        spans.push(Span::styled(chain, Style::new().fg(theme::warn())));
    }
    if area.width >= 120
        && let Some(traffic) = &app.traffic
    {
        spans.push(Span::styled(
            format!(" | up: {} B/s", traffic.up),
            Style::new().fg(theme::accent()),
        ));
        spans.push(Span::styled(
            format!(" down: {} B/s", traffic.down),
            Style::new().fg(theme::accent()),
        ));
    }

    let line = Line::from(spans);
    let para = Paragraph::new(line).style(Style::new());
    frame.render_widget(para, area);
}
