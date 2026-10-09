//! Rules view: rules and rule providers, plus sing-box / rule-edit / DNS-edit
//! flows.

use crate::app::{Action, App, InputMode, Overlay};
use crate::tui::handlers::Ctx;

/// Load the active remote profile's rule list into the edit buffer. Used by
/// `toggle_edit_mode` on entry. Kept private to this module so it does not
/// widen the surface owned by other workers (it never spawns, only reads
/// the profile store and the YAML file on disk).
///
/// Logical (AND/OR) rules live in the sing-box sidecar, not in the profile
/// YAML, so they are appended to the buffer as well (#59): without them a
/// previously saved logical rule is neither shown nor deletable, and the next
/// save would overwrite the sidecar and drop it. They are only loaded under
/// sing-box, because the mihomo save path rejects logical rules outright.
async fn load_edit_buffer_from_active_profile(app: &mut App, singbox: bool) -> Result<(), String> {
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
    app.rules_edit_buffer = compose_edit_buffer(rules, &load_logical_rules(singbox)?);
    if let Ok(home) = clash_verge_core::utils::dirs::app_home_dir() {
        app.rule_sets_edit = crate::singbox::load_rule_sets(&home)?;
    }
    app.rules_selected_index = 0;
    app.rules_edit_mode = true;
    app.rules_edit_dirty = false;
    Ok(())
}

/// Stored logical rules, or none when the active core cannot express them.
/// A broken sidecar is reported rather than silently dropped: the user must
/// fix it before saving, or the next save would overwrite it.
fn load_logical_rules(singbox: bool) -> Result<Vec<crate::routing::IRouteRule>, String> {
    if !singbox {
        return Ok(Vec::new());
    }
    let home = clash_verge_core::utils::dirs::app_home_dir().map_err(|e| e.to_string())?;
    crate::singbox::load_logical_rules(&home)
}

/// Profile rules first, stored logical rules after them — the same order the
/// sing-box generator appends them in, so the editor list matches the order
/// the core actually evaluates.
fn compose_edit_buffer(
    profile_rules: Vec<crate::routing::IRouteRule>,
    logical: &[crate::routing::IRouteRule],
) -> Vec<crate::routing::IRouteRule> {
    profile_rules.into_iter().chain(logical.iter().cloned()).collect()
}

/// Persist the split rule buffer: clash rules back into the profile YAML,
/// logical rules into the sing-box sidecar.
///
/// The sidecar is always written, and removed when no logical rule is left
/// (#59): writing only on a non-empty list left deleted rules in place, and
/// never writing it lost every rule that was already saved.
fn persist_rules(
    path: &std::path::Path,
    home: &std::path::Path,
    yaml: &str,
    buffer: Vec<crate::routing::IRouteRule>,
) -> Result<(), String> {
    let (clash_rules, logical): (Vec<_>, Vec<_>) = buffer
        .into_iter()
        .partition(|r| !matches!(r, crate::routing::IRouteRule::Logical { .. }));
    let saved = crate::routing::save_profile_rules(yaml, &clash_rules)?;
    write_atomic(path, &saved)?;
    persist_logical_rules(home, &logical)
}

/// Write the logical sidecar, or delete it when the list is empty so that a
/// user who removed every logical rule actually loses them.
fn persist_logical_rules(home: &std::path::Path, logical: &[crate::routing::IRouteRule]) -> Result<(), String> {
    if logical.is_empty() {
        return match std::fs::remove_file(home.join(crate::singbox::LOGICAL_RULES_FILE)) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(format!(
                "remove {}: {error}",
                home.join(crate::singbox::LOGICAL_RULES_FILE).display()
            )),
        };
    }
    crate::singbox::save_logical_rules(home, logical)
}

/// Write through a temporary file in the same directory and rename over the
/// target, so an interrupted save cannot leave a truncated profile.
fn write_atomic(path: &std::path::Path, body: &str) -> Result<(), String> {
    let temporary = path.with_extension("yaml.tmp");
    std::fs::write(&temporary, body).map_err(|error| format!("write {}: {error}", temporary.display()))?;
    if let Err(error) = std::fs::rename(&temporary, path) {
        let _ = std::fs::remove_file(&temporary);
        return Err(format!("replace {}: {error}", path.display()));
    }
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
    if ctx.manager.core_kind() == crate::mihomo_manager::CoreKind::SingBox {
        app.rule_providers_loading = false;
        app.rule_providers_error = Some(
            "rule providers are not exposed by the sing-box clash_api; configure native route/rule-sets instead".into(),
        );
        return;
    }
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
    if ctx.manager.core_kind() == crate::mihomo_manager::CoreKind::SingBox {
        app.status_msg = Some("rule provider refresh is unsupported by the sing-box clash_api".into());
        return;
    }
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
        let _ = tx
            .send(match api.update_rule_provider(&name).await {
                Ok(()) => Action::RuleProviderUpdated(name),
                Err(error) => Action::RuleProviderUpdateFailed {
                    name,
                    error: error.to_string(),
                },
            })
            .await;
    });
}

