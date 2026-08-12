use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::app::{App, ProxyDisplayRow, proxy_display_rows};

pub fn draw(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let expanded_group = app.expanded_proxy_group.as_deref();
    let rows = proxy_display_rows(&app.proxy_groups, expanded_group);
    let left_title = expanded_group
        .map(|group| {
            format!(
                "{} / {} {}",
                app.tr("proxies.groups"),
                crate::ui::terminal_text::display(group),
                app.tr("proxies.nodes")
            )
        })
        .unwrap_or_else(|| app.tr("proxies.proxy_groups").to_string());
    let panes = crate::ui::split_view::draw(
        frame,
        area,
        app.tr("proxies.title"),
        &left_title,
        app.tr("proxies.detail"),
        64,
    );

    crate::ui::proxy_list::draw(
        frame,
        panes.left,
        &rows,
        app.node_selected_index,
        &app.delay_map,
        &app.core_state,
        app.runtime_loading.proxies,
        app.runtime_errors.proxies.as_deref(),
        app.language,
    );
    draw_detail(frame, panes.right, app, &rows);
}

fn draw_detail(frame: &mut Frame<'_>, area: Rect, app: &App, rows: &[ProxyDisplayRow]) {
    let mut lines = match rows.get(app.node_selected_index) {
        Some(ProxyDisplayRow::Group {
            name,
            group_type,
            current,
            node_count,
        }) => vec![
            Line::from(Span::styled(
                crate::ui::terminal_text::display(name),
                Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            )),
            Line::from(format!("{}: {group_type}", app.tr("proxies.type"))),
            Line::from(format!(
                "{}: {}",
                app.tr("proxies.selected"),
                current
                    .as_deref()
                    .map(crate::ui::terminal_text::display)
                    .unwrap_or_else(|| "none".to_string())
            )),
            Line::from(format!("{}: {node_count}", app.tr("proxies.nodes"))),
            Line::from(chain_selection_label(app)),
            Line::from(Span::styled(
                chain_command_hint(app),
                Style::default().fg(Color::DarkGray),
            )),
        ],
        Some(ProxyDisplayRow::Node { group, name, current }) => {
            let delay = match app.delay_map.get(name) {
                Some(Some(milliseconds)) => format!("{milliseconds}ms"),
                Some(None) => app.tr("common.failed").to_string(),
                None => app.tr("proxies.not_tested").to_string(),
            };
            vec![
                Line::from(Span::styled(
                    crate::ui::terminal_text::display(name),
                    Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                )),
                Line::from(format!(
                    "{}: {}",
                    app.tr("proxies.group"),
                    crate::ui::terminal_text::display(group)
                )),
                Line::from(format!("{}: {delay}", app.tr("proxies.delay"))),
                Line::from(format!(
                    "{}: {}",
                    app.tr("proxies.current"),
                    if *current {
                        app.tr("common.yes")
                    } else {
                        app.tr("common.no")
                    }
                )),
                Line::from(Span::styled(
                    chain_selection_label(app),
                    Style::default().fg(Color::DarkGray),
                )),
                Line::from(Span::styled(
                    chain_command_hint(app),
                    Style::default().fg(Color::DarkGray),
                )),
            ]
        }
        None => vec![
            Line::from(Span::styled(
                app.tr("proxies.no_selection"),
                Style::default().fg(Color::Yellow),
            )),
            Line::from(Span::styled(
                app.tr("proxies.start_or_profile"),
                Style::default().fg(Color::DarkGray),
            )),
        ],
    };

    if let Some((done, total)) = app.batch_delay {
        lines.push(Line::from(Span::styled(
            format!("{}: {done}/{total}", app.tr("proxies.batch_delay")),
            Style::default().fg(Color::Yellow),
        )));
    }

    frame.render_widget(Paragraph::new(lines), area);
}

fn chain_selection_label(app: &App) -> String {
    if !app.chain_mode {
        return app.tr("proxies.chain_off").into();
    }

    match app.chain_nodes.as_slice() {
        [] => app.tr("proxies.chain_route").into(),
        [entry] => format!(
            "{}: {}; {}",
            app.tr("proxies.chain_entry"),
            crate::ui::terminal_text::display(entry),
            app.tr("proxies.select_exit")
        ),
        [entry, .., exit] => format!(
            "{}: {} -> {}: {} ({} {})",
            app.tr("proxies.chain_entry"),
            crate::ui::terminal_text::display(entry),
            app.tr("proxies.select_exit"),
            crate::ui::terminal_text::display(exit),
            app.chain_nodes.len(),
            app.tr("proxies.chain_nodes")
        ),
    }
}

fn chain_command_hint(app: &App) -> &'static str {
    if app.chain_mode {
        app.tr("proxies.hint_chain")
    } else {
        app.tr("proxies.hint_normal")
    }
}
