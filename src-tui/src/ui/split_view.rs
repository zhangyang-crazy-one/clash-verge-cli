use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::ui::theme;

/// A two-pane workspace with one outer border and one shared divider.
///
/// Rendering adjacent full `Block`s produces duplicate vertical borders. Keeping
/// the frame ownership here makes the separator a single stable terminal cell.
pub struct SplitViewAreas {
    pub left: Rect,
    pub right: Rect,
}

pub fn draw(
    frame: &mut Frame<'_>,
    area: Rect,
    title: &str,
    left_title: &str,
    right_title: &str,
    left_percent: u16,
    focused: bool,
) -> SplitViewAreas {
    let outer = theme::panel_block(title, focused);
    let inner = outer.inner(area);
    frame.render_widget(outer, area);

    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(left_percent),
            Constraint::Length(1),
            Constraint::Min(1),
        ])
        .split(inner);

    let header_style = theme::bold(theme::accent());
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(left_title, header_style))),
        header_area(columns[0]),
    );
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(right_title, header_style))),
        header_area(columns[2]),
    );
    frame.render_widget(Block::new().borders(Borders::LEFT), columns[1]);

    SplitViewAreas {
        left: body_area(columns[0]),
        right: body_area(columns[2]),
    }
}

fn header_area(area: Rect) -> Rect {
    Rect {
        height: area.height.min(1),
        ..area
    }
}

fn body_area(area: Rect) -> Rect {
    Rect {
        y: area.y.saturating_add(1),
        height: area.height.saturating_sub(1),
        ..area
    }
}