// --- Task 7.1 / 7.5: profile rule editing -------------------------------

/// `E` on Rules: enter/exit profile rule edit mode. The buffer is loaded
/// from the current profile's YAML (plus the stored logical rules under
/// sing-box) on entry.
pub(super) async fn toggle_edit_mode(app: &mut App, ctx: &Ctx) {
    if app.rules_edit_mode {
        app.rules_edit_mode = false;
        app.rules_edit_buffer.clear();
        app.rules_edit_dirty = false;
        app.status_msg = Some("rule editing off".into());
        return;
    }
    let singbox = ctx.manager.core_kind() == crate::mihomo_manager::CoreKind::SingBox;
    match load_edit_buffer_from_active_profile(app, singbox).await {
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
        let mut sets = match crate::singbox::load_rule_sets(&home) {
            Ok(sets) => sets,
            Err(error) => {
                app.status_msg = Some(format!("rule-set load failed: {error}"));
                return;
            }
        };
        if sets.is_empty() {
            app.status_msg = Some("no rule-sets defined".into());
            return;
        }
        let i = app.rules_selected_index.min(sets.len().saturating_sub(1));
        sets.remove(i);
        if let Err(error) = crate::singbox::save_rule_sets(&home, &sets) {
            app.status_msg = Some(format!("rule-set save failed: {error}"));
            return;
        }
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
        let tx = ctx.tx.for_current();
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
    let tx = ctx.tx.for_current();
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
    ctx.send(Action::RulesRefresh);
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
    tx: crate::tui::background::EventSender,
) {
    tx.clone().spawn(async move {
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
            let home = clash_verge_core::utils::dirs::app_home_dir().map_err(|e| e.to_string())?;
            let yaml = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
            if buffer
                .iter()
                .any(|rule| matches!(rule, crate::routing::IRouteRule::Logical { .. }))
                && !is_singbox
            {
                return Err("logical rules require the sing-box core".into());
            }
            persist_rules(&path, &home, &yaml, buffer)?;
            Ok(item)
        }
        .await;
        match outcome {
            Ok(item) => {
                let report = if is_singbox {
                    // #59: regenerate from the profile as it is NOW on disk.
                    // Passing the pre-edit snapshot here restarted sing-box
                    // with the old rules while the status bar already claimed
                    // the edits were applied.
                    restart_singbox_with_saved_rules(&manager).await
                } else {
                    crate::runtime_config::reload_remote_profile(&manager.api(), &item, enable_tun, true)
                        .await
                        .map(|_| "rules applied (hot reload)".into())
                };
                match report {
                    Ok(message) => {
                        let _ = tx.send(Action::RulesEditSaved(message)).await;
                    }
                    Err(error) => {
                        let _ = tx.send(Action::RulesEditFailed(error)).await;
                    }
                }
            }
            Err(error) => {
                let _ = tx.send(Action::RulesEditFailed(error)).await;
            }
        }
    });
}

/// Apply the saved rules to a running sing-box.
///
/// Deliberately takes no caller-supplied YAML: the profile was just rewritten,
/// so the restart must re-read it from disk (`active_profile_yaml`, which also
/// applies the profile DNS preprocessing). Handing back the pre-edit snapshot
/// is exactly the bug of issue #59.
async fn restart_singbox_with_saved_rules(manager: &crate::mihomo_manager::MihomoManager) -> Result<String, String> {
    crate::runtime_config::apply_singbox_active_reload(manager).await
}

// --- Task 8.1: sing-box DNS editor --------------------------------------

