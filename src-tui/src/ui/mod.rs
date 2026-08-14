pub mod dialog;
pub mod input_bar;
pub mod profile_list;
pub mod proxy_list;
pub mod split_view;
pub mod status_bar;
pub mod terminal_text;
pub mod theme;
pub mod views;

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, ListState};

use crate::app::{App, Focus, Overlay, View};

pub fn draw(frame: &mut ratatui::Frame<'_>, app: &App) {
    let shell = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(0), Constraint::Length(1)])
        .split(frame.area());

    status_bar::draw(frame, shell[0], app);

    let main = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(nav_width(app)), Constraint::Min(0)])
        .split(shell[1]);

    draw_navigation(frame, main[0], app);
    views::draw(frame, main[1], app);
    input_bar::draw(frame, shell[2], app);
    draw_overlay(frame, app);
}

/// Nav column width: longest localized view label + 6, covering the block
/// borders (2), the `> ` highlight symbol (2), and the `1 ` number prefix (2),
/// clamped to a usable band regardless of locale.
fn nav_width(app: &App) -> u16 {
    let longest = View::ALL
        .iter()
        .map(|view| Line::from(view.localized_label(app.language)).width())
        .max()
        .unwrap_or(0);
    (longest + 6).clamp(17, 26) as u16
}

fn draw_navigation(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    let selected = View::ALL.iter().position(|view| *view == app.view).unwrap_or_default();
    let menu_focused = app.focus == Focus::Menu;
    let items = View::ALL.iter().enumerate().map(|(index, view)| {
        ListItem::new(Line::from(vec![
            Span::styled(format!("{} ", index + 1), Style::new().fg(theme::dim())),
            Span::raw(view.localized_label(app.language)),
        ]))
    });
    let list = List::new(items)
        .block(theme::panel_block(app.tr("menu"), menu_focused))
        .highlight_style(theme::highlight(menu_focused))
        .highlight_symbol("> ");
    let mut state = ListState::default().with_selected(Some(selected));
    frame.render_stateful_widget(list, area, &mut state);
}

