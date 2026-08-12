//! Centered content-driven dialogs: rounded borders, semantic kind styling,
//! and a height that follows the rendered line count instead of a fixed popup.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::{Block, BorderType, Clear, Padding, Paragraph, Wrap};

use crate::ui::theme;

/// Visual category of a dialog; drives the title accent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DialogKind {
    /// Generic informational dialog (help).
    Info,
    /// Warning confirmation (SSRF trust, destructive hints).
    Warn,
    /// Destructive confirmation (close connection / close all).
    Danger,
    /// Password / authentication prompt.
    Password,
}

/// Rounded dialog block with a kind-styled title. Dialogs are the only
/// surfaces that use rounded borders.
pub fn dialog_block<'a, T: Into<Line<'a>>>(kind: DialogKind, title: T) -> Block<'a> {
    let title_style = match kind {
        DialogKind::Info => theme::bold(theme::accent()),
        DialogKind::Warn | DialogKind::Password => theme::bold(theme::warn()),
        DialogKind::Danger => theme::bold(theme::danger()),
    };
    Block::bordered()
        .border_type(BorderType::Rounded)
        .title(title)
        .title_style(title_style)
}

/// Centered dialog rect: width is a fixed share of the terminal, height follows
/// `line_count` (borders included) and is clamped to the screen.
pub fn dialog_area(area: Rect, width: u16, line_count: usize) -> Rect {
    let height = (line_count as u16 + 2).clamp(3, area.height.saturating_sub(2).max(3));
    centered_rect(width, height, area)
}

/// Render a dialog centered over `area`. Returns the rect the dialog occupied.
pub fn draw_dialog<'a>(
    frame: &mut Frame<'_>,
    area: Rect,
    kind: DialogKind,
    title: &'a str,
    content: Vec<Line<'a>>,
) -> Rect {
    let width = (area.width * 70 / 100).max(30);
    // The paragraph wraps inside the block: 2 border + 2 horizontal padding,
    // so the effective wrap width is 4 less than the dialog width.
    let inner_width = width.saturating_sub(4);
    let line_count = wrapped_line_count(&content, inner_width);
    let rect = dialog_area(area, width, line_count);
    let paragraph = Paragraph::new(content)
        .wrap(Wrap { trim: true })
        .block(dialog_block(kind, title).padding(Padding::horizontal(1)));
    frame.render_widget(Clear, rect);
    frame.render_widget(paragraph, rect);
    rect
}

