use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph, Wrap};

use crate::app::App;

pub fn draw(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let panel = Paragraph::new(vec![
        Line::from(Span::styled(
            app.tr("unlock.limited"),
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        )),
        Line::from(app.tr("unlock.unavailable")),
        Line::from(Span::styled(
            app.tr("unlock.gui_workflow"),
            Style::default().fg(Color::DarkGray),
        )),
    ])
    .block(Block::bordered().title(app.tr("unlock.title")))
    .wrap(Wrap { trim: true });
    frame.render_widget(panel, area);
}