fn draw_overlay(frame: &mut ratatui::Frame<'_>, app: &App) {
    let Some(overlay) = app.overlay else {
        return;
    };
    if overlay == Overlay::Filter {
        return;
    }

    match overlay {
        Overlay::Help => {
            let content = vec![
                Line::from(Span::styled(app.tr("help.global"), theme::bold(theme::accent()))),
                Line::from(app.tr("help.global_commands")),
                Line::from(app.tr("help.global_commands_more")),
                Line::from(""),
                Line::from(Span::styled(app.tr("help.current_view"), theme::bold(theme::accent()))),
                Line::from(crate::tui::input::context_hint(app.view, app.focus, app.language)),
                Line::from(""),
                Line::from(Span::styled(app.tr("help.close"), Style::new().fg(theme::dim()))),
            ];
            dialog::draw_dialog(frame, frame.area(), dialog::DialogKind::Info, app.tr("help"), content);
        }
        Overlay::CloseConfirmation => {
            let target = app
                .pending_connection_close
                .as_deref()
                .unwrap_or(app.tr("common.unknown"));
            let content = vec![
                Line::from(Span::styled(
                    app.tr("dialog.confirm_close"),
                    theme::bold(theme::danger()),
                )),
                Line::from(format!("{}: {target}", app.tr("dialog.target"))),
                Line::from(""),
                Line::from(Span::styled(
                    app.tr("input.confirm_cancel").trim_start_matches(" | "),
                    Style::new().fg(theme::dim()),
                )),
            ];
            dialog::draw_dialog(
                frame,
                frame.area(),
                dialog::DialogKind::Danger,
                app.tr("input.close"),
                content,
            );
        }
        Overlay::Filter => {}
        Overlay::TrustConfirmation => {
            let is_update = app.pending_trust.as_ref().is_some_and(|pending| pending.uid.is_some());
            let host = app
                .pending_trust
                .as_ref()
                .map(|pending| pending.host.as_str())
                .unwrap_or(app.tr("common.unknown"));
            let title = app.tr(if is_update {
                "dialog.trust_update_title"
            } else {
                "dialog.trust_title"
            });
            let warning = app.tr(if is_update {
                "dialog.trust_update_warning"
            } else {
                "dialog.trust_warning"
            });
            let confirm = app.tr(if is_update {
                "dialog.trust_update_confirm"
            } else {
                "dialog.trust_confirm"
            });
            let dialog_title = app.tr(if is_update {
                "dialog.trust_update"
            } else {
                "dialog.trust"
            });
            let content = vec![
                Line::from(Span::styled(title, theme::bold(theme::danger()))),
                Line::from(format!("{}: {host}", app.tr("dialog.target"))),
                Line::from(""),
                Line::from(Span::styled(warning, Style::new().fg(theme::warn()))),
                Line::from(""),
                Line::from(Span::styled(confirm, Style::new().fg(theme::dim()))),
            ];
            dialog::draw_dialog(frame, frame.area(), dialog::DialogKind::Warn, dialog_title, content);
        }
        Overlay::CloseAllConnectionsConfirmation => {
            let content = vec![
                Line::from(Span::styled("Close ALL connections?", theme::bold(theme::danger()))),
                Line::from("This will terminate every active connection."),
                Line::from(""),
                Line::from(Span::styled(
                    "Enter = confirm | Esc/q = cancel",
                    Style::new().fg(theme::dim()),
                )),
            ];
            dialog::draw_dialog(frame, frame.area(), dialog::DialogKind::Danger, "Close All", content);
        }
        Overlay::TunSetupConfirmation => {
            let content = vec![
                Line::from(Span::styled(
                    app.tr("dialog.tun_setup_title"),
                    theme::bold(theme::warn()),
                )),
                Line::from(app.tr("dialog.tun_setup_warning")),
                Line::from(""),
                Line::from(Span::styled(
                    app.tun_setup_confirm_hint(),
                    Style::new().fg(theme::dim()),
                )),
            ];
            dialog::draw_dialog(
                frame,
                frame.area(),
                dialog::DialogKind::Warn,
                app.tr("dialog.tun_setup"),
                content,
            );
        }
        Overlay::ServiceUninstallConfirmation => {
            let content = vec![
                Line::from(Span::styled(
                    app.tr("dialog.service_uninstall_title"),
                    theme::bold(theme::danger()),
                )),
                Line::from(app.tr("dialog.service_uninstall_warning")),
                Line::from(""),
                Line::from(Span::styled(
                    app.tr("dialog.service_uninstall_confirm"),
                    Style::new().fg(theme::dim()),
                )),
            ];
            dialog::draw_dialog(
                frame,
                frame.area(),
                dialog::DialogKind::Danger,
                app.tr("dialog.service_uninstall"),
                content,
            );
        }
        Overlay::PasswordInput => {
            let prompt = app.password_prompt.as_deref().unwrap_or("sudo");
            let masked = dialog::mask_password(app.password_buffer.len());
            let content = vec![
                Line::from(Span::styled(prompt, theme::bold(theme::warn()))),
                Line::from(vec![
                    Span::styled(app.tr("dialog.password.prompt"), theme::bold(theme::text())),
                    Span::styled(masked, Style::new().fg(theme::text())),
                ]),
                Line::from(""),
                Line::from(Span::styled(
                    app.tr("dialog.password.hint"),
                    Style::new().fg(theme::dim()),
                )),
            ];
            dialog::draw_dialog(
                frame,
                frame.area(),
                dialog::DialogKind::Password,
                app.tr("dialog.password.title"),
                content,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use clash_verge_core::config::PrfItem;
    use ratatui::{Terminal, backend::TestBackend};

    use super::*;
    use crate::app::CoreState;
    use crate::mihomo_api::types::{ConnectionInfo, ConnectionMeta, LogEntry, ProxyGroup, TrafficData};

    fn buffer_text(terminal: &Terminal<TestBackend>) -> String {
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    fn row_text(terminal: &Terminal<TestBackend>, row: u16) -> String {
        let buffer = terminal.backend().buffer();
        (0..buffer.area.width)
            .map(|column| buffer[(column, row)].symbol())
            .collect()
    }

    fn render(app: &App, width: u16, height: u16) -> (String, Vec<String>) {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal.draw(|frame| draw(frame, app)).expect("draw shell");
        let rows = (0..height).map(|row| row_text(&terminal, row)).collect();
        (buffer_text(&terminal), rows)
    }

    fn representative_app() -> App {
        let mut app = App::new();
        app.core_state = CoreState::Running;
        app.core_pid = Some(4242);
        app.core_version = Some("mihomo-test".to_string());
        app.focus = Focus::Content;
        app.status_msg = Some("Connected to mihomo API".to_string());
        app.profiles.push(PrfItem {
            name: Some("Sample Subscription".into()),
            itype: Some("remote".into()),
            url: Some("https://example.test/sub".into()),
            ..Default::default()
        });
        app.traffic = Some(TrafficData {
            up: 42_000,
            down: 81_000,
        });
        app.gui_config.enable_system_proxy = Some(true);
        app.gui_config.enable_tun_mode = Some(false);
        app.gui_config.enable_dns_settings = Some(false);
        app.gui_config.proxy_host = Some("127.0.0.1".into());
        app.core_config.0.insert("mode".into(), "global".into());
        app.clash_mode = "global".into();
        app.proxy_groups.insert(
            "Auto".to_string(),
            ProxyGroup {
                group_type: "Selector".to_string(),
                now: Some("Tokyo".to_string()),
                all: Some(vec!["Tokyo".to_string(), "Singapore".to_string()]),
                history: None,
            },
        );
        app.expanded_proxy_group = Some("Auto".to_string());
        app.connections = vec![ConnectionInfo {
            id: "conn-42".to_string(),
            metadata: Some(ConnectionMeta {
                host: Some("api.example.test".to_string()),
                network: Some("tcp".to_string()),
            }),
            upload: 128,
            download: 256,
            start: "2026-07-11T03:30:00Z".to_string(),
            rule: Some("MATCH".to_string()),
            chains: Some(vec!["Auto".to_string(), "Tokyo".to_string()]),
        }];
        app.selected_connection_id = Some("conn-42".to_string());
        app.logs = vec![
            LogEntry {
                level: "info".to_string(),
                payload: "proxy selected".to_string(),
            },
            LogEntry {
                level: "error".to_string(),
                payload: "dial failure".to_string(),
            },
        ];
        app
    }

    #[test]
    fn nav_width_covers_borders_highlight_and_number_prefix() {
        let mut app = App::new();
        app.language = crate::i18n::Language::English;
        // "Connections" (11 cells) + 6 (borders 2 + "> " 2 + "1 " 2) = 17;
        // the longest selected row never truncates.
        assert_eq!(nav_width(&app), 17);

        app.language = crate::i18n::Language::SimplifiedChinese;
        // CJK labels are 4 cells wide; the 17 minimum applies.
        assert_eq!(nav_width(&app), 17);
    }

    #[test]
    fn shell_renders_common_sizes() {
        let app = representative_app();
        for (width, height) in [(80, 24), (120, 32), (160, 40)] {
            let (rendered, rows) = render(&app, width, height);
            assert!(
                rows[0].contains("clash-verge-cli"),
                "missing top strip at {width}x{height}"
            );
            assert!(rows[0].contains("Home"), "missing active view at {width}x{height}");
            assert!(rendered.contains("Menu"), "missing navigation at {width}x{height}");
            assert!(rendered.contains("Core"), "missing workspace at {width}x{height}");
            assert!(
                rows[usize::from(height - 1)].contains("1-8 views"),
                "missing command bar at {width}x{height}"
            );
        }

        let (_, compact_rows) = render(&app, 80, 24);
        assert!(!compact_rows[0].contains("chain:"));
        assert!(!compact_rows[0].contains("up:"));

        let (_, standard_rows) = render(&app, 120, 32);
        assert!(standard_rows[0].contains("chain: off"));
        assert!(standard_rows[0].contains("up: 42000 B/s"));

        let (wide, wide_rows) = render(&app, 160, 40);
        assert!(wide_rows[0].contains("down: 81000 B/s"));
        assert!(wide.contains("Traffic"));
    }

    #[test]
    fn all_views_render() {
        let expected_markers = [
            (View::Home, "Core"),
            (View::Proxies, "Groups / Auto Nodes"),
            (View::Profiles, "Profile Detail"),
            (View::Connections, "Connection Detail"),
            (View::Rules, "Rule Providers"),
            (View::Logs, "ERROR"),
            (View::Unlock, "Limited support"),
            (View::Settings, "Runtime Settings"),
        ];

        for (view, marker) in expected_markers {
            let mut app = representative_app();
            app.view = view;
            let (rendered, rows) = render(&app, 160, 40);
            assert!(rows[0].contains(view.label()), "missing active route {view:?}");
            assert!(rendered.contains(marker), "missing {marker} in {view:?}");
        }
    }

    #[test]
    fn simplified_chinese_locale_localizes_navigation_and_settings() {
        let mut app = representative_app();
        app.language = crate::i18n::Language::SimplifiedChinese;
        app.view = View::Settings;

        let _ = render(&app, 120, 32);
        assert_eq!(View::Settings.localized_label(app.language), "设置");
        assert_eq!(app.tr("menu"), "菜单");
        assert_eq!(app.tr("settings.runtime"), "运行设置");
        assert_eq!(app.tr("settings.language"), "语言");
        assert_eq!(app.tr("settings.system"), "系统代理 / TUN");
    }

    #[test]
    fn empty_operational_lists_render_without_cursor_underflow() {
        let expected_markers = [
            (View::Profiles, "No profiles"),
            (View::Proxies, "mihomo is not running"),
            (View::Logs, "no logs received yet"),
        ];

        for (view, marker) in expected_markers {
            let mut app = App::new();
            app.view = view;
            let (rendered, _) = render(&app, 120, 32);
            assert!(rendered.contains(marker), "missing {marker} in empty {view:?}");
        }
    }

    #[test]
    fn representative_states_render_their_real_or_limited_feedback() {
        let mut app = representative_app();
        app.view = View::Home;
        app.core_state = CoreState::Stopped;
        let (stopped, _) = render(&app, 120, 32);
        assert!(stopped.contains("STOPPED"));
        assert!(stopped.contains("Press s to start the core"));

        app.core_state = CoreState::Error("socket unavailable".to_string());
        let (error, _) = render(&app, 120, 32);
        assert!(error.contains("ERROR"));
        assert!(error.contains("socket unavailable"));

        app.view = View::Proxies;
        app.node_selected_index = 0;
        let (proxies, _) = render(&app, 160, 40);
        assert!(proxies.contains("+ Auto"));
        assert!(proxies.contains("Tokyo"));

        app.view = View::Profiles;
        let (profiles, _) = render(&app, 160, 40);
        assert!(profiles.contains("Enter switch | u update | i add"));

        app.view = View::Connections;
        app.overlay = Some(Overlay::CloseConfirmation);
        app.pending_connection_close = Some("conn-42".to_string());
        let (confirmation, _) = render(&app, 120, 32);
        assert!(confirmation.contains("Target: conn-42"));
        assert!(confirmation.contains("CLOSE conn-42"));

        app.overlay = None;
        app.view = View::Logs;
        app.log_filter = Some("error".to_string());
        let (logs, _) = render(&app, 120, 32);
        assert!(logs.contains("Logs [filter: error]"));
        assert!(logs.contains("ERROR"));

        app.view = View::Rules;
        let (rules, _) = render(&app, 120, 32);
        assert!(rules.contains("Rule Providers"));

        app.view = View::Unlock;
        let (unlock, _) = render(&app, 120, 32);
        assert!(unlock.contains("Media unlock checks are not wired"));

        app.view = View::Settings;
        let (settings, _) = render(&app, 120, 32);
        assert!(settings.contains("Writable settings"));
        assert!(settings.contains("System proxy: on"));
        assert!(settings.contains("TUN: off"));
        assert!(settings.contains("Mihomo mode: global"));
        assert!(settings.contains("Core PID: 4242"));
        assert!(settings.contains("Enter toggles the highlighted setting"));
    }

    #[test]
    fn proxy_delay_failure_is_distinct_from_an_untested_node() {
        let mut app = representative_app();
        app.view = View::Proxies;
        app.node_selected_index = 1;
        app.delay_map.insert("Singapore".to_string(), None);

        let (rendered, _) = render(&app, 160, 40);
        assert!(rendered.contains("Delay: failed"));
        assert!(rendered.contains("Singapore failed"));
    }

    #[test]
    fn batch_delay_progress_renders_in_the_proxies_view() {
        let mut app = representative_app();
        app.view = View::Proxies;
        app.batch_delay = Some((2, 5));

        let (rendered, rows) = render(&app, 120, 32);
        assert!(rendered.contains("Batch delay test: 2/5"));
        assert!(rows.iter().any(|row| row.contains("2/5")));
    }

    #[test]
    fn filter_help_and_focus_states_render_visible_commands() {
        let mut app = representative_app();
        app.view = View::Connections;
        app.overlay = Some(Overlay::Filter);
        app.filter = Some("dns".to_string());
        let (filter, _) = render(&app, 120, 32);
        assert!(filter.contains("FILTER Connections: dns"));
        assert!(filter.contains("Esc cancel"));

        app.overlay = Some(Overlay::Help);
        let (help, _) = render(&app, 120, 32);
        assert!(help.contains("1-8 view | Tab focus | h/l focus | j/k move"));
        assert!(help.contains("Enter/d close | / filter"));
        assert!(help.contains("Press ? or Esc to close"));

        app.overlay = None;
        app.focus = Focus::Menu;
        let (menu, _) = render(&app, 120, 32);
        assert!(menu.contains("Enter content | j/k views"));

        app.focus = Focus::Content;
        app.view = View::Proxies;
        let (proxies, _) = render(&app, 120, 32);
        assert!(!proxies.contains("/ filter"));

        app.view = View::Logs;
        let (logs, _) = render(&app, 120, 32);
        assert!(logs.contains("/ filter"));
    }

    #[test]
    fn trust_confirmation_renders_host_warning_and_explicit_choice() {
        let mut app = representative_app();
        app.view = View::Profiles;
        app.pending_trust = Some(crate::app::TrustPending {
            url: "http://192.168.1.1/sub".to_string(),
            host: "192.168.1.1".to_string(),
            uid: None,
        });
        app.overlay = Some(Overlay::TrustConfirmation);

        let (rendered, _) = render(&app, 120, 32);
        assert!(rendered.contains("Trust Host"));
        assert!(rendered.contains("192.168.1.1"));
        assert!(rendered.contains("SSRF"));
        assert!(rendered.contains("y = trust & import"));
        assert!(rendered.contains("no trust saved"));
    }

    #[test]
    fn trust_update_confirmation_renders_update_wording() {
        // The refresh trust prompt (pending.uid set) must use the update wording
        // instead of the import wording while sharing the same overlay.
        let mut app = representative_app();
        app.view = View::Profiles;
        app.pending_trust = Some(crate::app::TrustPending {
            url: "http://192.168.1.1/sub".to_string(),
            host: "192.168.1.1".to_string(),
            uid: Some("R7iHvBBicAOz".to_string()),
        });
        app.overlay = Some(Overlay::TrustConfirmation);

        let (rendered, _) = render(&app, 120, 32);
        assert!(rendered.contains("Trust & Update"));
        assert!(rendered.contains("192.168.1.1"));
        assert!(rendered.contains("SSRF"));
        assert!(rendered.contains("y = trust & update"));
        assert!(
            !rendered.contains("y = trust & import"),
            "update prompt must not use import wording"
        );

        // Chinese locale mirrors the same split.
        app.language = crate::i18n::Language::SimplifiedChinese;
        let (zh, _) = render(&app, 120, 32);
        // CJK double-width glyphs split across buffer cells: strip the empty
        // continuation cells before searching for contiguous phrases.
        let zh_clean: String = zh.chars().filter(|c| !c.is_whitespace() && *c != '\0').collect();
        assert!(zh_clean.contains("信任并更新"));
        assert!(!zh_clean.contains("信任并导入"));
    }

    #[test]
    fn tun_setup_confirmation_renders_warning_and_choice() {
        let mut app = representative_app();
        app.view = View::Home;
        app.overlay = Some(Overlay::TunSetupConfirmation);

        let (rendered, _) = render(&app, 120, 32);
        assert!(rendered.contains("TUN Setup"));
        assert!(rendered.contains("TUN needs one-time setup"));
        assert!(rendered.contains("polkit"));
        assert!(rendered.contains("y = setup now"));
        assert!(rendered.contains("start without setup"));
    }

    #[test]
    fn service_uninstall_confirmation_renders_warning_and_choice() {
        let mut app = representative_app();
        app.view = View::Settings;
        app.overlay = Some(Overlay::ServiceUninstallConfirmation);

        let (rendered, _) = render(&app, 120, 32);
        assert!(rendered.contains("Uninstall service"));
        assert!(rendered.contains("system service"));
        assert!(rendered.contains("y = uninstall"));
        assert!(rendered.contains("n/Esc"));
    }

    #[test]
    fn settings_view_renders_service_and_autostart_rows() {
        let mut app = representative_app();
        app.view = View::Settings;
        app.service_installed = true;
        app.service_active = "active".into();
        app.service_enabled = "enabled".into();
        app.auto_launch_enabled = true;

        let (rendered, _) = render(&app, 160, 40);
        assert!(rendered.contains("System service"));
        assert!(rendered.contains("installed · enabled · running"));
        assert!(rendered.contains("Launch at login"));
        assert!(rendered.contains("Launch at login: on"));
    }

    #[test]
    fn profiles_view_shows_import_feedback_status() {
        // Import success/failure must be visible on the initiating view.
        let mut app = representative_app();
        app.view = View::Profiles;
        app.status_msg = Some("Import failed: SSRF blocked".to_string());

        let (rendered, rows) = render(&app, 120, 32);
        assert!(rendered.contains("Import failed: SSRF blocked"));
        assert!(
            rows.iter().any(|row| row.contains("Import failed")),
            "import feedback must appear as a visible row on Profiles"
        );
    }
}
