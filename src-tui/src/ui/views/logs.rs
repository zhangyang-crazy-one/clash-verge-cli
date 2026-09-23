use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, ListState};

use crate::app::App;
use crate::ui::theme;

pub fn draw(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let logs = app.visible_logs();
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
    let mut title = format!("{} [{}: {}]", app.tr("logs.title"), app.tr("logs.level"), app.log_level);
    if let Some(query) = crate::app::filter::active(app.log_filter.as_deref()) {
        title.push_str(&format!(
            " [{}: {}]",
            app.tr("logs.filter"),
            crate::ui::terminal_text::display(query)
        ));
    }
    let list = List::new(items)
        .block(theme::panel_block(title, app.focus == crate::app::Focus::Content))
        .highlight_style(theme::highlight(true))
        .highlight_symbol("> ");
    frame.render_stateful_widget(list, area, &mut state);
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
