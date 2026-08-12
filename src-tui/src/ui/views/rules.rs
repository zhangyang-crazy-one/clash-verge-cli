use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, Paragraph, Wrap};

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
    draw_providers_panel(frame, rows[1], app, content_focus && app.rules_focus_providers);
}

fn draw_rules_panel(frame: &mut Frame<'_>, area: Rect, app: &App, focused: bool) {
    let title = format!("{} ({})", app.tr("rules.title"), app.rules.len());
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

    if app.rules.is_empty() {
        let msg = Paragraph::new(Line::from(Span::styled(
            app.tr("rules.empty"),
            Style::new().fg(theme::dim()),
        )))
        .block(block);
        frame.render_widget(msg, area);
        return;
    }

    let items: Vec<ListItem> = app
        .rules
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
    frame.render_widget(list, area);
}

fn draw_providers_panel(frame: &mut Frame<'_>, area: Rect, app: &App, focused: bool) {
    let title = format!("Rule Providers ({})", app.rule_providers.len());
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

    if app.rule_providers.is_empty() {
        let msg = Paragraph::new(Line::from(Span::styled(
            "No rule providers",
            Style::new().fg(theme::dim()),
        )))
        .block(block);
        frame.render_widget(msg, area);
        return;
    }

    let items: Vec<ListItem> = app
        .rule_providers
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
    frame.render_widget(list, area);
}
