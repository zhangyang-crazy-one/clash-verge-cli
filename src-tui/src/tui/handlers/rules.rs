//! Rules view: rules and rule providers, plus sing-box / rule-edit / DNS-edit
//! flows.

use crate::app::{Action, App, InputMode, Overlay};
use crate::tui::handlers::Ctx;

/// Load the active remote profile's rule list into the edit buffer. Used by
/// `toggle_edit_mode` on entry. Kept private to this module so it does not
/// widen the surface owned by other workers (it never spawns, only reads
/// the profile store and the YAML file on disk).
async fn load_edit_buffer_from_active_profile(app: &mut App) -> Result<(), String> {
    let store = crate::profile_store::store::ProfileStore::snapshot()
        .await
        .map_err(|e| e.to_string())?;
    let uid = store.current_uid();
    let item = store.items().into_iter().find(|i| i.uid == uid);
    let Some(item) = item else {
        return Err("no active profile".into());
    };
    let Some(file) = item.file.as_deref() else {
        return Err("no active remote profile to edit".into());
    };
    let path = clash_verge_core::utils::dirs::app_profiles_dir()
        .map_err(|e| e.to_string())?
        .join(file);
    let yaml = std::fs::read_to_string(&path).map_err(|e| format!("read profile: {e}"))?;
    let rules = crate::routing::load_profile_rules(&yaml).map_err(|e| e.to_string())?;
    app.rules_edit_buffer = rules;
    if let Ok(home) = clash_verge_core::utils::dirs::app_home_dir() {
        app.rule_sets_edit = crate::singbox::load_rule_sets(&home);
    }
    app.rules_selected_index = 0;
    app.rules_edit_mode = true;
    app.rules_edit_dirty = false;
    Ok(())
}

/// Persist the in-memory DNS spec edit to disk.
pub(super) fn persist_dns_spec(app: &App) -> Result<(), String> {
    let home = clash_verge_core::utils::dirs::app_home_dir().map_err(|e| e.to_string())?;
    crate::singbox::save_dns_spec(&home, &app.dns_spec_edit)
}

/// Length of the DNS list currently focused in the editor (task 8.1).
fn dns_list_len(app: &App) -> usize {
    if app.dns_focus_rules {
        app.dns_spec_edit.rules.len()
    } else {
        app.dns_spec_edit.servers.len()
    }
}

pub(super) fn refresh_rules(app: &mut App, ctx: &Ctx) {
    app.rules_loading = true;
    app.rules_error = None;
    let api = ctx.manager.api();
    ctx.spawn_result(
        async move { api.get_rules().await },
        |resp| Action::RulesFetched(resp.rules),
        |error| Action::RulesFailed(error.to_string()),
    );
}

pub(super) fn note_rules(app: &mut App, rules: Vec<crate::mihomo_api::types::Rule>) {
    app.rules_loading = false;
    app.rules = rules;
    app.rules_selected_index = app
        .rules_selected_index
        .min(app.visible_rules_panel_len().saturating_sub(1));
}

pub(super) fn refresh_providers(app: &mut App, ctx: &Ctx) {
    app.rule_providers_loading = true;
    app.rule_providers_error = None;
    let api = ctx.manager.api();
    ctx.spawn_result(
        async move { api.get_rule_providers().await },
        |resp| Action::RuleProvidersFetched(resp.providers.into_values().collect()),
        |error| Action::RuleProvidersFailed(error.to_string()),
    );
}

pub(super) fn note_providers(app: &mut App, mut providers: Vec<crate::mihomo_api::types::RuleProvider>) {
    app.rule_providers_loading = false;
    // The API returns a map: sort so the cursor stays on the same provider.
    providers.sort_by(|left, right| left.name.cmp(&right.name));
    app.rule_providers = providers;
    app.rules_selected_index = app
        .rules_selected_index
        .min(app.visible_rules_panel_len().saturating_sub(1));
}

/// `Enter` with the providers panel focused: update that provider.
pub(super) fn update_selected_provider(app: &mut App, ctx: &Ctx) {
    if !app.rules_focus_providers {
        return;
    }
    let Some(provider) = app.visible_rule_providers().get(app.rules_selected_index).copied() else {
        return;
    };
    let name = provider.name.clone();
    app.status_msg = Some(format!("Updating rule provider {name}..."));
    let api = ctx.manager.api();
    ctx.spawn(|tx| async move {
        let _ = tx.send(match api.update_rule_provider(&name).await {
            Ok(()) => Action::RuleProviderUpdated(name),
            Err(error) => Action::RuleProviderUpdateFailed {
                name,
                error: error.to_string(),
            },
        });
    });
}

