use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, ListState, Paragraph, Wrap};

use crate::app::{App, Focus};
use crate::ui::theme;

pub fn draw(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let content_focus = app.focus == Focus::Content;

    // Split: top half = rules list, bottom half = providers list.
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Ratio(1, 2), Constraint::Ratio(1, 2)])
        .split(area);

    draw_rules_panel(frame, rows[0], app, content_focus && !app.rules_focus_providers);

    // Task 4.2: sing-box's clash_api exposes no functional provider
    // endpoints — show an explanatory notice instead of an empty list.
    if app.gui_config.get_valid_proxy_core() == "singbox" {
        draw_providers_unsupported_notice(frame, rows[1]);
    } else {
        draw_providers_panel(frame, rows[1], app, content_focus && app.rules_focus_providers);
    }
}

/// ` [filter: x]` while the rule filter is active.
fn filter_suffix(app: &App) -> String {
    crate::app::filter::active(app.rule_filter.as_deref())
        .map(|query| {
            format!(
                " [{}: {}]",
                app.tr("logs.filter"),
                crate::ui::terminal_text::display(query)
            )
        })
        .unwrap_or_default()
}

/// "12" or "3/12" when a filter hides some.
fn count(shown: usize, total: usize) -> String {
    if shown == total {
        total.to_string()
    } else {
        format!("{shown}/{total}")
    }
}

/// Keep the cursor row on screen (the list scrolls with it).
fn render_list(frame: &mut Frame<'_>, area: Rect, list: List<'_>, selected: Option<usize>) {
    let mut state = ListState::default().with_selected(selected);
    frame.render_stateful_widget(list, area, &mut state);
}

fn draw_rules_panel(frame: &mut Frame<'_>, area: Rect, app: &App, focused: bool) {
    // Task 7.1: edit mode renders the profile-rule buffer instead of the
    // runtime-expanded rule list.
    if app.rules_edit_mode {
        draw_edit_buffer(frame, area, app, focused);
        return;
    }

    let rules = app.visible_rules();
    let title = format!(
        "{} ({}){}",
        app.tr("rules.title"),
        count(rules.len(), app.rules.len()),
        filter_suffix(app)
    );
    let block = theme::panel_block(title, focused);

    if app.rules_loading {
        let msg = Paragraph::new(Line::from(Span::styled("Loading...", Style::new().fg(theme::dim())))).block(block);
        frame.render_widget(msg, area);
        return;
    }

    if let Some(error) = &app.rules_error {
        let msg = Paragraph::new(Line::from(Span::styled(error, Style::new().fg(theme::danger()))))
            .block(block)
            .wrap(Wrap { trim: true });
        frame.render_widget(msg, area);
        return;
    }

    if rules.is_empty() {
        let msg = Paragraph::new(Line::from(Span::styled(
            app.tr(if app.rules.is_empty() {
                "rules.empty"
            } else {
                "common.no_matches"
            }),
            Style::new().fg(theme::dim()),
        )))
        .block(block);
        frame.render_widget(msg, area);
        return;
    }

    let items: Vec<ListItem> = rules
        .iter()
        .enumerate()
        .map(|(i, rule)| {
            let is_selected = focused && i == app.rules_selected_index;
            let prefix = if is_selected { ">" } else { " " };
            let line = Line::from(vec![
                Span::styled(
                    format!("{prefix} {:<10}", rule.rule_type),
                    if is_selected {
                        theme::bold(theme::accent())
                    } else {
                        Style::new().fg(theme::ok())
                    },
                ),
                Span::styled(
                    format!("{:<40}", rule.payload),
                    if is_selected {
                        Style::new().fg(theme::text())
                    } else {
                        Style::new().fg(theme::dim())
                    },
                ),
                Span::styled(&rule.proxy, Style::new().fg(theme::warn())),
            ]);
            ListItem::new(line)
        })
        .collect();

    let list = List::new(items).block(block);
    render_list(frame, area, list, focused.then_some(app.rules_selected_index));
}

