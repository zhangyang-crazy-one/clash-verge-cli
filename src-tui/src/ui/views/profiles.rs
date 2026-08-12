use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::app::{App, Focus};
use crate::ui::theme;

pub fn draw(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(1)])
        .split(area);

    let panes = crate::ui::split_view::draw(
        frame,
        rows[0],
        app.tr("profiles.title"),
        app.tr("profiles.title"),
        app.tr("profiles.detail"),
        60,
        app.focus == Focus::Content,
    );

    crate::ui::profile_list::draw(frame, panes.left, &app.profiles, app.selected_index, app.language);
    draw_detail(frame, panes.right, app);
    draw_status(frame, rows[1], app);
}

/// Transient import/update feedback for the Profiles view. The user starts
/// the import here, so the outcome (success or failure) must be visible here
/// instead of only on Home.
fn draw_status(frame: &mut Frame<'_>, area: Rect, app: &App) {
    if let Some(message) = app.status_msg.as_deref() {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                crate::ui::terminal_text::display(message),
                Style::new().fg(theme::status_color(message)),
            ))),
            area,
        );
    }
}

fn draw_detail(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let lines = if let Some(profile) = app.profiles.get(app.selected_index) {
        let name = profile.name.as_deref().unwrap_or(app.tr("common.unknown"));
        let kind = profile.itype.as_deref().unwrap_or(app.tr("common.unknown"));
        let source = profile
            .url
            .as_deref()
            .map(redact_source)
            .unwrap_or_else(|| app.tr("profiles.local").into());
        vec![
            Line::from(Span::styled(
                crate::ui::terminal_text::display(name),
                theme::bold(theme::accent()),
            )),
            Line::from(format!("{}: {kind}", app.tr("profiles.type"))),
            Line::from(vec![
                Span::styled(
                    format!("{}: ", app.tr("profiles.source")),
                    Style::new().fg(theme::dim()),
                ),
                Span::raw(source),
            ]),
            Line::from(Span::styled(app.tr("profiles.hint"), Style::new().fg(theme::dim()))),
        ]
    } else {
        vec![
            Line::from(Span::styled(
                app.tr("profiles.no_selection"),
                Style::new().fg(theme::warn()),
            )),
            Line::from(Span::styled(app.tr("profiles.import"), Style::new().fg(theme::dim()))),
        ]
    };

    frame.render_widget(Paragraph::new(lines), area);
}

fn redact_source(source: &str) -> String {
    match url::Url::parse(source) {
        Ok(url) => {
            let host = url.host_str().unwrap_or("unknown-host");
            let short_host = host
                .rsplit_once('.')
                .map(|(prefix, tld)| {
                    let domain = prefix.rsplit_once('.').map_or(prefix, |(_, domain)| domain);
                    format!("{domain}.{tld}")
                })
                .unwrap_or_else(|| host.to_string());
            match url.port() {
                Some(port) => format!("{short_host}:{port}"),
                None => short_host,
            }
        }
        Err(_) => "configured source".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::redact_source;

    #[test]
    fn profile_source_does_not_expose_subscription_tokens() {
        assert_eq!(
            redact_source("https://example.test/api/v1/client/secret-token?token=also-secret"),
            "example.test"
        );
        assert_eq!(redact_source("not a URL"), "configured source");
    }
}