// --- Task 7.1 / 7.5: profile rule editing -------------------------------

/// `E` on Rules: enter/exit profile rule edit mode. The buffer is loaded
/// from the current profile's YAML on entry.
pub(super) async fn toggle_edit_mode(app: &mut App) {
    if app.rules_edit_mode {
        app.rules_edit_mode = false;
        app.rules_edit_buffer.clear();
        app.rules_edit_dirty = false;
        app.status_msg = Some("rule editing off".into());
        return;
    }
    match load_edit_buffer_from_active_profile(app).await {
        Ok(()) => {
            app.status_msg = Some("rule editing ON - D del, J/K move, A raw, F form, W save, E exit".into());
        }
        Err(message) => app.status_msg = Some(message),
    }
}

/// `A` on Rules: open the raw clash rule-string input.
pub(super) fn open_rule_input(app: &mut App) {
    if app.rules_edit_mode {
        app.input_mode = InputMode::RuleInput(String::new());
        app.status_msg = Some("rule: TYPE,PAYLOAD,PROXY (or sing-box JSON)".into());
    }
}

/// `r` on Rules (uppercase): open the rule-set definition input.
pub(super) fn open_rule_set_input(app: &mut App) {
    if app.rules_edit_mode {
        app.input_mode = InputMode::RuleSetInput(String::new());
    }
}

/// `x` on Rules: delete the selected rule-set.
pub(super) fn delete_rule_set(app: &mut App) {
    if !app.rules_edit_mode {
        return;
    }
    if let Ok(home) = clash_verge_core::utils::dirs::app_home_dir() {
        let mut sets = crate::singbox::load_rule_sets(&home);
        if sets.is_empty() {
            app.status_msg = Some("no rule-sets defined".into());
            return;
        }
        let i = app.rules_selected_index.min(sets.len().saturating_sub(1));
        sets.remove(i);
        let _ = crate::singbox::save_rule_sets(&home, &sets);
        app.rule_sets_edit = sets;
        app.status_msg = Some("rule-set removed".into());
    }
}

/// `D` on Rules: delete the selected rule from the edit buffer.
pub(super) fn delete_selected_rule(app: &mut App) {
    if !app.rules_edit_mode {
        return;
    }
    if app.rules_selected_index < app.rules_edit_buffer.len() {
        app.rules_edit_buffer.remove(app.rules_selected_index);
        app.rules_edit_dirty = true;
        app.rules_selected_index = app
            .rules_selected_index
            .min(app.rules_edit_buffer.len().saturating_sub(1));
    }
}

/// `J` on Rules: move the selected rule one step down.
pub(super) fn move_rule_down(app: &mut App) {
    if !app.rules_edit_mode {
        return;
    }
    let i = app.rules_selected_index;
    if i + 1 < app.rules_edit_buffer.len() {
        app.rules_edit_buffer.swap(i, i + 1);
        app.rules_selected_index = i + 1;
        app.rules_edit_dirty = true;
    }
}

/// `K` on Rules: move the selected rule one step up.
pub(super) fn move_rule_up(app: &mut App) {
    if !app.rules_edit_mode {
        return;
    }
    let i = app.rules_selected_index;
    if i > 0 && i < app.rules_edit_buffer.len() {
        app.rules_edit_buffer.swap(i - 1, i);
        app.rules_selected_index = i - 1;
        app.rules_edit_dirty = true;
    }
}

/// `W` on Rules: persist the buffer. Under sing-box the user must confirm
/// the core restart first.
pub(super) fn save_rules(app: &mut App, ctx: &Ctx) {
    if !app.rules_edit_mode || !app.rules_edit_dirty {
        return;
    }
    if ctx.manager.core_kind() == crate::mihomo_manager::CoreKind::SingBox {
        app.overlay = Some(Overlay::RulesRestartConfirmation);
        app.status_msg = Some("saving will regenerate config & restart sing-box".into());
    } else {
        let enable_tun = app.gui_config.enable_tun_mode.unwrap_or(false);
        let m = ctx.manager.clone();
        let tx = ctx.tx.clone();
        let buffer = std::mem::take(&mut app.rules_edit_buffer);
        spawn_rules_save(m, enable_tun, buffer, tx);
    }
}

