use clash_verge_core::config::PrfItem;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, ListState};

use crate::i18n::{Language, tr};
use crate::ui::theme;

pub fn draw(frame: &mut Frame<'_>, area: Rect, profiles: &[PrfItem], selected_index: usize, language: Language) {
    let mut items: Vec<ListItem<'_>> = profiles
        .iter()
        .enumerate()
        .map(|(index, profile)| {
            let name = profile.name.as_deref().unwrap_or(tr(language, "common.unknown"));
            let kind = profile.itype.as_deref().unwrap_or(tr(language, "common.unknown"));
            ListItem::new(Line::from(vec![
                Span::styled(format!("{} ", index + 1), Style::new().fg(theme::dim())),
                Span::styled(
                    crate::ui::terminal_text::display(name),
                    Style::new().add_modifier(Modifier::BOLD),
                ),
                Span::styled(format!(" [{kind}]"), Style::new().fg(theme::dim())),
            ]))
        })
        .collect();
    if items.is_empty() {
        items.push(ListItem::new(Span::styled(
            tr(language, "profiles.none"),
            Style::new().fg(theme::dim()),
        )));
    }

    let selection = if profiles.is_empty() {
        None
    } else {
        Some(selected_index.min(profiles.len() - 1))
    };
    let mut state = ListState::default().with_selected(selection);

    let list = List::new(items)
        .highlight_style(theme::highlight(true))
        .highlight_symbol("> ");

    frame.render_stateful_widget(list, area, &mut state);
}