/// `d` on Settings: enter/exit the DNS edit mode.
pub(super) fn toggle_dns_edit(app: &mut App) {
    if app.dns_edit_mode {
        app.dns_edit_mode = false;
        app.status_msg = Some("sing-box DNS editor off".into());
        return;
    }
    let home = match clash_verge_core::utils::dirs::app_home_dir() {
        Ok(home) => home,
        Err(error) => {
            app.status_msg = Some(format!("sing-box DNS settings unavailable: {error}"));
            return;
        }
    };
    app.dns_spec_edit = match crate::singbox::load_dns_spec(&home) {
        Ok(spec) => spec,
        Err(error) => {
            app.status_msg = Some(format!("sing-box DNS settings invalid: {error}"));
            return;
        }
    };
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
    let tx = ctx.tx.for_current();
    tx.clone().spawn(async move {
        match crate::runtime_config::apply_singbox_active_reload(&m).await {
            Ok(message) => {
                let _ = tx.send(Action::DnsApplied(message)).await;
            }
            Err(error) => {
                let _ = tx.send(Action::DnsApplyFailed(error)).await;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routing::{IRouteRule, LogicOp, MatchField, RuleTarget};

    const PROFILE: &str = "proxies: []\nrules:\n  - DOMAIN,a.com,DIRECT\n  - MATCH,PROXY\n";

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "rules-handler-test-{}-{name}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).expect("mkdir");
        dir
    }

    fn simple(domain: &str) -> IRouteRule {
        IRouteRule::Simple {
            matches: vec![MatchField::Domain(domain.into())],
            target: RuleTarget::Direct,
        }
    }

    fn logical(domain: &str) -> IRouteRule {
        IRouteRule::Logical {
            op: LogicOp::Or,
            rules: vec![simple(domain)],
            target: RuleTarget::Block,
        }
    }

    /// #59: already-saved logical rules must show up in the editor buffer,
    /// after the profile rules — the order the sing-box generator uses.
    #[test]
    fn saved_logical_rules_are_visible_in_the_edit_buffer() {
        let buffer = compose_edit_buffer(vec![simple("a.com"), simple("b.com")], &[logical("c.com")]);
        assert_eq!(buffer.len(), 3);
        assert!(matches!(buffer[2], IRouteRule::Logical { .. }));
        assert!(crate::routing::describe(&buffer[2]).contains("OR"));
    }

    #[test]
    fn no_logical_rules_means_the_buffer_is_the_profile_rules() {
        let buffer = compose_edit_buffer(vec![simple("a.com")], &[]);
        assert_eq!(buffer, vec![simple("a.com")]);
    }

    /// The save writes the edited rules into the profile YAML the restart
    /// re-reads, keeping every other section untouched.
    #[test]
    fn persisting_rules_writes_the_edited_profile_for_the_restart() {
        let dir = temp_dir("persist");
        let profile = dir.join("profile.yaml");
        let home = dir.join("home");
        std::fs::create_dir_all(&home).expect("mkdir home");
        std::fs::write(&profile, PROFILE).expect("seed");

        persist_rules(
            &profile,
            &home,
            PROFILE,
            vec![
                simple("b.com"),
                IRouteRule::Raw {
                    clash_raw: "MATCH,DIRECT".into(),
                },
            ],
        )
        .expect("persist");

        // This is the file `apply_singbox_active_reload` reads: the pre-edit
        // rules must be gone from it.
        let after = std::fs::read_to_string(&profile).expect("reread");
        assert!(after.contains("DOMAIN,b.com,DIRECT"), "{after}");
        assert!(after.contains("MATCH,DIRECT"), "{after}");
        assert!(!after.contains("DOMAIN,a.com,DIRECT"), "{after}");
        assert!(after.contains("proxies:"), "{after}");
        // Atomic write: no temporary left behind.
        assert!(!profile.with_extension("yaml.tmp").exists());
    }

    /// #59: saving with no logical rule left must clear the sidecar instead
    /// of keeping the deleted rules alive in it.
    #[test]
    fn deleting_all_logical_rules_clears_the_sidecar() {
        let dir = temp_dir("clear");
        let profile = dir.join("profile.yaml");
        let home = dir.join("home");
        std::fs::create_dir_all(&home).expect("mkdir home");
        std::fs::write(&profile, PROFILE).expect("seed");
        crate::singbox::save_logical_rules(&home, &[logical("c.com")]).expect("seed sidecar");
        assert!(home.join(crate::singbox::LOGICAL_RULES_FILE).is_file());

        persist_rules(&profile, &home, PROFILE, vec![simple("a.com")]).expect("persist");

        assert!(!home.join(crate::singbox::LOGICAL_RULES_FILE).exists());
        assert!(crate::singbox::load_logical_rules(&home).unwrap().is_empty());
    }

    /// A non-empty logical list still round-trips through the sidecar, so the
    /// always-write change cannot lose saved rules.
    #[test]
    fn persisting_keeps_and_replaces_the_logical_sidecar() {
        let dir = temp_dir("sidecar");
        let profile = dir.join("profile.yaml");
        let home = dir.join("home");
        std::fs::create_dir_all(&home).expect("mkdir home");
        std::fs::write(&profile, PROFILE).expect("seed");

        persist_rules(&profile, &home, PROFILE, vec![simple("a.com"), logical("c.com")]).expect("first");
        assert_eq!(
            crate::singbox::load_logical_rules(&home).unwrap(),
            vec![logical("c.com")]
        );

        persist_rules(&profile, &home, PROFILE, vec![simple("a.com"), logical("d.com")]).expect("second");
        assert_eq!(
            crate::singbox::load_logical_rules(&home).unwrap(),
            vec![logical("d.com")]
        );
    }

    /// Fail closed: a profile that cannot be re-serialized must leave both
    /// files untouched instead of half-writing them.
    #[test]
    fn unparsable_profile_is_rejected_without_sidecar_changes() {
        let dir = temp_dir("reject");
        let profile = dir.join("profile.yaml");
        let home = dir.join("home");
        std::fs::create_dir_all(&home).expect("mkdir home");
        std::fs::write(&profile, PROFILE).expect("seed");

        let error = persist_rules(&profile, &home, "rules: [oops\n", vec![logical("c.com")]).expect_err("invalid yaml");
        assert!(!error.is_empty());
        assert_eq!(std::fs::read_to_string(&profile).unwrap(), PROFILE);
        assert!(!home.join(crate::singbox::LOGICAL_RULES_FILE).exists());
    }
}
