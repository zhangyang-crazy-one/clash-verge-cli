use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, ListState};
use std::collections::HashMap;

use crate::app::{CoreState, ProxyDisplayRow};
use crate::i18n::{Language, tr};

#[allow(clippy::too_many_arguments)]
pub fn draw(
    frame: &mut Frame<'_>,
    area: Rect,
    rows: &[ProxyDisplayRow],
    selected_flat_index: usize,
    delay_map: &HashMap<String, Option<u64>>,
    core_state: &CoreState,
    loading: bool,
    error: Option<&str>,
    language: Language,
) {
    let mut items: Vec<ListItem<'_>> = Vec::new();

    for row in rows {
        match row {
            ProxyDisplayRow::Group {
                name,
                current,
                node_count,
                ..
            } => {
                let mut spans = vec![
                    Span::styled(
                        format!("+ {name}"),
                        Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        format!(" {node_count} {}", tr(language, "proxies.nodes")),
                        Style::default().fg(Color::DarkGray),
                    ),
                ];
                if current.is_some() {
                    spans.push(Span::styled(" *", Style::default().fg(Color::Green)));
                }
                items.push(ListItem::new(Line::from(spans)));
            }
            ProxyDisplayRow::Node { name, current, .. } => {
                let display_name = crate::ui::terminal_text::display(name);
                let delay = match delay_map.get(name) {
                    Some(Some(milliseconds)) => format!("{milliseconds}ms"),
                    Some(None) => tr(language, "common.failed").to_string(),
                    None => "-".to_string(),
                };
                let suffix = if *current { " selected" } else { "" };
                items.push(ListItem::new(Line::from(vec![
                    Span::styled(
                        format!("  {display_name}"),
                        if *current {
                            Style::default().fg(Color::Green)
                        } else {
                            Style::default()
                        },
                    ),
                    Span::styled(format!(" {delay}{suffix}"), Style::default().fg(Color::DarkGray)),
                ])));
            }
        }
    }

    if items.is_empty() {
        let status = if let Some(error) = error {
            format!("proxy request failed: {error}")
        } else if loading {
            "loading proxies...".to_string()
        } else {
            match core_state {
                CoreState::Stopped => "mihomo is not running - press s to start".to_string(),
                CoreState::Starting => "connecting to mihomo...".to_string(),
                CoreState::Running => "no proxies - switch an active profile first".to_string(),
                CoreState::Error(error) => format!("mihomo error: {error}"),
            }
        };
        items.push(ListItem::new(Span::styled(
            status,
            Style::default().fg(Color::DarkGray),
        )));
    }

    let selection = if rows.is_empty() {
        None
    } else {
        Some(selected_flat_index.min(rows.len() - 1))
    };
    let mut state = ListState::default().with_selected(selection);
    let list = List::new(items)
        .highlight_style(Style::default().fg(Color::Cyan))
        .highlight_symbol("> ");

    frame.render_stateful_widget(list, area, &mut state);
}
