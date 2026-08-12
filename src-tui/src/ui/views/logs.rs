use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, ListState};

use crate::app::App;
use crate::mihomo_api::types::LogEntry;
use crate::ui::theme;

pub fn draw(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let logs = filtered_logs(app);
    let mut items: Vec<ListItem<'_>> = logs
        .iter()
        .map(|entry| {
            let color = severity_color(&entry.level);
            ListItem::new(Line::from(vec![
                Span::styled(
                    format!("{:>5} ", entry.level.to_ascii_uppercase()),
                    Style::new().fg(color).add_modifier(Modifier::BOLD),
                ),
                Span::raw(entry.payload.as_str()),
            ]))
        })
        .collect();
    if items.is_empty() {
        let message = if let Some(error) = app.runtime_errors.logs.as_deref() {
            format!("logs request failed: {error}")
        } else if app.runtime_loading.logs {
            app.tr("logs.loading").to_string()
        } else if let Some(query) = app.log_filter.as_deref() {
            format!("no logs match: {query}")
        } else {
            app.tr("logs.empty").to_string()
        };
        items.push(ListItem::new(Span::styled(message, Style::new().fg(theme::dim()))));
    }
    let selection = if logs.is_empty() {
        None
    } else {
        Some(app.log_selected_index.min(logs.len() - 1))
    };
    let mut state = ListState::default().with_selected(selection);
    let title = app
        .log_filter
        .as_deref()
        .map(|query| format!("{} [{}: {query}]", app.tr("logs.title"), app.tr("logs.filter")))
        .unwrap_or_else(|| app.tr("logs.title").to_string());
    let list = List::new(items)
        .block(theme::panel_block(title, app.focus == crate::app::Focus::Content))
        .highlight_style(theme::highlight(true))
        .highlight_symbol("> ");
    frame.render_stateful_widget(list, area, &mut state);
}

fn filtered_logs(app: &App) -> Vec<&LogEntry> {
    let Some(query) = app.log_filter.as_deref().filter(|query| !query.is_empty()) else {
        return app.logs.iter().collect();
    };
    let query = query.to_ascii_lowercase();
    app.logs
        .iter()
        .filter(|entry| {
            entry.level.to_ascii_lowercase().contains(&query) || entry.payload.to_ascii_lowercase().contains(&query)
        })
        .collect()
}

/// Severity colors for log levels. Delegates to the semantic palette so the
/// whole shell shares one accent family.
fn severity_color(level: &str) -> ratatui::style::Color {
    match level.to_ascii_lowercase().as_str() {
        "error" | "fatal" => theme::danger(),
        "warn" | "warning" => theme::warn(),
        "debug" | "trace" => theme::dim(),
        _ => theme::accent(),
    }
}
