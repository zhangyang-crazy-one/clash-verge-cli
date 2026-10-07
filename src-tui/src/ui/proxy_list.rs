use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, ListState};
use std::collections::HashMap;

use crate::app::{CoreState, ProxyDisplayRow};
use crate::i18n::{Language, tr};
use crate::ui::theme;

#[allow(clippy::too_many_arguments)]
pub fn draw(
    frame: &mut Frame<'_>,
    area: Rect,
    rows: &[ProxyDisplayRow],
    selected_flat_index: usize,
    delay_map: &HashMap<String, Option<u64>>,
    delay_keys: &HashMap<(String, String), Vec<crate::mihomo_api::types::ProxyDelayTarget>>,
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
                let display_name = crate::ui::terminal_text::display(name);
                let mut spans = vec![
                    Span::styled(format!("+ {display_name}"), theme::bold(theme::warn())),
                    Span::styled(
                        format!(" {node_count} {}", tr(language, "proxies.nodes")),
                        Style::new().fg(theme::dim()),
                    ),
                ];
                if current.is_some() {
                    spans.push(Span::styled(" *", Style::new().fg(theme::ok())));
                }
                items.push(ListItem::new(Line::from(spans)));
            }
            ProxyDisplayRow::Node { group, name, current } => {
                let display_name = crate::ui::terminal_text::display(name);
                let delay = target_delays(
                    delay_map,
                    delay_keys.get(&(group.clone(), name.clone())).map(Vec::as_slice),
                    name,
                    tr(language, "common.failed"),
                );
                let suffix = if *current { " selected" } else { "" };
                items.push(ListItem::new(Line::from(vec![
                    Span::styled(
                        format!("  {display_name}"),
                        if *current {
                            Style::new().fg(theme::ok())
                        } else {
                            Style::new()
                        },
                    ),
                    Span::styled(format!(" {delay}{suffix}"), Style::new().fg(theme::dim())),
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
        items.push(ListItem::new(Span::styled(status, Style::new().fg(theme::dim()))));
    }

    let selection = if rows.is_empty() {
        None
    } else {
        Some(selected_flat_index.min(rows.len() - 1))
    };
    let mut state = ListState::default().with_selected(selection);
    let list = List::new(items)
        .highlight_style(theme::highlight(true))
        .highlight_symbol("> ");

    frame.render_stateful_widget(list, area, &mut state);
}

/// Each provider identity keeps its own label and delay in a shared-name row.
pub(crate) fn target_delays(
    values: &HashMap<String, Option<u64>>,
    targets: Option<&[crate::mihomo_api::types::ProxyDelayTarget]>,
    fallback_name: &str,
    failed: &str,
) -> String {
    let delay = |key: &str| match values.get(key) {
        Some(Some(ms)) => format!("{ms}ms"),
        Some(None) => failed.to_owned(),
        None => "-".to_owned(),
    };
    match targets {
        Some(targets) => targets
            .iter()
            .map(|target| {
                if targets.len() > 1 {
                    format!(
                        "{}: {}",
                        crate::ui::terminal_text::display(&target.label),
                        delay(&target.key)
                    )
                } else {
                    delay(&target.key)
                }
            })
            .collect::<Vec<_>>()
            .join(" · "),
        None => delay(fallback_name),
    }
}

#[cfg(test)]
mod identity_tests {
    use super::*;
    #[test]
    fn one_shared_name_row_keeps_both_provider_outcomes() {
        let a = crate::services::proxy::provider_target("A", "JP");
        let b = crate::services::proxy::provider_target("B", "JP");
        let values = HashMap::from([(a.key.clone(), Some(41)), (b.key.clone(), None)]);
        let text = target_delays(&values, Some(&[a, b]), "JP", "failed");
        assert_eq!(text, "A/JP: 41ms · B/JP: failed");
    }
}
