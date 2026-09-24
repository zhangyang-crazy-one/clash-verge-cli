use ratatui::Frame;
use ratatui::layout::{Constraint, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Cell, Padding, Paragraph, Row, Table, Wrap};

use crate::app::App;
use crate::services::unlock::{Service, Verdict};
use crate::ui::theme;

pub fn draw(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let block = theme::panel_block(app.tr("unlock.title"), false).padding(Padding::horizontal(1));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let mut header = vec![header_line(app)];
    if let Some(error) = app.unlock.error.as_deref() {
        header.push(Line::from(Span::styled(
            crate::ui::terminal_text::display(error),
            Style::new().fg(theme::danger()),
        )));
    }
    header.push(Line::from(Span::styled(
        app.tr("unlock.via"),
        Style::new().fg(theme::dim()),
    )));
    let header_height = u16::try_from(header.len() + 1).unwrap_or(u16::MAX);
    frame.render_widget(
        Paragraph::new(header).wrap(Wrap { trim: true }),
        Rect {
            height: header_height.min(inner.height),
            ..inner
        },
    );

    let table_area = Rect {
        y: inner.y.saturating_add(header_height),
        height: inner.height.saturating_sub(header_height),
        ..inner
    };
    let rows = Service::ALL.iter().map(|service| {
        let result = app
            .unlock
            .report
            .as_ref()
            .and_then(|report| report.results.iter().find(|result| result.service == *service));
        let (status, color) = match (app.unlock.running, result) {
            (true, _) => (app.tr("unlock.checking"), theme::dim()),
            (false, Some(result)) => (app.tr(result.verdict.label_key()), verdict_color(result.verdict)),
            (false, None) => ("-", theme::dim()),
        };
        let region = result
            .and_then(|result| result.region.clone())
            .unwrap_or_else(|| "-".into());
        let detail = result.and_then(|result| result.detail.clone()).unwrap_or_default();
        Row::new(vec![
            Cell::from(service.name()),
            Cell::from(Span::styled(status, Style::new().fg(color))),
            Cell::from(region),
            Cell::from(Span::styled(
                crate::ui::terminal_text::display(&detail),
                Style::new().fg(theme::dim()),
            )),
        ])
    });
    let table = Table::new(
        rows,
        [
            Constraint::Length(16),
            Constraint::Length(16),
            Constraint::Length(8),
            Constraint::Min(10),
        ],
    )
    .header(
        Row::new(vec![
            app.tr("unlock.service"),
            app.tr("unlock.status"),
            app.tr("unlock.region"),
            app.tr("unlock.detail"),
        ])
        .style(theme::bold(theme::accent())),
    );
    frame.render_widget(table, table_area);
}

/// `Exit: Proxy → Tokyo 01 · checked at 12:00`, or how to start.
fn header_line(app: &App) -> Line<'static> {
    let Some(report) = app.unlock.report.as_ref() else {
        return Line::from(Span::styled(
            app.tr("unlock.press_r").to_string(),
            Style::new().fg(theme::text()),
        ));
    };
    let exit = if report.exit.is_empty() {
        "-".to_string()
    } else {
        report
            .exit
            .iter()
            .map(|name| crate::ui::terminal_text::display(name))
            .collect::<Vec<_>>()
            .join(" → ")
    };
    let checked = app
        .unlock
        .checked_at
        .map(|time| format!(" · {} {}", app.tr("unlock.checked_at"), time.format("%H:%M:%S")))
        .unwrap_or_default();
    Line::from(vec![
        Span::styled(format!("{}: ", app.tr("unlock.exit")), Style::new().fg(theme::dim())),
        Span::styled(exit, Style::new().fg(theme::ok())),
        Span::styled(checked, Style::new().fg(theme::dim())),
    ])
}

fn verdict_color(verdict: Verdict) -> ratatui::style::Color {
    match verdict {
        Verdict::Available => theme::ok(),
        Verdict::OriginalsOnly => theme::warn(),
        Verdict::Unavailable => theme::danger(),
        Verdict::Failed => theme::dim(),
    }
}