/// Task 7.1: render the in-memory edit buffer (and the `*` dirty marker).
fn draw_edit_buffer(frame: &mut Frame<'_>, area: Rect, app: &App, focused: bool) {
    let title = format!(
        "{} [EDIT{}] ({})",
        app.tr("rules.title"),
        if app.rules_edit_dirty { "*" } else { "" },
        app.rules_edit_buffer.len()
    );
    let block = theme::panel_block(title, focused);

    if app.rules_edit_buffer.is_empty() {
        let msg = Paragraph::new(Line::from(Span::styled(
            "No rules - W saves, E exits",
            Style::new().fg(theme::dim()),
        )))
        .block(block);
        frame.render_widget(msg, area);
        return;
    }

    let items: Vec<ListItem> = app
        .rules_edit_buffer
        .iter()
        .enumerate()
        .map(|(i, rule)| {
            let is_selected = focused && i == app.rules_selected_index;
            let prefix = if is_selected { ">" } else { " " };
            let text = crate::routing::describe(rule);
            let line = Line::from(Span::styled(
                format!("{prefix} {text}"),
                if is_selected {
                    theme::bold(theme::accent())
                } else {
                    Style::new().fg(theme::text())
                },
            ));
            ListItem::new(line)
        })
        .collect();

    let list = List::new(items).block(block);
    frame.render_widget(list, area);
}

fn draw_providers_panel(frame: &mut Frame<'_>, area: Rect, app: &App, focused: bool) {
    let providers = app.visible_rule_providers();
    let title = format!(
        "Rule Providers ({}){}",
        count(providers.len(), app.rule_providers.len()),
        filter_suffix(app)
    );
    let block = theme::panel_block(title, focused);

    if app.rule_providers_loading {
        let msg = Paragraph::new(Line::from(Span::styled("Loading...", Style::new().fg(theme::dim())))).block(block);
        frame.render_widget(msg, area);
        return;
    }

    if let Some(error) = &app.rule_providers_error {
        let msg = Paragraph::new(Line::from(Span::styled(error, Style::new().fg(theme::danger()))))
            .block(block)
            .wrap(Wrap { trim: true });
        frame.render_widget(msg, area);
        return;
    }

    if providers.is_empty() {
        let msg = Paragraph::new(Line::from(Span::styled(
            if app.rule_providers.is_empty() {
                "No rule providers"
            } else {
                app.tr("common.no_matches")
            },
            Style::new().fg(theme::dim()),
        )))
        .block(block);
        frame.render_widget(msg, area);
        return;
    }

    let items: Vec<ListItem> = providers
        .iter()
        .enumerate()
        .map(|(i, provider)| {
            let is_selected = focused && i == app.rules_selected_index;
            let prefix = if is_selected { ">" } else { " " };
            let updated = provider.updated_at.as_deref().unwrap_or("-");
            let line = Line::from(vec![
                Span::styled(
                    format!("{prefix} "),
                    if is_selected {
                        Style::new().fg(theme::accent())
                    } else {
                        Style::new()
                    },
                ),
                Span::styled(
                    format!("{:<20}", provider.name),
                    if is_selected {
                        theme::bold(theme::text())
                    } else {
                        Style::new().fg(theme::text())
                    },
                ),
                Span::styled(format!("{:<10}", provider.behavior), Style::new().fg(theme::ok())),
                Span::styled(
                    format!("{:>4} rules  ", provider.rule_count),
                    Style::new().fg(theme::warn()),
                ),
                Span::styled(format!("{:<20}", updated), Style::new().fg(theme::dim())),
            ]);
            ListItem::new(line)
        })
        .collect();

    let list = List::new(items).block(block);
    render_list(frame, area, list, focused.then_some(app.rules_selected_index));
}

/// Task 4.2: explanatory notice shown instead of the providers panel while
/// the sing-box core is active (its clash_api provider endpoints are empty
/// stubs — hiding beats an empty list that looks broken).
fn draw_providers_unsupported_notice(frame: &mut Frame<'_>, area: Rect) {
    let block = theme::panel_block("Rule Providers", false);
    let msg = Paragraph::new(vec![
        Line::from(Span::styled(
            "Rule providers are not available under sing-box",
            Style::new().fg(theme::warn()),
        )),
        Line::from(Span::styled(
            "sing-box's clash_api does not implement provider endpoints.",
            Style::new().fg(theme::dim()),
        )),
        Line::from(Span::styled(
            "Switch back to mihomo to manage rule providers.",
            Style::new().fg(theme::dim()),
        )),
    ])
    .block(block);
    frame.render_widget(msg, area);
}
