use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, ListState, Padding, Paragraph, Wrap};

use crate::app::{App, Focus};
use crate::mihomo_api::types::ConnectionInfo;
use crate::ui::theme;

pub fn draw(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(62), Constraint::Percentage(38)])
        .split(area);
    let connections = filtered_connections(app);
    draw_list(frame, columns[0], app, &connections);
    draw_detail(frame, columns[1], app, &connections);
}

fn filtered_connections(app: &App) -> Vec<&ConnectionInfo> {
    let Some(query) = app.connection_filter.as_deref().filter(|query| !query.is_empty()) else {
        return app.connections.iter().collect();
    };
    let query = query.to_ascii_lowercase();
    app.connections
        .iter()
        .filter(|connection| {
            let host = connection
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.host.as_deref())
                .unwrap_or_default();
            let rule = connection.rule.as_deref().unwrap_or_default();
            connection.id.to_ascii_lowercase().contains(&query)
                || host.to_ascii_lowercase().contains(&query)
                || rule.to_ascii_lowercase().contains(&query)
        })
        .collect()
}

fn draw_list(frame: &mut Frame<'_>, area: Rect, app: &App, connections: &[&ConnectionInfo]) {
    let mut items: Vec<ListItem<'_>> = connections
        .iter()
        .map(|connection| {
            let host = connection
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.host.as_deref())
                .unwrap_or(app.tr("connections.unknown_host"));
            let rule = connection.rule.as_deref().unwrap_or("-");
            ListItem::new(Line::from(vec![
                Span::styled(host, Style::new().fg(theme::text())),
                Span::styled(format!("  {rule}"), Style::new().fg(theme::dim())),
            ]))
        })
        .collect();
    if items.is_empty() {
        let message = if let Some(error) = app.runtime_errors.connections.as_deref() {
            format!("connections request failed: {error}")
        } else if app.runtime_loading.connections {
            app.tr("connections.loading").to_string()
        } else if let Some(query) = app.connection_filter.as_deref() {
            format!("no connections match: {query}")
        } else {
            app.tr("connections.empty").to_string()
        };
        items.push(ListItem::new(Span::styled(message, Style::new().fg(theme::dim()))));
    }

    let selected = app
        .selected_connection_id
        .as_deref()
        .and_then(|id| connections.iter().position(|connection| connection.id == id));
    let mut state = ListState::default().with_selected(selected);
    let list = List::new(items)
        .block(theme::panel_block(
            app.tr("connections.title"),
            app.focus == Focus::Content,
        ))
        .highlight_style(theme::highlight(true))
        .highlight_symbol("> ");
    frame.render_stateful_widget(list, area, &mut state);
}

fn draw_detail(frame: &mut Frame<'_>, area: Rect, app: &App, connections: &[&ConnectionInfo]) {
    let selected = app
        .selected_connection_id
        .as_deref()
        .and_then(|id| connections.iter().find(|connection| connection.id == id).copied());
    let lines = if let Some(connection) = selected {
        let host = connection
            .metadata
            .as_ref()
            .and_then(|metadata| metadata.host.as_deref())
            .unwrap_or(app.tr("connections.unknown_host"));
        let network = connection
            .metadata
            .as_ref()
            .and_then(|metadata| metadata.network.as_deref())
            .unwrap_or(app.tr("common.unknown"));
        vec![
            Line::from(Span::styled(host, theme::bold(theme::accent()))),
            Line::from(format!("ID: {}", connection.id)),
            Line::from(format!("{}: {network}", app.tr("connections.network"))),
            Line::from(format!(
                "{}: {}",
                app.tr("connections.rule"),
                connection.rule.as_deref().unwrap_or("-")
            )),
            Line::from(
                app.tr("connections.transfer")
                    .replace("{up}", &connection.upload.to_string())
                    .replace("{down}", &connection.download.to_string()),
            ),
            Line::from(Span::styled(
                app.tr("connections.close_hint"),
                Style::new().fg(theme::danger()),
            )),
        ]
    } else {
        vec![
            Line::from(Span::styled(app.tr("connections.none"), Style::new().fg(theme::warn()))),
            Line::from(Span::styled(
                app.tr("connections.browse_filter"),
                Style::new().fg(theme::dim()),
            )),
        ]
    };
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                theme::panel_block(app.tr("connections.detail"), app.focus == Focus::Content)
                    .padding(Padding::horizontal(1)),
            )
            .wrap(Wrap { trim: true }),
        area,
    );
}
