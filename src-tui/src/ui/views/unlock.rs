use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Padding, Paragraph, Wrap};

use crate::app::App;
use crate::ui::theme;

pub fn draw(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let panel = Paragraph::new(vec![
        Line::from(Span::styled(app.tr("unlock.limited"), theme::bold(theme::warn()))),
        Line::from(app.tr("unlock.unavailable")),
        Line::from(Span::styled(
            app.tr("unlock.gui_workflow"),
            Style::new().fg(theme::dim()),
        )),
    ])
    .block(theme::panel_block(app.tr("unlock.title"), false).padding(Padding::horizontal(1)))
    .wrap(Wrap { trim: true });
    frame.render_widget(panel, area);
}
