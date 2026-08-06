use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::app::{App, InputMode, Overlay, View};
use crate::tui::input;

/// Draw the inline URL input bar at the bottom of the screen.
pub fn draw(frame: &mut Frame<'_>, area: Rect, app: &App) {
    if let Some(overlay) = app.overlay {
        match overlay {
            Overlay::Filter => {
                let query = app.filter.as_deref().unwrap_or_default();
                let line = Line::from(vec![
                    Span::styled(format!("{} ", app.tr("input.filter")), Style::default().fg(Color::Cyan)),
                    Span::styled(
                        app.view.localized_label(app.language),
                        Style::default().fg(Color::White),
                    ),
                    Span::styled(": ", Style::default().fg(Color::DarkGray)),
                    Span::styled(query, Style::default().fg(Color::White)),
                    Span::styled(app.tr("input.apply_cancel"), Style::default().fg(Color::DarkGray)),
                ]);
                frame.render_widget(Paragraph::new(line), area);
                return;
            }
            Overlay::CloseConfirmation => {
                let target = app
                    .pending_connection_close
                    .as_deref()
                    .unwrap_or(app.tr("common.unknown"));
                let line = Line::from(vec![
                    Span::styled(format!("{} ", app.tr("input.close")), Style::default().fg(Color::Red)),
                    Span::styled(target, Style::default().fg(Color::White)),
                    Span::styled(app.tr("input.confirm_cancel"), Style::default().fg(Color::DarkGray)),
                ]);
                frame.render_widget(Paragraph::new(line), area);
                return;
            }
            Overlay::CloseAllConnectionsConfirmation => {
                let line = Line::from(vec![
                    Span::styled("Close ALL connections? ", Style::default().fg(Color::Red)),
                    Span::styled("Enter = confirm | Esc/q = cancel", Style::default().fg(Color::DarkGray)),
                ]);
                frame.render_widget(Paragraph::new(line), area);
                return;
            }
            Overlay::Help => {}
        }
    }

    match &app.input_mode {
        InputMode::Normal => {
            let mut spans = vec![
                Span::styled(app.tr("input.views_focus"), Style::default().fg(Color::DarkGray)),
                Span::styled(" | ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    input::context_hint(app.view, app.focus, app.language),
                    Style::default().fg(Color::White),
                ),
            ];
            if matches!(app.view, View::Connections | View::Logs) {
                spans.push(Span::styled(" | / filter", Style::default().fg(Color::DarkGray)));
            }
            spans.push(Span::styled(
                app.tr("input.help_quit"),
                Style::default().fg(Color::DarkGray),
            ));
            frame.render_widget(Paragraph::new(Line::from(spans)), area);
        }
        InputMode::Importing(buffer) => {
            let line = Line::from(vec![
                Span::styled(app.tr("input.url"), Style::default().fg(Color::Cyan)),
                Span::styled(buffer.clone(), Style::default().fg(Color::White)),
                Span::styled("|", Style::default().fg(Color::DarkGray)),
                Span::styled(app.tr("input.submit_cancel"), Style::default().fg(Color::DarkGray)),
            ]);
            frame.render_widget(Paragraph::new(line), area);
        }
    }
}
