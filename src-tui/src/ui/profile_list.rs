use clash_verge_core::config::PrfItem;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, ListState};

use crate::i18n::{Language, tr};

pub fn draw(frame: &mut Frame<'_>, area: Rect, profiles: &[PrfItem], selected_index: usize, language: Language) {
    let mut items: Vec<ListItem<'_>> = profiles
        .iter()
        .enumerate()
        .map(|(index, profile)| {
            let name = profile.name.as_deref().unwrap_or(tr(language, "common.unknown"));
            let kind = profile.itype.as_deref().unwrap_or(tr(language, "common.unknown"));
            ListItem::new(Line::from(vec![
                Span::styled(format!("{} ", index + 1), Style::default().fg(Color::DarkGray)),
                Span::styled(
                    crate::ui::terminal_text::display(name),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Span::styled(format!(" [{kind}]"), Style::default().fg(Color::DarkGray)),
            ]))
        })
        .collect();
    if items.is_empty() {
        items.push(ListItem::new(Span::styled(
            tr(language, "profiles.none"),
            Style::default().fg(Color::DarkGray),
        )));
    }

    let selection = if profiles.is_empty() {
        None
    } else {
        Some(selected_index.min(profiles.len() - 1))
    };
    let mut state = ListState::default().with_selected(selection);

    let list = List::new(items)
        .highlight_style(Style::default().fg(Color::Cyan))
        .highlight_symbol("> ");

    frame.render_stateful_widget(list, area, &mut state);
}
