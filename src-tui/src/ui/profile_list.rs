use clash_verge_core::config::PrfItem;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, ListState};

use crate::i18n::{Language, tr};
use crate::ui::theme;

/// Draw the profiles at `visible` (indices into `profiles`), highlighting
/// `selected_index` when it is one of them.
pub fn draw(
    frame: &mut Frame<'_>,
    area: Rect,
    profiles: &[PrfItem],
    visible: &[usize],
    selected_index: usize,
    language: Language,
) {
    let mut items: Vec<ListItem<'_>> = visible
        .iter()
        .filter_map(|index| profiles.get(*index).map(|profile| (*index, profile)))
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
            tr(
                language,
                if profiles.is_empty() {
                    "profiles.none"
                } else {
                    "common.no_matches"
                },
            ),
            Style::new().fg(theme::dim()),
        )));
    }

    let selection = visible.iter().position(|index| *index == selected_index);
    let mut state = ListState::default().with_selected(selection);

    let list = List::new(items)
        .highlight_style(theme::highlight(true))
        .highlight_symbol("> ");

    frame.render_stateful_widget(list, area, &mut state);
}