/// `y` on the restart confirmation: same as save_rules under sing-box.
pub(super) fn confirm_save_rules(app: &mut App, ctx: &Ctx) {
    if app.overlay != Some(Overlay::RulesRestartConfirmation) {
        return;
    }
    app.overlay = None;
    let enable_tun = app.gui_config.enable_tun_mode.unwrap_or(false);
    let m = ctx.manager.clone();
    let tx = ctx.tx.clone();
    let buffer = std::mem::take(&mut app.rules_edit_buffer);
    spawn_rules_save(m, enable_tun, buffer, tx);
}

/// `n` / Esc on the restart confirmation: keep the edits in the buffer.
pub(super) fn cancel_save_rules(app: &mut App) {
    if app.overlay == Some(Overlay::RulesRestartConfirmation) {
        app.overlay = None;
        app.status_msg = Some("save cancelled - edits kept".into());
    }
}

/// `F` on Rules: open the structured rule form input.
pub(super) fn open_rule_form(app: &mut App) {
    if app.rules_edit_mode {
        app.input_mode = InputMode::RuleFormInput(String::new());
        app.status_msg = Some("rule form: kind=value>target".into());
    }
}

/// Channel-receive side: a save completed; report status and exit edit mode.
pub(super) fn note_rules_saved(app: &mut App, ctx: &Ctx, message: String) {
    app.rules_edit_dirty = false;
    app.rules_edit_buffer.clear();
    app.rules_edit_mode = false;
    app.status_msg = Some(message);
    let _ = ctx.tx.send(Action::RulesRefresh);
}

/// Channel-receive side: a save failed; the buffer is cleared so the user
/// can re-enter edit mode, retry the import, or back out without stranding
/// the in-memory state.
pub(super) fn note_rules_failed(app: &mut App, error: String) {
    app.rules_edit_buffer.clear();
    app.rules_edit_mode = false;
    app.status_msg = Some(format!("rule save failed: {error}"));
}

/// Task 7.1/7.5: persist edited profile rules and apply them per core kind.
/// Clash-expressible rules go back into the profile YAML; logical rules have
/// no clash form and live in the sing-box sidecar instead.
fn spawn_rules_save(
    manager: crate::mihomo_manager::MihomoManager,
    enable_tun: bool,
    buffer: Vec<crate::routing::IRouteRule>,
    tx: tokio::sync::mpsc::UnboundedSender<Action>,
) {
    tokio::spawn(async move {
        let is_singbox = manager.core_kind() == crate::mihomo_manager::CoreKind::SingBox;
        let outcome = async {
            let store = crate::profile_store::store::ProfileStore::snapshot()
                .await
                .map_err(|e| e.to_string())?;
            let uid = store.current_uid();
            let item = store.items().into_iter().find(|i| i.uid == uid);
            let Some(item) = item else {
                return Err("no active profile".into());
            };
            let file = item.file.clone().ok_or("active profile has no file")?;
            let path = clash_verge_core::utils::dirs::app_profiles_dir()
                .map_err(|e| e.to_string())?
                .join(file.as_str());
            let yaml = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
            let (clash_rules, logical): (Vec<_>, Vec<_>) = buffer
                .into_iter()
                .partition(|r| !matches!(r, crate::routing::IRouteRule::Logical { .. }));
            if !logical.is_empty() && !is_singbox {
                return Err("logical rules require the sing-box core".into());
            }
            let saved = crate::routing::save_profile_rules(&yaml, &clash_rules)?;
            std::fs::write(&path, saved).map_err(|e| e.to_string())?;
            if !logical.is_empty() {
                let home = clash_verge_core::utils::dirs::app_home_dir().map_err(|e| e.to_string())?;
                crate::singbox::save_logical_rules(&home, &logical)?;
            }
            Ok((item, yaml))
        }
        .await;
        match outcome {
            Ok((item, yaml)) => {
                let report = if is_singbox {
                    crate::runtime_config::apply_singbox_restart(&manager, Some(yaml.as_str()), enable_tun).await
                } else {
                    crate::runtime_config::reload_remote_profile(&manager.api(), &item, enable_tun, true)
                        .await
                        .map(|_| "rules applied (hot reload)".into())
                };
                match report {
                    Ok(message) => {
                        let _ = tx.send(Action::RulesEditSaved(message));
                    }
                    Err(error) => {
                        let _ = tx.send(Action::RulesEditFailed(error));
                    }
                }
            }
            Err(error) => {
                let _ = tx.send(Action::RulesEditFailed(error));
            }
        }
    });
}

