//! Shell navigation: focus, selection movement, view switching, the filter
//! overlay, and help.

use crossterm::event::{KeyCode, KeyEvent};

use crate::app::{Action, App, Focus, Overlay, View};
use crate::tui::input;

use super::Ctx;
use super::connections::{move_connection_selection, move_log_selection};

/// `j`/`k`: move the menu selection or the focused list's selection,
/// wrapping at both ends.
pub(super) fn move_selection(app: &mut App, forward: bool) {
    if app.focus == Focus::Menu {
        let index = View::ALL.iter().position(|view| *view == app.view).unwrap_or_default();
        app.view = View::ALL[step(index, View::ALL.len(), forward)];
        return;
    }
    match app.view {
        View::Profiles => {
            let visible = app.visible_profile_indices();
            if !visible.is_empty() {
                let position = visible.iter().position(|index| *index == app.selected_index);
                // Off the filtered list: start from its first entry.
                let next = position.map_or(0, |position| step(position, visible.len(), forward));
                app.selected_index = visible[next];
            }
        }
        View::Proxies => {
            let total = app.proxy_rows().len();
            if total > 0 {
                app.node_selected_index = step(app.node_selected_index, total, forward);
            }
        }
        View::Rules => {
            let total = app.visible_rules_panel_len();
            if total > 0 {
                app.rules_selected_index = step(app.rules_selected_index, total, forward);
            }
        }
        View::Connections => move_connection_selection(app, forward),
        View::Logs => move_log_selection(app, forward),
        View::Settings => {
            app.settings_selected_index = step(
                app.settings_selected_index,
                crate::ui::views::settings::SETTINGS_ROW_COUNT,
                forward,
            );
        }
        _ => {}
    }
}

/// Next/previous index in `0..len` with wrap-around (`len > 0`). An index
/// already out of range (stale after the list shrank) steps from the end.
fn step(index: usize, len: usize, forward: bool) -> usize {
    if forward {
        if index + 1 >= len { 0 } else { index + 1 }
    } else if index == 0 {
        len.saturating_sub(1)
    } else {
        index.saturating_sub(1)
    }
}

/// `1`–`8`: switch views, loading data the new view shows.
pub(super) fn switch_view(app: &mut App, ctx: &Ctx, view: View) {
    app.view = view;
    match view {
        View::Proxies if app.proxy_groups.is_empty() => ctx.send(Action::ProxiesRefresh),
        View::Connections => ctx.send(Action::ConnectionsRefresh),
        View::Logs => ctx.send(Action::LogsRefresh),
        View::Rules => {
            if app.rules.is_empty() {
                ctx.send(Action::RulesRefresh);
            }
            if app.rule_providers.is_empty() {
                ctx.send(Action::RuleProvidersRefresh);
            }
        }
        _ => {}
    }
}

/// `Tab`: on the Rules view cycle between the Rules and Providers panels,
/// elsewhere between menu and content.
pub(super) fn cycle_focus(app: &mut App) {
    if app.view == View::Rules && app.focus == Focus::Content {
        app.rules_focus_providers = !app.rules_focus_providers;
        app.rules_selected_index = 0;
    } else {
        app.focus = app.focus.cycle();
    }
}

/// `/`: open the filter prompt, prefilled with the view's current filter.
pub(super) fn start_filter(app: &mut App) {
    app.filter = Some(view_filter(app).cloned().unwrap_or_default());
    app.overlay = Some(Overlay::Filter);
    app.focus = Focus::Content;
}

fn view_filter(app: &App) -> Option<&String> {
    match app.view {
        View::Connections => app.connection_filter.as_ref(),
        View::Logs => app.log_filter.as_ref(),
        View::Proxies => app.proxy_filter.as_ref(),
        View::Profiles => app.profile_filter.as_ref(),
        View::Rules => app.rule_filter.as_ref(),
        _ => None,
    }
}

/// Whether `/` filters the view.
pub(crate) const fn view_filters(view: View) -> bool {
    matches!(
        view,
        View::Connections | View::Logs | View::Proxies | View::Profiles | View::Rules
    )
}

pub(super) fn toggle_help(app: &mut App) {
    app.overlay = match app.overlay {
        Some(Overlay::Help) => None,
        _ => Some(Overlay::Help),
    };
}

/// Keys while the filter overlay is open.
pub(super) fn filter_input(app: &mut App, key: KeyEvent) {
    match input::map_key(key, key_context(app)) {
        Some(Action::SubmitFilter) => {
            let query = app.filter.take().unwrap_or_default();
            let submitted = (!query.trim().is_empty()).then_some(query);
            match app.view {
                View::Connections => app.connection_filter = submitted,
                View::Logs => {
                    app.log_filter = submitted;
                    app.log_selected_index = 0;
                }
                View::Proxies => {
                    let keep = app.proxy_rows().get(app.node_selected_index).cloned();
                    app.proxy_filter = submitted;
                    super::proxy::reselect(app, keep.as_ref());
                }
                View::Profiles => {
                    app.profile_filter = submitted;
                    let visible = app.visible_profile_indices();
                    if !visible.contains(&app.selected_index)
                        && let Some(first) = visible.first()
                    {
                        app.selected_index = *first;
                    }
                }
                View::Rules => {
                    app.rule_filter = submitted;
                    app.rules_selected_index = 0;
                }
                view => {
                    app.status_msg = Some(format!("Filtering is not available in {} yet", view.label()));
                }
            }
            app.overlay = None;
            app.focus = Focus::Content;
        }
        Some(Action::DismissOverlay) => dismiss_overlay(app),
        _ => match key.code {
            KeyCode::Backspace => {
                if let Some(filter) = app.filter.as_mut() {
                    filter.pop();
                }
            }
            KeyCode::Char(character) => {
                if let Some(filter) = app.filter.as_mut() {
                    filter.push(character);
                }
            }
            _ => {}
        },
    }
}

pub(super) fn key_context(app: &App) -> input::KeyContext<'_> {
    input::KeyContext {
        view: app.view,
        focus: app.focus,
        overlay: app.overlay,
        pending_connection_close: app.pending_connection_close.as_deref(),
    }
}

pub(super) fn dismiss_overlay(app: &mut App) {
    app.overlay = None;
    app.filter = None;
    app.pending_connection_close = None;
    app.pending_trust = None;
    app.focus = Focus::Menu;
}

#[cfg(test)]
mod step_tests {
    use super::step;

    #[test]
    fn step_wraps_in_both_directions() {
        assert_eq!(step(0, 3, true), 1);
        assert_eq!(step(2, 3, true), 0);
        assert_eq!(step(0, 3, false), 2);
        assert_eq!(step(2, 3, false), 1);
        // Stale index past the end (list shrank): forward wraps to the start.
        assert_eq!(step(7, 3, true), 0);
        assert_eq!(step(7, 3, false), 6);
    }
}
