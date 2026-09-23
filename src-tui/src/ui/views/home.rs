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
        .constraints([Constraint::Length(6), Constraint::Length(6), Constraint::Min(3)])
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
    let lines = if let Some(profile) = app.current_profile() {
        let name = profile.name.as_deref().unwrap_or(app.tr("common.unknown"));
        let kind = profile.itype.as_deref().unwrap_or(app.tr("common.unknown"));
        let mut lines = vec![
            Line::from(Span::styled(
                crate::ui::terminal_text::display(name),
                theme::bold(theme::text()),
            )),
            Line::from(Span::styled(format!("type: {kind}"), Style::new().fg(theme::dim()))),
        ];
        if let Some(extra) = profile.extra.as_ref() {
            let used = crate::commands::format_bytes(extra.upload.saturating_add(extra.download));
            let usage = if extra.total > 0 {
                format!("{used} / {}", crate::commands::format_bytes(extra.total))
            } else {
                used
            };
            lines.push(Line::from(vec![
                Span::styled(format!("{}: ", app.tr("home.usage")), Style::new().fg(theme::dim())),
                Span::styled(
                    usage,
                    Style::new().fg(usage_color(extra.upload + extra.download, extra.total)),
                ),
            ]));
            if extra.expire > 0 {
                lines.push(expiry_line(app, extra.expire));
            }
        }
        if let Some(updated) = profile
            .updated
            .and_then(|secs| local_date(secs as u64, "%Y-%m-%d %H:%M"))
        {
            lines.push(Line::from(Span::styled(
                format!("{}: {updated}", app.tr("home.updated")),
                Style::new().fg(theme::dim()),
            )));
        }
        lines
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

/// Red once 90% of the quota is used, yellow from 75%.
fn usage_color(used: u64, total: u64) -> ratatui::style::Color {
    if total == 0 {
        return theme::text();
    }
    match used.saturating_mul(100) / total {
        90.. => theme::danger(),
        75.. => theme::warn(),
        _ => theme::text(),
    }
}

/// `Expires: 2026-12-31 (99 days)`, red within a week, when expired.
fn expiry_line(app: &App, expire: u64) -> Line<'static> {
    let date = local_date(expire, "%Y-%m-%d").unwrap_or_else(|| "-".into());
    let now = chrono::Utc::now().timestamp();
    let days = (i64::try_from(expire).unwrap_or(i64::MAX) - now).div_euclid(86_400);
    let (detail, color) = if days < 0 {
        (app.tr("home.expired").to_string(), theme::danger())
    } else {
        (
            format!("{days} {}", app.tr("home.days_left")),
            if days < 7 { theme::danger() } else { theme::text() },
        )
    };
    Line::from(vec![
        Span::styled(format!("{}: ", app.tr("home.expires")), Style::new().fg(theme::dim())),
        Span::styled(format!("{date} ({detail})"), Style::new().fg(color)),
    ])
}

fn local_date(unix_secs: u64, format: &str) -> Option<String> {
    let secs = i64::try_from(unix_secs).ok()?;
    let time = chrono::DateTime::from_timestamp(secs, 0)?;
    Some(time.with_timezone(&chrono::Local).format(format).to_string())
}

fn draw_system(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let mode = if app.clash_mode.is_empty() {
        app.tr("common.unknown")
    } else {
        app.clash_mode.as_str()
    };
    let chain = app.outbound_chain();
    let outbound = if chain.is_empty() {
        Span::styled(app.tr("home.outbound_unknown"), Style::new().fg(theme::dim()))
    } else {
        let names: Vec<String> = chain
            .iter()
            .map(|name| crate::ui::terminal_text::display(name))
            .collect();
        let delay = chain
            .last()
            .and_then(|leaf| app.delay_map.get(leaf))
            .map(|delay| match delay {
                Some(ms) => format!(" ({ms}ms)"),
                None => format!(" ({})", app.tr("common.failed")),
            })
            .unwrap_or_default();
        Span::styled(format!("{}{delay}", names.join(" → ")), Style::new().fg(theme::ok()))
    };
    let on_off = |enabled: Option<bool>| {
        app.tr(if enabled.unwrap_or(false) {
            "common.on"
        } else {
            "common.off"
        })
    };
    let mut lines = vec![
        Line::from(Span::styled(
            format!("{}: {mode}", app.tr("home.mode")),
            Style::new().fg(theme::accent()),
        )),
        Line::from(vec![
            Span::styled(format!("{}: ", app.tr("home.outbound")), Style::new().fg(theme::dim())),
            outbound,
        ]),
        Line::from(Span::styled(
            format!(
                "{}: {} · TUN: {}",
                app.tr("home.system_proxy"),
                on_off(app.gui_config.enable_system_proxy),
                on_off(app.gui_config.enable_tun_mode)
            ),
            Style::new().fg(theme::dim()),
        )),
    ];
    if app.chain_mode {
        lines.push(Line::from(Span::styled(
            app.tr("home.chain_enabled"),
            Style::new().fg(theme::dim()),
        )));
    }
    frame.render_widget(
        Paragraph::new(lines)
            .block(theme::panel_block(app.tr("home.proxy_system"), false).padding(Padding::horizontal(1))),
        area,
    );
}

fn draw_traffic(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let lines = if let Some(traffic) = &app.traffic {
        let rate = |bytes: u64| format!("{}/s", crate::commands::format_bytes(bytes));
        let mut lines = vec![Line::from(vec![
            Span::styled("↑ ", Style::new().fg(theme::dim())),
            Span::styled(rate(traffic.up), Style::new().fg(theme::accent())),
            Span::styled("   ↓ ", Style::new().fg(theme::dim())),
            Span::styled(rate(traffic.down), Style::new().fg(theme::accent())),
        ])];
        if let Some((up, down)) = app.traffic_totals {
            lines.push(Line::from(Span::styled(
                format!(
                    "{}: ↑ {}  ↓ {}",
                    app.tr("home.session_total"),
                    crate::commands::format_bytes(up),
                    crate::commands::format_bytes(down)
                ),
                Style::new().fg(theme::dim()),
            )));
        }
        lines
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
