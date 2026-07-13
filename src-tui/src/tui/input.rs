use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::app::{Action, Focus, Overlay, View};
use crate::i18n::Language;

/// The state that determines both key dispatch and the commands visible in the shell.
/// Text entry itself remains in the event loop, but its submit/cancel actions are mapped
/// here so overlays cannot accidentally fall through to a global command.
#[derive(Debug, Clone, Copy)]
pub struct KeyContext<'a> {
    pub view: View,
    pub focus: Focus,
    pub overlay: Option<Overlay>,
    pub pending_connection_close: Option<&'a str>,
}

impl<'a> KeyContext<'a> {
    #[cfg(test)]
    pub const fn base(view: View, focus: Focus) -> Self {
        Self {
            view,
            focus,
            overlay: None,
            pending_connection_close: None,
        }
    }
}

/// Short command labels shared by the command bar and the per-view dispatch model.
pub fn context_hint(view: View, focus: Focus, language: Language) -> &'static str {
    if matches!(focus, Focus::Menu) {
        return crate::i18n::tr(language, "hint.menu");
    }
    match view {
        View::Home => crate::i18n::tr(language, "hint.home"),
        View::Proxies => crate::i18n::tr(language, "hint.proxies"),
        View::Profiles => crate::i18n::tr(language, "hint.profiles"),
        View::Connections => crate::i18n::tr(language, "hint.connections"),
        View::Rules => crate::i18n::tr(language, "hint.rules"),
        View::Logs => crate::i18n::tr(language, "hint.logs"),
        View::Unlock => crate::i18n::tr(language, "hint.unlock"),
        View::Settings => crate::i18n::tr(language, "hint.settings"),
    }
}