// --- Task 8.1: sing-box DNS editor --------------------------------------

/// `d` on Settings: enter/exit the DNS edit mode.
pub(super) fn toggle_dns_edit(app: &mut App) {
    if app.dns_edit_mode {
        app.dns_edit_mode = false;
        app.status_msg = Some("sing-box DNS editor off".into());
        return;
    }
    let home = clash_verge_core::utils::dirs::app_home_dir();
    app.dns_spec_edit = home
        .ok()
        .and_then(|home| crate::singbox::load_dns_spec(&home))
        .unwrap_or_default();
    app.dns_cursor = 0;
    app.dns_focus_rules = false;
    app.dns_edit_mode = true;
    app.status_msg =
        Some("sing-box DNS editor ON - a server | r rule | R resolver | x del | Tab lists | w apply | d exit".into());
}

/// `Tab` (BackTab) on Settings: switch DNS focus between servers and split rules.
pub(super) fn toggle_dns_focus(app: &mut App) {
    if !app.dns_edit_mode {
        return;
    }
    app.dns_focus_rules = !app.dns_focus_rules;
    app.dns_cursor = 0;
    let target = if app.dns_focus_rules { "split rules" } else { "servers" };
    app.status_msg = Some(format!("DNS editor focus: {target}"));
}

/// `a` on Settings: open the DNS server spec input.
pub(super) fn open_dns_server_input(app: &mut App) {
    if app.dns_edit_mode {
        app.input_mode = InputMode::DnsServerInput(String::new());
    }
}

/// `r` on Settings: open the DNS rule spec input.
pub(super) fn open_dns_rule_input(app: &mut App) {
    if app.dns_edit_mode {
        app.input_mode = InputMode::DnsRuleInput(String::new());
    }
}

/// `R` on Settings: open the bootstrap resolver input.
pub(super) fn open_dns_resolver_input(app: &mut App) {
    if app.dns_edit_mode {
        let current = app.dns_spec_edit.domain_resolver.clone().unwrap_or_default();
        app.input_mode = InputMode::DnsResolverInput(current);
    }
}

/// `x` on Settings: delete the focused DNS entry.
pub(super) fn delete_dns_entry(app: &mut App) {
    if !app.dns_edit_mode {
        return;
    }
    let len = dns_list_len(app);
    if app.dns_cursor >= len {
        app.status_msg = Some("nothing to delete at cursor".into());
        return;
    }
    let removed_msg = if app.dns_focus_rules {
        let rule = app.dns_spec_edit.rules.remove(app.dns_cursor);
        Some(format!("dns rule -> {} removed", rule.server))
    } else {
        let server = app.dns_spec_edit.servers.remove(app.dns_cursor);
        Some(format!("dns server {} removed", server.tag))
    };
    match removed_msg {
        Some(message) => {
            app.dns_cursor = app.dns_cursor.min(dns_list_len(app).saturating_sub(1));
            match persist_dns_spec(app) {
                Ok(()) => app.status_msg = Some(message),
                Err(error) => app.status_msg = Some(format!("persist dns: {error}")),
            }
        }
        None => app.status_msg = Some("nothing to delete at cursor".into()),
    }
}

/// `w` on Settings: regenerate the sing-box config and restart it.
pub(super) fn apply_dns_edit(app: &mut App, ctx: &Ctx) {
    if !app.dns_edit_mode {
        return;
    }
    if ctx.manager.core_kind() != crate::mihomo_manager::CoreKind::SingBox {
        app.status_msg = Some("switch proxy core to sing-box before applying DNS".into());
        return;
    }
    let m = ctx.manager.clone();
    let tx = ctx.tx.clone();
    tokio::spawn(async move {
        match crate::runtime_config::apply_singbox_active_reload(&m).await {
            Ok(message) => {
                let _ = tx.send(Action::DnsApplied(message));
            }
            Err(error) => {
                let _ = tx.send(Action::DnsApplyFailed(error));
            }
        }
    });
    app.status_msg = Some("regenerating config & restarting sing-box...".into());
}

/// Channel-receive side: sing-box DNS apply succeeded.
pub(super) fn note_dns_applied(app: &mut App, message: String) {
    app.status_msg = Some(message);
}

/// Channel-receive side: sing-box DNS apply failed.
pub(super) fn note_dns_failed(app: &mut App, error: String) {
    app.status_msg = Some(format!("dns apply failed: {error}"));
}
