use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, List, ListItem, Paragraph, Wrap};

use crate::app::{App, Focus};

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
    let border_style = if focused {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default()
    };
    let title = format!("{} ({})", app.tr("rules.title"), app.rules.len());

    if app.rules_loading {
        let msg = Paragraph::new(Line::from(Span::styled(
            "Loading...",
            Style::default().fg(Color::DarkGray),
        )))
        .block(Block::bordered().title(title).border_style(border_style));
        frame.render_widget(msg, area);
        return;
    }

    if let Some(error) = &app.rules_error {
        let msg = Paragraph::new(Line::from(Span::styled(error, Style::default().fg(Color::Red))))
            .block(Block::bordered().title(title).border_style(border_style))
            .wrap(Wrap { trim: true });
        frame.render_widget(msg, area);
        return;
    }

    if app.rules.is_empty() {
        let msg = Paragraph::new(Line::from(Span::styled(
            app.tr("rules.empty"),
            Style::default().fg(Color::DarkGray),
        )))
        .block(Block::bordered().title(title).border_style(border_style));
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
                        Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(Color::Green)
                    },
                ),
                Span::styled(
                    format!("{:<40}", rule.payload),
                    if is_selected {
                        Style::default().fg(Color::White)
                    } else {
                        Style::default().fg(Color::Gray)
                    },
                ),
                Span::styled(&rule.proxy, Style::default().fg(Color::Yellow)),
            ]);
            ListItem::new(line)
        })
        .collect();

    let list = List::new(items).block(Block::bordered().title(title).border_style(border_style));
    frame.render_widget(list, area);
}

fn draw_providers_panel(frame: &mut Frame<'_>, area: Rect, app: &App, focused: bool) {
    let border_style = if focused {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default()
    };
    let title = format!("Rule Providers ({})", app.rule_providers.len());

    if app.rule_providers_loading {
        let msg = Paragraph::new(Line::from(Span::styled(
            "Loading...",
            Style::default().fg(Color::DarkGray),
        )))
        .block(Block::bordered().title(title).border_style(border_style));
        frame.render_widget(msg, area);
        return;
    }

    if let Some(error) = &app.rule_providers_error {
        let msg = Paragraph::new(Line::from(Span::styled(error, Style::default().fg(Color::Red))))
            .block(Block::bordered().title(title).border_style(border_style))
            .wrap(Wrap { trim: true });
        frame.render_widget(msg, area);
        return;
    }

    if app.rule_providers.is_empty() {
        let msg = Paragraph::new(Line::from(Span::styled(
            "No rule providers",
            Style::default().fg(Color::DarkGray),
        )))
        .block(Block::bordered().title(title).border_style(border_style));
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
                        Style::default().fg(Color::Cyan)
                    } else {
                        Style::default()
                    },
                ),
                Span::styled(
                    format!("{:<20}", provider.name),
                    if is_selected {
                        Style::default().fg(Color::White).add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(Color::White)
                    },
                ),
                Span::styled(format!("{:<10}", provider.behavior), Style::default().fg(Color::Green)),
                Span::styled(
                    format!("{:>4} rules  ", provider.rule_count),
                    Style::default().fg(Color::Yellow),
                ),
                Span::styled(format!("{:<20}", updated), Style::default().fg(Color::DarkGray)),
            ]);
            ListItem::new(line)
        })
        .collect();

    let list = List::new(items).block(Block::bordered().title(title).border_style(border_style));
    frame.render_widget(list, area);
}
