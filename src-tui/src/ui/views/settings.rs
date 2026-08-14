use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Padding, Paragraph, Wrap};

use crate::app::App;
use crate::ui::theme;

pub const SETTINGS_ROW_COUNT: usize = 7;

pub fn draw(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(8), Constraint::Min(6)])
        .split(area);
    let mode = if app.clash_mode.is_empty() {
        app.core_config
            .get_mode()
            .unwrap_or_else(|| app.tr("common.unknown").into())
    } else {
        app.clash_mode.clone()
    };
    let core = Paragraph::new(vec![
        Line::from(Span::styled(
            app.tr("settings.runtime_heading"),
            theme::bold(theme::accent()),
        )),
        Line::from(format!("{}: {}", app.tr("settings.core"), core_state_label(app))),
        Line::from(format!("{}: {}", app.tr("settings.profile_count"), app.profiles.len())),
        Line::from(format!("{}: {mode}", app.tr("settings.mihomo_mode"))),
        Line::from(format!(
            "{}: mixed {} | socks {} | http {}",
            app.tr("settings.ports"),
            app.core_config.get_mixed_port(),
            app.core_config.get_socks_port(),
            app.core_config.get_port()
        )),
        Line::from(format!("{}: {}", app.tr("settings.core_pid"), core_owner_label(app))),
    ])
    .block(theme::panel_block(app.tr("settings.runtime"), false).padding(Padding::horizontal(1)));
    frame.render_widget(core, rows[0]);

    let cursor = app.settings_selected_index.min(SETTINGS_ROW_COUNT - 1);
    let support = Paragraph::new(vec![
        Line::from(Span::styled(app.tr("settings.gui_config"), theme::bold(theme::warn()))),
        settings_row(
            app,
            0,
            cursor,
            format!("{}: {}", app.tr("settings.language"), app.language.display_name()),
        ),
        settings_row(
            app,
            1,
            cursor,
            format!(
                "{}: {}",
                app.tr("settings.system_proxy"),
                enabled(app, app.gui_config.enable_system_proxy)
            ),
        ),
        settings_row(
            app,
            2,
            cursor,
            format!(
                "TUN: {}{}",
                enabled(app, app.gui_config.enable_tun_mode),
                if app.tun_privileged {
                    format!(" ({})", app.tr("settings.tun_capable"))
                } else {
                    String::new()
                }
            ),
        ),
        settings_row(
            app,
            3,
            cursor,
            format!(
                "{}: {}",
                app.tr("settings.tun_setup"),
                if app.tun_privileged {
                    app.tr("settings.tun_capable")
                } else {
                    app.tr("settings.tun_missing")
                }
            ),
        ),
        settings_row(app, 4, cursor, format!("{}: {mode}", app.tr("settings.mihomo_mode"))),
        settings_row(
            app,
            5,
            cursor,
            format!("{}: {}", app.tr("settings.service"), service_status(app)),
        ),
        settings_row(
            app,
            6,
            cursor,
            format!(
                "{}: {}",
                app.tr("settings.auto_launch"),
                if app.auto_launch_enabled {
                    app.tr("settings.on")
                } else {
                    app.tr("settings.off")
                }
            ),
        ),
        Line::from(format!(
            "{}: {}",
            app.tr("settings.proxy_host"),
            app.gui_config
                .proxy_host
                .as_deref()
                .unwrap_or(app.tr("settings.not_configured"))
        )),
        Line::from(Span::styled(
            app.tr("settings.writable_hint"),
            Style::new().fg(theme::dim()),
        )),
        Line::from(Span::styled(
            app.tr("settings.sudo_hint"),
            Style::new().fg(theme::dim()),
        )),
    ])
    .block(theme::panel_block(app.tr("settings.system"), false).padding(Padding::horizontal(1)))
    .wrap(Wrap { trim: true });
    frame.render_widget(support, rows[1]);
}

fn settings_row(app: &App, index: usize, cursor: usize, text: String) -> Line<'static> {
    let focused = app.focus == crate::app::Focus::Content && index == cursor;
    let style = if focused {
        theme::highlight(true)
    } else {
        Style::new().fg(theme::text())
    };
    let prefix = if focused { "> " } else { "  " };
    Line::from(Span::styled(format!("{prefix}{text}"), style))
}

