use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Padding, Paragraph};

use crate::app::{App, CoreState};
use crate::ui::theme;

pub fn draw(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(5), Constraint::Length(5), Constraint::Min(3)])
        .split(area);
    let top = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(rows[0]);
    let middle = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(rows[1]);

    draw_core(frame, top[0], app);
    draw_profile(frame, top[1], app);
    draw_system(frame, middle[0], app);
    draw_traffic(frame, middle[1], app);
    draw_messages(frame, rows[2], app);
}

fn draw_core(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let (label, color, detail) = match &app.core_state {
        CoreState::Running => {
            let detail = if let Some(version) = app.core_version.as_deref() {
                version
            } else if area.width < 36 {
                app.tr("home.core_accepts")
            } else {
                app.tr("home.core_accepting")
            };
            (app.tr("status.running"), theme::ok(), detail)
        }
        CoreState::Starting => (app.tr("status.starting"), theme::warn(), app.tr("home.core_waiting")),
        CoreState::Stopped => (
            app.tr("status.stopped"),
            theme::dim(),
            if area.width < 36 {
                app.tr("home.press_start")
            } else {
                app.tr("home.press_start_core")
            },
        ),
        CoreState::Error(message) => (app.tr("status.error"), theme::danger(), message.as_str()),
    };
    let mut lines = vec![
        Line::from(Span::styled(label, theme::bold(color))),
        Line::from(Span::styled(detail, Style::new().fg(theme::dim()))),
    ];
    if matches!(app.core_state, CoreState::Running)
        && let Some(pid) = app.core_pid
    {
        lines.push(Line::from(Span::styled(
            format!("pid {pid}"),
            Style::new().fg(theme::dim()),
        )));
    }
    frame.render_widget(
        Paragraph::new(lines).block(theme::panel_block(app.tr("home.core"), false).padding(Padding::horizontal(1))),
        area,
    );
}

fn draw_profile(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let lines = if let Some(profile) = app.profiles.get(app.selected_index) {
        let name = profile.name.as_deref().unwrap_or(app.tr("common.unknown"));
        let kind = profile.itype.as_deref().unwrap_or(app.tr("common.unknown"));
        vec![
            Line::from(Span::styled(
                crate::ui::terminal_text::display(name),
                theme::bold(theme::text()),
            )),
            Line::from(Span::styled(format!("type: {kind}"), Style::new().fg(theme::dim()))),
        ]
    } else {
        vec![
            Line::from(Span::styled(
                app.tr("home.no_active_profile"),
                Style::new().fg(theme::warn()),
            )),
            Line::from(Span::styled(
                app.tr("home.import_profile"),
                Style::new().fg(theme::dim()),
            )),
        ]
    };
    frame.render_widget(
        Paragraph::new(lines)
            .block(theme::panel_block(app.tr("home.active_profile"), false).padding(Padding::horizontal(1))),
        area,
    );
}

fn draw_system(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let selection = if app.chain_mode {
        app.tr("home.chain_enabled")
    } else {
        app.tr("home.direct_selection")
    };
    let mode = if app.clash_mode.is_empty() {
        app.tr("common.unknown")
    } else {
        app.clash_mode.as_str()
    };
    let focus = match app.focus {
        crate::app::Focus::Menu => app.tr("home.menu_focus"),
        crate::app::Focus::Content => app.tr("home.content_focus"),
    };
    let lines = vec![
        Line::from(Span::styled(
            format!("{}: {mode}", app.tr("home.mode")),
            Style::new().fg(theme::accent()),
        )),
        Line::from(Span::styled(selection, Style::new().fg(theme::dim()))),
        Line::from(Span::styled(focus, Style::new().fg(theme::dim()))),
    ];
    frame.render_widget(
        Paragraph::new(lines)
            .block(theme::panel_block(app.tr("home.proxy_system"), false).padding(Padding::horizontal(1))),
        area,
    );
}

fn draw_traffic(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let lines = if let Some(traffic) = &app.traffic {
        vec![
            Line::from(vec![
                Span::styled("UP   ", Style::new().fg(theme::dim())),
                Span::styled(format!("{} B/s", traffic.up), Style::new().fg(theme::accent())),
            ]),
            Line::from(vec![
                Span::styled("DOWN ", Style::new().fg(theme::dim())),
                Span::styled(format!("{} B/s", traffic.down), Style::new().fg(theme::accent())),
            ]),
        ]
    } else {
        vec![
            Line::from(Span::styled(app.tr("home.no_traffic"), Style::new().fg(theme::dim()))),
            Line::from(Span::styled(
                if area.width < 36 {
                    app.tr("home.press_traffic")
                } else {
                    app.tr("home.start_traffic")
                },
                Style::new().fg(theme::dim()),
            )),
        ]
    };
    frame.render_widget(
        Paragraph::new(lines).block(theme::panel_block(app.tr("home.traffic"), false).padding(Padding::horizontal(1))),
        area,
    );
}

fn draw_messages(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let message = app.status_msg.as_deref().unwrap_or(app.tr("home.no_messages"));
    let color = if app.status_msg.is_some() {
        theme::status_color(message)
    } else {
        theme::dim()
    };
    let paragraph = Paragraph::new(Line::from(Span::styled(
        crate::ui::terminal_text::display(message),
        Style::new().fg(color),
    )))
    .block(theme::panel_block(app.tr("home.messages"), false).padding(Padding::horizontal(1)));
    frame.render_widget(paragraph, area);
}
