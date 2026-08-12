use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::app::{Action, EditorTarget, Focus, Overlay, View};
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
            (Overlay::CloseAllConnectionsConfirmation, KeyCode::Enter) => Some(Action::ConfirmCloseAllConnections),
            // The password is opaque UTF-8 data: every printable character
            // (including `q`) enters the buffer. Esc is the only cancel key.
            (Overlay::PasswordInput, KeyCode::Char(c)) => Some(Action::PasswordChar(c)),
            (Overlay::PasswordInput, KeyCode::Backspace) => Some(Action::PasswordBackspace),
            (Overlay::PasswordInput, KeyCode::Enter) => Some(Action::PasswordSubmit),
            (Overlay::PasswordInput, KeyCode::Esc) => Some(Action::PasswordCancel),
            // Trust confirmation is an explicit opt-in: only `y` retries the
            // import with the host in `trusted_hosts`; `n`/Esc cancel without
            // saving any trust. `q` still dismisses via the generic fallback.
            (Overlay::TrustConfirmation, KeyCode::Char('y')) | (Overlay::TrustConfirmation, KeyCode::Char('Y')) => {
                Some(Action::ConfirmTrustImport)
            }
            (Overlay::TrustConfirmation, KeyCode::Char('n')) | (Overlay::TrustConfirmation, KeyCode::Char('N')) => {
                Some(Action::CancelTrustImport)
            }
            (Overlay::TrustConfirmation, KeyCode::Esc) => Some(Action::CancelTrustImport),
            // Core-start TUN setup confirm is an explicit opt-in: `y` opens
            // the existing password popup (setup then resumes the pending
            // start); `n`/Esc/`q` dismiss and start anyway — the app never
            // falls through to a system polkit dialog.
            (Overlay::TunSetupConfirmation, KeyCode::Char('y'))
            | (Overlay::TunSetupConfirmation, KeyCode::Char('Y')) => Some(Action::ConfirmTunSetup),
            (Overlay::TunSetupConfirmation, KeyCode::Char('n'))
            | (Overlay::TunSetupConfirmation, KeyCode::Char('N'))
            | (Overlay::TunSetupConfirmation, KeyCode::Esc)
            | (Overlay::TunSetupConfirmation, KeyCode::Char('q')) => Some(Action::SkipTunSetupStart),
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
        KeyCode::Char('m') if context.view == View::Home || context.view == View::Settings => {
            Some(Action::CycleClashMode)
        }
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
        KeyCode::Char('D') if context.view == View::Connections && context.focus == Focus::Content => {
            Some(Action::RequestCloseAllConnections)
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

        // Rules
        KeyCode::Char('r') if context.view == View::Rules => Some(Action::RulesRefresh),
        KeyCode::Char('u') if context.view == View::Rules && context.focus == Focus::Content => {
            // Update the selected rule provider.
            None // handled by event loop's Activate/Update on Rules
        }

        // Settings editor
        KeyCode::Char('e') if context.view == View::Settings && context.focus == Focus::Content => {
            Some(Action::OpenEditor(EditorTarget::Verge))
        }

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
    fn batch_delay_shortcut_maps_on_the_proxy_view() {
        let proxies = base(View::Proxies);
        // Shift+T is the one-key batch delay test.
        assert!(matches!(
            map_key(KeyEvent::new(KeyCode::Char('T'), KeyModifiers::SHIFT), proxies),
            Some(Action::NodeDelayAll)
        ));
        // Plain t keeps the single-node delay test.
        assert!(matches!(
            map_key(event(KeyCode::Char('t')), proxies),
            Some(Action::NodeDelayTest)
        ));
        // The batch shortcut only exists on the Proxies view.
        assert!(!matches!(
            map_key(KeyEvent::new(KeyCode::Char('T'), KeyModifiers::SHIFT), base(View::Home)),
            Some(Action::NodeDelayAll)
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
        assert_eq!(
            context_hint(View::Proxies, Focus::Content, Language::English),
            "Enter group/node | t delay | T all | c chain"
        );
    }

    fn password_context() -> KeyContext<'static> {
        KeyContext {
            view: View::Settings,
            focus: Focus::Content,
            overlay: Some(Overlay::PasswordInput),
            pending_connection_close: None,
        }
    }

    fn trust_context() -> KeyContext<'static> {
        KeyContext {
            view: View::Profiles,
            focus: Focus::Content,
            overlay: Some(Overlay::TrustConfirmation),
            pending_connection_close: None,
        }
    }

    fn tun_setup_confirm_context() -> KeyContext<'static> {
        KeyContext {
            view: View::Home,
            focus: Focus::Content,
            overlay: Some(Overlay::TunSetupConfirmation),
            pending_connection_close: None,
        }
    }

    #[test]
    fn tun_setup_confirm_y_opens_setup_and_n_escalates() {
        // `y` opens the password popup (setup then resumes the start); `n`,
        // `N`, Esc, and `q` all skip the setup and start anyway — no key may
        // fall through to the generic dismiss that would strand the pending
        // start.
        assert!(matches!(
            map_key(event(KeyCode::Char('y')), tun_setup_confirm_context()),
            Some(Action::ConfirmTunSetup)
        ));
        assert!(matches!(
            map_key(event(KeyCode::Char('Y')), tun_setup_confirm_context()),
            Some(Action::ConfirmTunSetup)
        ));
        for code in [KeyCode::Char('n'), KeyCode::Char('N'), KeyCode::Esc, KeyCode::Char('q')] {
            assert!(
                matches!(
                    map_key(event(code), tun_setup_confirm_context()),
                    Some(Action::SkipTunSetupStart)
                ),
                "key {code:?} must skip setup and start anyway"
            );
        }
    }

    #[test]
    fn tun_setup_confirm_does_not_swallow_other_keys_as_confirmation() {
        // No other key may confirm the setup: accidental keystrokes must not
        // open the password popup or skip the start.
        for code in [
            KeyCode::Enter,
            KeyCode::Char('a'),
            KeyCode::Char(' '),
            KeyCode::Backspace,
            KeyCode::Char('1'),
        ] {
            assert!(
                map_key(event(code), tun_setup_confirm_context()).is_none(),
                "key {code:?} must not confirm or skip the setup"
            );
        }
    }

    #[test]
    fn trust_prompt_requires_explicit_y_and_n_escalates() {
        // Only `y` retries the import; `n` and Esc cancel. `q` falls through
        // to the generic dismiss (which also drops the pending trust).
        assert!(matches!(
            map_key(event(KeyCode::Char('y')), trust_context()),
            Some(Action::ConfirmTrustImport)
        ));
        assert!(matches!(
            map_key(event(KeyCode::Char('Y')), trust_context()),
            Some(Action::ConfirmTrustImport)
        ));
        assert!(matches!(
            map_key(event(KeyCode::Char('n')), trust_context()),
            Some(Action::CancelTrustImport)
        ));
        assert!(matches!(
            map_key(event(KeyCode::Char('N')), trust_context()),
            Some(Action::CancelTrustImport)
        ));
        assert!(matches!(
            map_key(event(KeyCode::Esc), trust_context()),
            Some(Action::CancelTrustImport)
        ));
        assert!(matches!(
            map_key(event(KeyCode::Char('q')), trust_context()),
            Some(Action::DismissOverlay)
        ));
    }

    #[test]
    fn trust_prompt_does_not_swallow_other_keys_as_confirmation() {
        // No other key may confirm the trust: accidental keystrokes must not
        // retry the import or save an exception.
        for code in [
            KeyCode::Enter,
            KeyCode::Char('a'),
            KeyCode::Char(' '),
            KeyCode::Backspace,
        ] {
            assert!(
                map_key(event(code), trust_context()).is_none(),
                "key {code:?} must not confirm trust"
            );
        }
    }

    #[test]
    fn q_enters_the_password_buffer_like_any_printable_char() {
        // Regression: `q` used to cancel the popup, which destroyed valid
        // passwords containing q. The password is opaque data — only Esc
        // cancels now.
        assert!(matches!(
            map_key(event(KeyCode::Char('q')), password_context()),
            Some(Action::PasswordChar('q'))
        ));
    }

    #[test]
    fn esc_is_the_only_password_cancel_key() {
        // Esc cancels; q must never map to PasswordCancel.
        assert!(matches!(
            map_key(event(KeyCode::Esc), password_context()),
            Some(Action::PasswordCancel)
        ));
    }

    #[test]
    fn other_password_characters_still_enter_the_buffer() {
        assert!(matches!(
            map_key(event(KeyCode::Char('p')), password_context()),
            Some(Action::PasswordChar('p'))
        ));
        assert!(matches!(
            map_key(event(KeyCode::Backspace), password_context()),
            Some(Action::PasswordBackspace)
        ));
        assert!(matches!(
            map_key(event(KeyCode::Enter), password_context()),
            Some(Action::PasswordSubmit)
        ));
    }
}
