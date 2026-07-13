use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph, Wrap};

use crate::app::App;

pub fn draw(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(5), Constraint::Min(3)])
        .split(area);
    let summary = Paragraph::new(vec![
        Line::from(Span::styled(
            app.tr("rules.limited"),
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            app.tr("rules.inventory"),
            Style::default().fg(Color::DarkGray),
        )),
        Line::from(Span::styled(
            app.tr("rules.cached"),
            Style::default().fg(Color::DarkGray),
        )),
    ])
    .block(Block::bordered().title(app.tr("rules.title")));
    frame.render_widget(summary, rows[0]);

    let rules: Vec<_> = app
        .connections
        .iter()
        .filter_map(|connection| connection.rule.as_deref())
        .take(12)
        .map(|rule| Line::from(Span::styled(rule, Style::default().fg(Color::Cyan))))
        .collect();
    let body = if rules.is_empty() {
        vec![Line::from(Span::styled(
            app.tr("rules.empty"),
            Style::default().fg(Color::DarkGray),
        ))]
    } else {
        rules
    };
    frame.render_widget(
        Paragraph::new(body)
            .block(Block::bordered().title(app.tr("rules.recent")))
            .wrap(Wrap { trim: true }),
        rows[1],
    );
}
