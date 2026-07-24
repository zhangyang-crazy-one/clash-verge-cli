use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};

use crate::app::{App, CoreState};

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
            (app.tr("status.running"), Color::Green, detail)
        }
        CoreState::Starting => (app.tr("status.starting"), Color::Yellow, app.tr("home.core_waiting")),
        CoreState::Stopped => (
            app.tr("status.stopped"),
            Color::DarkGray,
            if area.width < 36 {
                app.tr("home.press_start")
            } else {
                app.tr("home.press_start_core")
            },
        ),
        CoreState::Error(message) => (app.tr("status.error"), Color::Red, message.as_str()),
    };
    let mut lines = vec![
        Line::from(Span::styled(
            label,
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(detail, Style::default().fg(Color::DarkGray))),
    ];
    if matches!(app.core_state, CoreState::Running)
        && let Some(pid) = app.core_pid
    {
        lines.push(Line::from(Span::styled(
            format!("pid {pid}"),
            Style::default().fg(Color::DarkGray),
        )));
    }
    frame.render_widget(
        Paragraph::new(lines).block(Block::bordered().title(app.tr("home.core"))),
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
                Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::styled(
                format!("type: {kind}"),
                Style::default().fg(Color::DarkGray),
            )),
        ]
    } else {
        vec![
            Line::from(Span::styled(
                app.tr("home.no_active_profile"),
                Style::default().fg(Color::Yellow),
            )),
            Line::from(Span::styled(
                app.tr("home.import_profile"),
                Style::default().fg(Color::DarkGray),
            )),
        ]
    };
    frame.render_widget(
        Paragraph::new(lines).block(Block::bordered().title(app.tr("home.active_profile"))),
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
            Style::default().fg(Color::Cyan),
        )),
        Line::from(Span::styled(selection, Style::default().fg(Color::DarkGray))),
        Line::from(Span::styled(focus, Style::default().fg(Color::DarkGray))),
    ];
    frame.render_widget(
        Paragraph::new(lines).block(Block::bordered().title(app.tr("home.proxy_system"))),
        area,
    );
}

fn draw_traffic(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let lines = if let Some(traffic) = &app.traffic {
        vec![
            Line::from(vec![
                Span::styled("UP   ", Style::default().fg(Color::DarkGray)),
                Span::styled(format!("{} B/s", traffic.up), Style::default().fg(Color::Cyan)),
            ]),
            Line::from(vec![
                Span::styled("DOWN ", Style::default().fg(Color::DarkGray)),
                Span::styled(format!("{} B/s", traffic.down), Style::default().fg(Color::Magenta)),
            ]),
        ]
    } else {
        vec![
            Line::from(Span::styled(
                app.tr("home.no_traffic"),
                Style::default().fg(Color::DarkGray),
            )),
            Line::from(Span::styled(
                if area.width < 36 {
                    app.tr("home.press_traffic")
                } else {
                    app.tr("home.start_traffic")
                },
                Style::default().fg(Color::DarkGray),
            )),
        ]
    };
    frame.render_widget(
        Paragraph::new(lines).block(Block::bordered().title(app.tr("home.traffic"))),
        area,
    );
}

fn draw_messages(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let message = app.status_msg.as_deref().unwrap_or(app.tr("home.no_messages"));
    let paragraph = Paragraph::new(Line::from(Span::styled(
        crate::ui::terminal_text::display(message),
        Style::default().fg(Color::DarkGray),
    )))
    .block(Block::bordered().title(app.tr("home.messages")));
    frame.render_widget(paragraph, area);
}