/// Number of rendered lines when `lines` wrap at `width` (hard-wrap ceil).
///
/// `Paragraph::line_count` exists but is gated behind the `rendered-line-info`
/// feature, so the shell computes the same value with `Line::width()` — which
/// accounts for double-width CJK cells. Blank lines still occupy one row.
pub fn wrapped_line_count(lines: &[Line<'_>], width: u16) -> usize {
    if width < 1 {
        return lines.len();
    }
    let width = usize::from(width);
    lines
        .iter()
        .map(|line| {
            let line_width = line.width();
            if line_width == 0 { 1 } else { line_width.div_ceil(width) }
        })
        .sum()
}

/// Masked password display string (terminal-safe bullet, same glyph the
/// askpass subprocess echoes while typing).
pub fn mask_password(len: usize) -> String {
    "•".repeat(len)
}

fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    Rect { x, y, width, height }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Color;
    use ratatui::{Terminal, backend::TestBackend};

    fn render_block(kind: DialogKind, title: &str) -> (String, Vec<(u16, u16, String, Color)>) {
        let backend = TestBackend::new(40, 10);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| {
                let block = dialog_block(kind, title);
                frame.render_widget(block, Rect::new(5, 2, 20, 6));
            })
            .expect("draw block");
        let buffer = terminal.backend().buffer();
        let cells = (0..10)
            .flat_map(|row| (0..40).map(move |col| (col, row)))
            .filter(|&(col, row)| buffer[(col, row)].symbol() != " ")
            .map(|(col, row)| (col, row, buffer[(col, row)].symbol().to_string(), buffer[(col, row)].fg))
            .collect();
        (buffer.content().iter().map(|cell| cell.symbol()).collect(), cells)
    }

    /// First rendered title cell: the top border row starts one column inside
    /// the block, so (x + 1, y) holds the leading title grapheme.
    fn first_title_cell(cells: &[(u16, u16, String, Color)]) -> &(u16, u16, String, Color) {
        cells
            .iter()
            .find(|(col, row, _, _)| *col == 6 && *row == 2)
            .expect("title cell")
    }

    #[test]
    fn dialog_block_renders_rounded_corners_for_every_kind() {
        for kind in [
            DialogKind::Info,
            DialogKind::Warn,
            DialogKind::Danger,
            DialogKind::Password,
        ] {
            let (rendered, _) = render_block(kind, "title");
            assert!(rendered.contains('╭'), "{kind:?} must use rounded top-left");
            assert!(rendered.contains('╮'), "{kind:?} must use rounded top-right");
            assert!(rendered.contains('╰'), "{kind:?} must use rounded bottom-left");
            assert!(rendered.contains('╯'), "{kind:?} must use rounded bottom-right");
        }
    }

    #[test]
    fn dialog_kind_styles_its_title_semantically() {
        let (_, danger_cells) = render_block(DialogKind::Danger, "Close All");
        assert_eq!(first_title_cell(&danger_cells).3, theme::danger());

        let (_, password_cells) = render_block(DialogKind::Password, "Administrator privileges");
        assert_eq!(first_title_cell(&password_cells).3, theme::warn());

        let (_, info_cells) = render_block(DialogKind::Info, "Help");
        assert_eq!(first_title_cell(&info_cells).3, theme::accent());
    }

    #[test]
    fn dialog_area_height_follows_content_and_clamps_to_screen() {
        let area = Rect::new(0, 0, 120, 32);
        // 4 content lines + 2 borders = 6.
        let small = dialog_area(area, 80, 4);
        assert_eq!(small.height, 6);
        assert_eq!(small.x, 20);
        assert_eq!(small.y, 13);

        // A tall dialog is clamped inside the screen.
        let tall = dialog_area(area, 80, 100);
        assert_eq!(tall.height, 30);
        assert_eq!(tall.y, 1);

        // A tiny terminal still yields a usable dialog.
        let tiny = dialog_area(Rect::new(0, 0, 40, 5), 30, 100);
        assert!(tiny.height >= 3);
    }

    #[test]
    fn wrapped_line_count_counts_rows_for_wrap_and_cjk_width() {
        assert_eq!(wrapped_line_count(&[Line::from("hello world")], 80), 1);
        assert_eq!(wrapped_line_count(&[Line::from("hello world")], 6), 2);
        assert_eq!(wrapped_line_count(&[Line::from(""), Line::from("x")], 80), 2);
        // "连接" is 4 cells wide, so it wraps in a 3-cell box.
        assert_eq!(wrapped_line_count(&[Line::from("连接")], 3), 2);
        assert_eq!(wrapped_line_count(&[], 80), 0);
    }

    #[test]
    fn draw_dialog_sizes_height_for_wrap_after_borders_and_padding() {
        // An 81-cell unbroken line wraps to 2 rows at the real inner width
        // (dialog 84 - 2 border - 2 padding = 80) but to 1 row at width-2.
        // The height must be computed at width-4 or the last wrapped row clips.
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        let long_line = "x".repeat(81);
        terminal
            .draw(|frame| {
                draw_dialog(
                    frame,
                    frame.area(),
                    DialogKind::Info,
                    "T",
                    vec![Line::from(long_line.clone())],
                );
            })
            .expect("draw dialog");
        let x_count = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .filter(|cell| cell.symbol() == "x")
            .count();
        assert_eq!(x_count, 81, "wrapped last row must not be clipped");
    }

    #[test]
    fn mask_password_uses_the_askpass_bullet() {
        assert_eq!(mask_password(0), "");
        assert_eq!(mask_password(4), "••••");
    }
}
