use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::app::{App, InputMode, Overlay, View};
use crate::tui::input;
use crate::ui::theme;

/// Draw the inline URL input bar at the bottom of the screen.
pub fn draw(frame: &mut Frame<'_>, area: Rect, app: &App) {
    if let Some(overlay) = app.overlay {
        match overlay {
            Overlay::Filter => {
                let query = app.filter.as_deref().unwrap_or_default();
                let line = Line::from(vec![
                    Span::styled(format!("{} ", app.tr("input.filter")), Style::new().fg(theme::accent())),
                    Span::styled(app.view.localized_label(app.language), Style::new().fg(theme::text())),
                    Span::styled(": ", Style::new().fg(theme::dim())),
                    Span::styled(query, Style::new().fg(theme::text())),
                    Span::styled(app.tr("input.apply_cancel"), Style::new().fg(theme::dim())),
                ]);
                frame.render_widget(Paragraph::new(line), area);
                return;
            }
            Overlay::CloseConfirmation => {
                let target = app
                    .pending_connection_close
                    .as_deref()
                    .unwrap_or(app.tr("common.unknown"));
                let line = Line::from(vec![
                    Span::styled(format!("{} ", app.tr("input.close")), Style::new().fg(theme::danger())),
                    Span::styled(target, Style::new().fg(theme::text())),
                    Span::styled(app.tr("input.confirm_cancel"), Style::new().fg(theme::dim())),
                ]);
                frame.render_widget(Paragraph::new(line), area);
                return;
            }
            Overlay::CloseAllConnectionsConfirmation => {
                let line = Line::from(vec![
                    Span::styled("Close ALL connections? ", Style::new().fg(theme::danger())),
                    Span::styled("Enter = confirm | Esc/q = cancel", Style::new().fg(theme::dim())),
                ]);
                frame.render_widget(Paragraph::new(line), area);
                return;
            }
            // The core-start TUN setup confirm: canonical dialog renders the
            // warning; the input bar hints at the choice keys.
            Overlay::TunSetupConfirmation => {
                let line = Line::from(vec![
                    Span::styled(
                        format!("{} ", app.tr("dialog.tun_setup")),
                        Style::new().fg(theme::warn()),
                    ),
                    Span::styled(app.tun_setup_confirm_hint(), Style::new().fg(theme::dim())),
                ]);
                frame.render_widget(Paragraph::new(line), area);
                return;
            }
            // The service-uninstall confirm mirrors the tun confirm: dialog
            // renders the warning, the input bar hints at the choice keys.
            Overlay::ServiceUninstallConfirmation => {
                let line = Line::from(vec![
                    Span::styled(
                        format!("{} ", app.tr("dialog.service_uninstall")),
                        Style::new().fg(theme::danger()),
                    ),
                    Span::styled(
                        app.tr("dialog.service_uninstall_confirm"),
                        Style::new().fg(theme::dim()),
                    ),
                ]);
                frame.render_widget(Paragraph::new(line), area);
                return;
            }
            // The canonical password dialog (rounded, localized) renders the
            // prompt and masked buffer; the input bar only hints at the keys.
            Overlay::PasswordInput => {
                let line = Line::from(Span::styled(
                    app.tr("dialog.password.hint"),
                    Style::new().fg(theme::dim()),
                ));
                frame.render_widget(Paragraph::new(line), area);
                return;
            }
            Overlay::TrustConfirmation => {
                let is_update = app.pending_trust.as_ref().is_some_and(|pending| pending.uid.is_some());
                let host = app
                    .pending_trust
                    .as_ref()
                    .map(|pending| pending.host.as_str())
                    .unwrap_or(app.tr("common.unknown"));
                let label = app.tr(if is_update {
                    "dialog.trust_update"
                } else {
                    "dialog.trust"
                });
                let confirm = app.tr(if is_update {
                    "dialog.trust_update_confirm"
                } else {
                    "dialog.trust_confirm"
                });
                let line = Line::from(vec![
                    Span::styled(format!("{label} "), Style::new().fg(theme::danger())),
                    Span::styled(host, Style::new().fg(theme::text())),
                    Span::styled(confirm, Style::new().fg(theme::dim())),
                ]);
                frame.render_widget(Paragraph::new(line), area);
                return;
            }
            Overlay::Help => {}
        }
    }

    match &app.input_mode {
        InputMode::Normal => {
            let mut spans = vec![
                Span::styled(app.tr("input.views_focus"), Style::new().fg(theme::dim())),
                Span::styled(" | ", Style::new().fg(theme::dim())),
                Span::styled(
                    input::context_hint(app.view, app.focus, app.language),
                    Style::new().fg(theme::text()),
                ),
            ];
            if matches!(app.view, View::Connections | View::Logs) {
                spans.push(Span::styled(" | / filter", Style::new().fg(theme::dim())));
            }
            if let Some((done, total)) = app.batch_delay {
                spans.push(Span::styled(
                    format!(" | {} {done}/{total}", app.tr("proxies.batch_delay")),
                    Style::new().fg(theme::warn()),
                ));
            }
            spans.push(Span::styled(app.tr("input.help_quit"), Style::new().fg(theme::dim())));
            frame.render_widget(Paragraph::new(Line::from(spans)), area);
        }
        InputMode::Importing(buffer) => {
            let line = Line::from(vec![
                Span::styled(app.tr("input.url"), Style::new().fg(theme::accent())),
                Span::styled(buffer.clone(), Style::new().fg(theme::text())),
                Span::styled("|", Style::new().fg(theme::dim())),
                Span::styled(app.tr("input.submit_cancel"), Style::new().fg(theme::dim())),
            ]);
            frame.render_widget(Paragraph::new(line), area);
        }
    }
}
