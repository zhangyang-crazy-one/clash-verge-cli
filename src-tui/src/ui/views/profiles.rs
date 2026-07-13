use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::app::App;

pub fn draw(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let panes = crate::ui::split_view::draw(
        frame,
        area,
        app.tr("profiles.title"),
        app.tr("profiles.title"),
        app.tr("profiles.detail"),
        60,
    );

    crate::ui::profile_list::draw(frame, panes.left, &app.profiles, app.selected_index, app.language);
    draw_detail(frame, panes.right, app);
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
                name,
                Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            )),
            Line::from(format!("{}: {kind}", app.tr("profiles.type"))),
            Line::from(vec![
                Span::styled(
                    format!("{}: ", app.tr("profiles.source")),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::raw(source),
            ]),
            Line::from(Span::styled(
                app.tr("profiles.hint"),
                Style::default().fg(Color::DarkGray),
            )),
        ]
    } else {
        vec![
            Line::from(Span::styled(
                app.tr("profiles.no_selection"),
                Style::default().fg(Color::Yellow),
            )),
            Line::from(Span::styled(
                app.tr("profiles.import"),
                Style::default().fg(Color::DarkGray),
            )),
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