fn core_owner_label(app: &App) -> String {
    match (app.core_pid, &app.core_state) {
        (Some(pid), _) => pid.to_string(),
        (None, crate::app::CoreState::Running) => app.tr("settings.gui_managed").into(),
        (None, _) => app.tr("settings.not_running").into(),
    }
}

fn enabled(app: &App, value: Option<bool>) -> &str {
    match value {
        Some(true) => app.tr("settings.on"),
        Some(false) => app.tr("settings.off"),
        None => app.tr("common.unknown"),
    }
}

fn core_state_label(app: &App) -> String {
    match &app.core_state {
        crate::app::CoreState::Running => app.tr("status.running").into(),
        crate::app::CoreState::Starting => app.tr("status.starting").into(),
        crate::app::CoreState::Stopped => app.tr("status.stopped").into(),
        crate::app::CoreState::Error(message) => format!("{}: {message}", app.tr("status.error")),
    }
}

/// Human-readable status for the 'System service' row, derived from the
/// cached read-only probes (`systemctl is-enabled` / `is-active`):
/// installed+enabled+running, installed+enabled+stopped, running-but-not-
/// enabled, or not installed.
fn service_status(app: &App) -> &'static str {
    match (app.service_enabled.as_str(), app.service_active.as_str()) {
        ("enabled", "active") => app.tr("settings.service_status_running"),
        ("enabled", _) => app.tr("settings.service_status_enabled_stopped"),
        (_, "active") => app.tr("settings.service_status_running_disabled"),
        _ => app.tr("settings.service_status_not_installed"),
    }
}

#[cfg(test)]
mod tests {
    use super::{SETTINGS_ROW_COUNT, core_owner_label, service_status};
    use crate::app::{App, CoreState};

    #[test]
    fn running_external_core_is_labeled_as_gui_managed() {
        let mut app = App::new();
        app.core_state = CoreState::Running;

        assert_eq!(core_owner_label(&app), "GUI-managed");
    }

    #[test]
    fn settings_rows_append_after_the_existing_five() {
        // The two new rows (System service, Launch at login) are appended at
        // indices 5 and 6 so the existing index handlers (0..4) never shift.
        assert_eq!(SETTINGS_ROW_COUNT, 7, "new rows must append after the existing five");
    }

    #[test]
    fn settings_navigation_wraps_within_the_new_row_count() {
        // Mirrors the event loop's MoveNext/MovePrevious math for Settings:
        // indices wrap within SETTINGS_ROW_COUNT, so the two appended rows
        // (5, 6) are reachable and the existing rows never shift.
        let next = |index: usize| (index + 1) % SETTINGS_ROW_COUNT;
        let prev = |index: usize| (index + SETTINGS_ROW_COUNT - 1) % SETTINGS_ROW_COUNT;
        assert_eq!(next(4), 5, "service row follows the mode row");
        assert_eq!(next(5), 6, "autostart row follows the service row");
        assert_eq!(next(6), 0, "navigation wraps past the last row");
        assert_eq!(prev(0), 6, "navigation wraps back to the last row");
        assert_eq!(prev(5), 4, "previous row from the service row is the mode row");
    }

    #[test]
    fn service_status_covers_all_probe_states() {
        let mut app = App::new();
        app.service_enabled = "enabled".into();
        app.service_active = "active".into();
        assert_eq!(service_status(&app), "installed · enabled · running");

        app.service_active = "inactive".into();
        assert_eq!(service_status(&app), "installed · enabled · stopped");

        app.service_enabled = "disabled".into();
        app.service_active = "active".into();
        assert_eq!(service_status(&app), "installed · running · not enabled");

        app.service_active = "inactive".into();
        assert_eq!(service_status(&app), "not installed");

        // Unknown probes (systemctl missing) render as not installed.
        app.service_enabled = "unknown".into();
        app.service_active = "unknown".into();
        assert_eq!(service_status(&app), "not installed");
    }
}