/// Map a crossterm KeyEvent to an optional action using the current shell state.
/// Input mode character editing is handled directly in the event loop.
pub fn map_key(event: KeyEvent, context: KeyContext<'_>) -> Option<Action> {
    if let Some(overlay) = context.overlay {
        return match (overlay, event.code) {
            (Overlay::Filter, KeyCode::Enter) => Some(Action::SubmitFilter),
            (Overlay::CloseConfirmation, KeyCode::Enter) => context
                .pending_connection_close
                .map(|id| Action::ConfirmCloseConnection(id.to_string())),
            (_, KeyCode::Esc | KeyCode::Char('q')) => Some(Action::DismissOverlay),
            (Overlay::Help, KeyCode::Char('?')) => Some(Action::DismissOverlay),
            _ => None,
        };
    }

    match event.code {
        KeyCode::Char('q') => Some(Action::Quit),
        KeyCode::Char('s') if context.view == View::Home && event.modifiers.contains(KeyModifiers::SHIFT) => {
            Some(Action::StopCore)
        }
        KeyCode::Char('s') if context.view == View::Home => Some(Action::StartCore),
        KeyCode::Char('r') if context.view == View::Home => Some(Action::RestartCore),
        KeyCode::Esc => Some(Action::DismissOverlay),

        // View navigation
        KeyCode::Char('1') => Some(Action::SwitchView(View::Home)),
        KeyCode::Char('2') => Some(Action::SwitchView(View::Proxies)),
        KeyCode::Char('3') => Some(Action::SwitchView(View::Profiles)),
        KeyCode::Char('4') => Some(Action::SwitchView(View::Connections)),
        KeyCode::Char('5') => Some(Action::SwitchView(View::Rules)),
        KeyCode::Char('6') => Some(Action::SwitchView(View::Logs)),
        KeyCode::Char('7') => Some(Action::SwitchView(View::Unlock)),
        KeyCode::Char('8') => Some(Action::SwitchView(View::Settings)),
        KeyCode::Tab => Some(Action::CycleFocus),
        KeyCode::Char('h') | KeyCode::Left => Some(Action::FocusMenu),
        KeyCode::Char('l') | KeyCode::Right => Some(Action::FocusContent),
        KeyCode::Char('/') if matches!(context.view, View::Connections | View::Logs) => Some(Action::StartFilter),
        KeyCode::Char('?') => Some(Action::ToggleHelp),

        // Movement and contextual activation
        KeyCode::Char('j') | KeyCode::Down => Some(Action::MoveNext),
        KeyCode::Char('k') | KeyCode::Up => Some(Action::MovePrevious),
        KeyCode::Char('i') if context.view == View::Profiles => Some(Action::StartImport),
        KeyCode::Enter => Some(Action::Activate),
        KeyCode::Char('u') if context.view == View::Profiles => Some(Action::UpdateProfile),

        // Connection actions only exist on the Connections content surface. The request
        // opens a target-named confirmation; it never performs the DELETE directly.
        KeyCode::Char('d') if context.view == View::Connections && context.focus == Focus::Content => {
            Some(Action::RequestCloseConnection)
        }

        // Delay test
        KeyCode::Char('T') if context.view == View::Proxies && event.modifiers.contains(KeyModifiers::SHIFT) => {
            Some(Action::NodeDelayAll)
        }
        KeyCode::Char('t') if context.view == View::Proxies => Some(Action::NodeDelayTest),

        // Chain proxy
        KeyCode::Char('c') if context.view == View::Proxies => Some(Action::ToggleChainMode),
        KeyCode::Char('a') if context.view == View::Proxies => Some(Action::ApplyChain),
        KeyCode::Char('x') if context.view == View::Proxies => Some(Action::ClearChain),

        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn base(view: View) -> KeyContext<'static> {
        KeyContext::base(view, Focus::Content)
    }

    #[test]
    fn maps_number_keys_to_gui_ordered_views() {
        let mappings = [
            ('1', View::Home),
            ('2', View::Proxies),
            ('3', View::Profiles),
            ('4', View::Connections),
            ('5', View::Rules),
            ('6', View::Logs),
            ('7', View::Unlock),
            ('8', View::Settings),
        ];

        for (key, expected) in mappings {
            match map_key(event(KeyCode::Char(key)), base(View::Home)) {
                Some(Action::SwitchView(view)) => assert_eq!(view, expected),
                action => panic!("unexpected action for {key}: {action:?}"),
            }
        }
    }

    #[test]
    fn maps_global_focus_and_overlay_actions() {
        assert!(matches!(
            map_key(event(KeyCode::Tab), base(View::Home)),
            Some(Action::CycleFocus)
        ));
        assert!(matches!(
            map_key(event(KeyCode::Char('h')), base(View::Home)),
            Some(Action::FocusMenu)
        ));
        assert!(matches!(
            map_key(event(KeyCode::Left), base(View::Home)),
            Some(Action::FocusMenu)
        ));
        assert!(matches!(
            map_key(event(KeyCode::Char('l')), base(View::Home)),
            Some(Action::FocusContent)
        ));
        assert!(matches!(
            map_key(event(KeyCode::Right), base(View::Home)),
            Some(Action::FocusContent)
        ));
        assert!(matches!(
            map_key(event(KeyCode::Char('j')), base(View::Home)),
            Some(Action::MoveNext)
        ));
        assert!(matches!(
            map_key(event(KeyCode::Down), base(View::Home)),
            Some(Action::MoveNext)
        ));
        assert!(matches!(
            map_key(event(KeyCode::Char('k')), base(View::Home)),
            Some(Action::MovePrevious)
        ));
        assert!(matches!(
            map_key(event(KeyCode::Up), base(View::Home)),
            Some(Action::MovePrevious)
        ));
        assert!(matches!(
            map_key(event(KeyCode::Char('/')), base(View::Connections)),
            Some(Action::StartFilter)
        ));
        assert!(matches!(
            map_key(event(KeyCode::Char('?')), base(View::Home)),
            Some(Action::ToggleHelp)
        ));
        assert!(matches!(
            map_key(event(KeyCode::Esc), base(View::Home)),
            Some(Action::DismissOverlay)
        ));
        assert!(matches!(
            map_key(event(KeyCode::Char('q')), base(View::Home)),
            Some(Action::Quit)
        ));
    }

    #[test]
    fn overlays_dismiss_before_quit_and_filter_submits_explicitly() {
        let filter = KeyContext {
            overlay: Some(Overlay::Filter),
            ..base(View::Connections)
        };
        assert!(matches!(
            map_key(event(KeyCode::Esc), filter),
            Some(Action::DismissOverlay)
        ));
        assert!(matches!(
            map_key(event(KeyCode::Char('q')), filter),
            Some(Action::DismissOverlay)
        ));
        assert!(matches!(
            map_key(event(KeyCode::Enter), filter),
            Some(Action::SubmitFilter)
        ));

        let help = KeyContext {
            overlay: Some(Overlay::Help),
            ..base(View::Home)
        };
        assert!(matches!(
            map_key(event(KeyCode::Esc), help),
            Some(Action::DismissOverlay)
        ));
        assert!(matches!(
            map_key(event(KeyCode::Char('q')), help),
            Some(Action::DismissOverlay)
        ));
    }

    #[test]
    fn connection_close_requires_a_target_named_confirmation() {
        let connection = base(View::Connections);
        assert!(matches!(
            map_key(event(KeyCode::Char('d')), connection),
            Some(Action::RequestCloseConnection)
        ));

        let confirmation = KeyContext {
            overlay: Some(Overlay::CloseConfirmation),
            pending_connection_close: Some("conn-42"),
            ..connection
        };
        assert!(matches!(
            map_key(event(KeyCode::Enter), confirmation),
            Some(Action::ConfirmCloseConnection(id)) if id == "conn-42"
        ));
        assert!(matches!(
            map_key(event(KeyCode::Esc), confirmation),
            Some(Action::DismissOverlay)
        ));
    }

    #[test]
    fn proxy_chain_shortcuts_map_on_the_proxy_view() {
        let proxies = base(View::Proxies);
        assert!(matches!(
            map_key(event(KeyCode::Char('c')), proxies),
            Some(Action::ToggleChainMode)
        ));
        assert!(matches!(
            map_key(event(KeyCode::Char('a')), proxies),
            Some(Action::ApplyChain)
        ));
        assert!(matches!(
            map_key(event(KeyCode::Char('x')), proxies),
            Some(Action::ClearChain)
        ));
    }

    #[test]
    fn context_hints_follow_the_active_focus_region() {
        assert_eq!(
            context_hint(View::Connections, Focus::Menu, Language::English),
            "Enter content | j/k views"
        );
        assert_eq!(
            context_hint(View::Connections, Focus::Content, Language::English),
            "Enter/d close | / filter"
        );
    }
}
