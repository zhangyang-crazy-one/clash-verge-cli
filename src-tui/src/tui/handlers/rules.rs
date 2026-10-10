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
///
/// The rule list is the COMPOSED one (Rules/Merge/Script applied), not the
/// raw profile file: that is the list the core evaluates, the list the
/// generator interleaves the saved `Profile(i)` indices into, and therefore
/// the list the fingerprint describes. Editing the raw file's list while the
/// indices address the composed one is what made a fresh save look like a
/// subscription drift.
async fn load_edit_buffer_from_active_profile(app: &mut App, singbox: bool) -> Result<(), String> {
    let store = crate::profile_store::store::ProfileStore::snapshot()
        .await
        .map_err(|e| e.to_string())?;
    let uid = store.current_uid();
    let item = store.items().into_iter().find(|i| i.uid == uid);
    let Some(item) = item else {
        return Err("no active profile".into());
    };
    if item.file.is_none() {
        return Err("no active remote profile to edit".into());
    }
    let rules = crate::runtime_config::composed_profile_rules(&item).await?;
    let (order, drift) = match load_rule_order(singbox)? {
        Some(order) => order.resolve_profile_drift(crate::singbox::profile_rule_fingerprint(&rules)),
        None => (crate::singbox::RuleOrder::default(), None),
    };
    app.rules_edit_buffer = compose_edit_buffer(rules, &order);
    if let Ok(home) = clash_verge_core::utils::dirs::app_home_dir() {
        app.rule_sets_edit = crate::singbox::load_rule_sets(&home)?;
    }
    app.rules_selected_index = 0;
    app.rules_edit_mode = true;
    app.rules_edit_dirty = false;
    if let Some(note) = drift {
        // Generation applies the same demotion (see
        // `SingboxParts::assemble`), so the editor must show it too instead of
        // an order the core will not honour.
        app.status_msg = Some(note);
    }
    Ok(())
}

/// Stored logical rules and their interleaved order, or none when the
/// active core cannot express them. A broken sidecar is reported rather
/// than silently dropped: the user must fix it before saving, or the next
/// save would overwrite it.
fn load_rule_order(singbox: bool) -> Result<Option<crate::singbox::RuleOrder>, String> {
    if !singbox {
        return Ok(None);
    }
    let home = clash_verge_core::utils::dirs::app_home_dir().map_err(|e| e.to_string())?;
    let order = crate::singbox::load_rule_order(&home)?;
    Ok((!order.logical.is_empty() || !order.entries.is_empty()).then_some(order))
}

/// Rebuild the interleaved edit buffer: profile rules and logical rules in
/// the order the core actually evaluates them.
///
/// A sidecar written before the order was persisted carries no cross-type
/// order (`entries` empty) and keeps the historical "profile rules, then
/// logical rules" layout, which is also what the generator does with it.
/// Profile rules the stored order does not mention (the profile was edited
/// outside this editor) keep their relative order directly after the last
/// referenced profile rule — the same fallback `singbox::interleave_route_rules`
/// applies, so the editor and the generator cannot disagree.
fn compose_edit_buffer(
    profile_rules: Vec<crate::routing::IRouteRule>,
    order: &crate::singbox::RuleOrder,
) -> Vec<crate::routing::IRouteRule> {
    if order.entries.is_empty() {
        return profile_rules.into_iter().chain(order.logical.iter().cloned()).collect();
    }
    let mut buffer: Vec<crate::routing::IRouteRule> = Vec::new();
    let mut referenced = vec![false; profile_rules.len()];
    let mut last_profile_slot = None;
    for entry in &order.entries {
        match entry {
            crate::singbox::RuleOrderEntry::Profile(index) => match profile_rules.get(*index) {
                Some(rule) => {
                    referenced[*index] = true;
                    buffer.push(rule.clone());
                    last_profile_slot = Some(buffer.len() - 1);
                }
                None => continue,
            },
            crate::singbox::RuleOrderEntry::Logical(index) => match order.logical.get(*index) {
                Some(rule) => buffer.push(rule.clone()),
                None => continue,
            },
        }
    }
    let unplaced: Vec<crate::routing::IRouteRule> = profile_rules
        .iter()
        .enumerate()
        .filter(|(index, _)| !referenced[*index])
        .map(|(_, rule)| rule.clone())
        .collect();
    if !unplaced.is_empty() {
        let at = last_profile_slot.map_or(0, |slot| slot + 1);
        buffer.splice(at..at, unplaced);
    }
    buffer
}

/// Split the interleaved buffer back into the two files, remembering where
/// each logical rule sat relative to the profile rules.
///
/// `Profile(i)` indexes the COMPOSED clash rule list — the one the generator
/// interleaves and the one the fingerprint describes — and `Logical(j)` the
/// logical rule list written to the sidecar, so generation can rebuild the
/// very order the editor showed. A logical rule dragged above a `MATCH`
/// therefore stays above it instead of being re-emitted after the whole
/// profile rule list, where the catch-all would shadow it.
fn partition_edit_buffer(
    buffer: Vec<crate::routing::IRouteRule>,
) -> (Vec<crate::routing::IRouteRule>, crate::singbox::RuleOrder) {
    use crate::singbox::{RuleOrder, RuleOrderEntry};
    let mut clash = Vec::new();
    let mut order = RuleOrder::default();
    for rule in buffer {
        if matches!(rule, crate::routing::IRouteRule::Logical { .. }) {
            order.entries.push(RuleOrderEntry::Logical(order.logical.len()));
            order.logical.push(rule);
        } else {
            order.entries.push(RuleOrderEntry::Profile(clash.len()));
            clash.push(rule);
        }
    }
    // The saved order records WHICH composed rule list these indices belong
    // to (a subscription refresh that replaces the list keeps every index in
    // range while repointing it); `persist_rules` fills that fingerprint in
    // from the list the write will produce, which is the list the indices
    // address.
    (clash, order)
}

/// Rules that compare equal in their clash string form.
///
/// The buffer holds rules the user built (typed model), while the composed
/// list is re-parsed from YAML; equality has to be judged on the string the
/// core sees, not on the Rust representation.
fn rules_equivalent(left: &[crate::routing::IRouteRule], right: &[crate::routing::IRouteRule]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .all(|(left, right)| crate::routing::describe(left) == crate::routing::describe(right))
}

/// Remove the configured Rules/Merge/Script chain's own contribution from
/// the composed list the user edited, leaving the list that belongs in the
/// profile FILE.
///
/// The chain is re-applied on top of whatever the file holds, so writing the
/// composed list back verbatim would duplicate every fragment rule (a prepend
/// fragment's rule would appear twice on the next composition). The
/// contribution is exactly the list the chain produces from an empty profile,
/// which [`crate::runtime_config::compose_profile_rules`] computes.
fn strip_chain_rules(
    composed: &[crate::routing::IRouteRule],
    chain_rules: &[crate::routing::IRouteRule],
) -> Result<Vec<crate::routing::IRouteRule>, String> {
    if chain_rules.is_empty() {
        return Ok(composed.to_vec());
    }
    if composed.len() >= chain_rules.len() {
        // A prepend fragment (or a chain that replaces the list) puts its
        // own rules at the front.
        if rules_equivalent(&composed[..chain_rules.len()], chain_rules) {
            return Ok(composed[chain_rules.len()..].to_vec());
        }
        // An appending script/fragment contributes at the end.
        if rules_equivalent(&composed[composed.len() - chain_rules.len()..], chain_rules) {
            return Ok(composed[..composed.len() - chain_rules.len()].to_vec());
        }
    }
    Err(format!(
        "the configured Rules/Merge/Script chain contributes {} rules that are not a contiguous part of \
the edited list; saving the rules editor is not supported for this profile",
        chain_rules.len()
    ))
}

/// Persist the split rule buffer: clash rules back into the profile YAML,
/// logical rules into the sing-box sidecar, plus the interleaved order that
/// lets the generator put the two back together as the editor showed them.
///
/// The sidecar is written — and removed when no logical rule is left (#59) —
/// only under the sing-box core, which is the only core that consumes it.
/// Under mihomo the buffer holds no logical rule (the save path rejects them),
/// so writing it would take the partition's empty list and delete every rule
/// the user had saved for their next sing-box run.
///
/// Under sing-box the sidecar is always written: writing only on a non-empty
/// list left deleted rules in place, and never writing it lost every rule that
/// was already saved.
///
/// `chain_rules` is the configured Rules/Merge/Script chain's own contribution
/// (see [`crate::runtime_config::compose_profile_rules`]): it is stripped off
/// the composed list before the profile is written, and put back in front of
/// it for the fingerprint — the identity that must describe the list the
/// `Profile(i)` indices address.
fn persist_rules(
    path: &std::path::Path,
    home: &std::path::Path,
    yaml: &str,
    buffer: Vec<crate::routing::IRouteRule>,
    kind: crate::mihomo_manager::CoreKind,
    chain_rules: &[crate::routing::IRouteRule],
) -> Result<(), String> {
    let (composed_rules, mut order) = partition_edit_buffer(buffer);
    let file_rules = strip_chain_rules(&composed_rules, chain_rules)?;
    let saved = crate::routing::save_profile_rules(yaml, &file_rules)?;
    write_atomic(path, &saved)?;
    if kind != crate::mihomo_manager::CoreKind::SingBox {
        return Ok(());
    }
    // Fingerprint the composed list AS IT WILL READ BACK, not the in-memory
    // partition: generation matches the sidecar against the composed profile,
    // and only the round-tripped form is guaranteed to describe the same
    // rules. A profile that cannot be re-parsed keeps no fingerprint, which
    // the generator treats as "unverifiable" rather than "changed".
    if let Ok(rules) = crate::routing::load_profile_rules(&saved) {
        let mut canonical: Vec<crate::routing::IRouteRule> = chain_rules.to_vec();
        canonical.extend(rules);
        order.profile = Some(crate::singbox::profile_rule_fingerprint(&canonical));
    }
    persist_logical_rules(home, &order)
}

/// [`persist_rules`] for a real profile: computes the chain contribution and
/// proves, BEFORE the profile file is touched, that writing the derived list
/// reproduces the composed list the editor is showing.
async fn persist_rules_for_profile(
    item: &clash_verge_core::config::PrfItem,
    path: &std::path::Path,
    home: &std::path::Path,
    yaml: &str,
    buffer: Vec<crate::routing::IRouteRule>,
    kind: crate::mihomo_manager::CoreKind,
) -> Result<(), String> {
    let chain_rules = crate::runtime_config::compose_profile_rules(item, &[]).await?;
    let (composed, _) = partition_edit_buffer(buffer.clone());
    let file_rules = strip_chain_rules(&composed, &chain_rules)?;
    let reproduced = crate::runtime_config::compose_profile_rules(item, &file_rules).await?;
    if !rules_equivalent(&reproduced, &composed) {
        return Err(
            "the configured Rules/Merge/Script chain rewrites the profile rule list, so this profile's \
rules cannot be saved from the editor; nothing was written"
                .into(),
        );
    }
    persist_rules(path, home, yaml, buffer, kind, &chain_rules)
}

/// Write the logical sidecar, or delete it when the list is empty so that a
/// user who removed every logical rule actually loses them.
fn persist_logical_rules(home: &std::path::Path, order: &crate::singbox::RuleOrder) -> Result<(), String> {
    if order.logical.is_empty() {
        return match std::fs::remove_file(home.join(crate::singbox::LOGICAL_RULES_FILE)) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(format!(
                "remove {}: {error}",
                home.join(crate::singbox::LOGICAL_RULES_FILE).display()
            )),
        };
    }
    crate::singbox::save_rule_order(home, order)
}

/// Write through a uniquely-named temporary file in the same directory and
/// rename it over the target, so an interrupted save cannot leave a truncated
/// profile and concurrent writers cannot share a staging name.
///
/// Follows the same convention as `singbox::storage::atomic_write`: the
/// staging file is created `create_new` with owner-only permissions, its
/// content is fsynced before the rename, and an existing target's mode is
/// mirrored (otherwise the profile would silently become 0600 or 0644).
fn write_atomic(path: &std::path::Path, body: &str) -> Result<(), String> {
    use std::io::Write as _;
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

    let parent = path
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", path.display()))?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("profile.yaml");
    let sequence = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
    let temporary = parent.join(format!(".{name}.{}.{sequence}.tmp", std::process::id()));

    let result = (|| -> std::io::Result<()> {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
            let mode = std::fs::metadata(path)
                .map(|meta| meta.permissions().mode())
                .unwrap_or(0o600);
            options.mode(mode);
        }
        let mut file = options.open(&temporary)?;
        file.write_all(body.as_bytes())?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temporary, path)
    })();
    if let Err(error) = result {
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
            persist_rules_for_profile(&item, &path, &home, &yaml, buffer, manager.core_kind()).await?;
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
    use crate::mihomo_manager::CoreKind;
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
    /// after the profile rules — the order the sing-box generator uses for a
    /// sidecar that carries no cross-type order.
    #[test]
    fn saved_logical_rules_are_visible_in_the_edit_buffer() {
        let order = crate::singbox::RuleOrder {
            logical: vec![logical("c.com")],
            entries: Vec::new(),
            profile: None,
        };
        let buffer = compose_edit_buffer(vec![simple("a.com"), simple("b.com")], &order);
        assert_eq!(buffer.len(), 3);
        assert!(matches!(buffer[2], IRouteRule::Logical { .. }));
        assert!(crate::routing::describe(&buffer[2]).contains("OR"));
    }

    #[test]
    fn no_logical_rules_means_the_buffer_is_the_profile_rules() {
        let buffer = compose_edit_buffer(vec![simple("a.com")], &crate::singbox::RuleOrder::default());
        assert_eq!(buffer, vec![simple("a.com")]);
    }

    /// The order the editor persisted is the order the buffer is rebuilt in:
    /// a logical rule the user moved above the profile `MATCH` comes back
    /// above it, not appended at the end.
    #[test]
    fn the_saved_interleaved_order_drives_the_buffer() {
        let order = crate::singbox::RuleOrder {
            logical: vec![logical("blocked.example")],
            entries: vec![
                crate::singbox::RuleOrderEntry::Logical(0),
                crate::singbox::RuleOrderEntry::Profile(0),
                crate::singbox::RuleOrderEntry::Profile(1),
            ],
            profile: None,
        };
        let buffer = compose_edit_buffer(
            vec![
                simple("a.com"),
                IRouteRule::Raw {
                    clash_raw: "MATCH,DIRECT".into(),
                },
            ],
            &order,
        );
        assert!(matches!(buffer[0], IRouteRule::Logical { .. }));
        assert_eq!(buffer[1], simple("a.com"));
        assert_eq!(
            buffer[2],
            IRouteRule::Raw {
                clash_raw: "MATCH,DIRECT".into()
            }
        );
    }

    /// Splitting the buffer keeps both partitions and the cross-type order:
    /// the `MATCH` is profile rule 0, so the logical rule must be recorded
    /// before it, not after the whole profile list.
    #[test]
    fn partitioning_the_buffer_records_where_each_rule_sat() {
        let (clash, order) = partition_edit_buffer(vec![
            logical("blocked.example"),
            simple("a.com"),
            IRouteRule::Raw {
                clash_raw: "MATCH,DIRECT".into(),
            },
        ]);
        assert_eq!(clash.len(), 2);
        assert_eq!(order.logical, vec![logical("blocked.example")]);
        assert_eq!(
            order.entries,
            vec![
                crate::singbox::RuleOrderEntry::Logical(0),
                crate::singbox::RuleOrderEntry::Profile(0),
                crate::singbox::RuleOrderEntry::Profile(1),
            ]
        );
    }

    /// Profile rules the stored order does not mention (profile edited outside
    /// this editor) keep their relative order directly after the last
    /// referenced profile rule, mirroring what the generator does.
    #[test]
    fn unmentioned_profile_rules_land_after_the_last_referenced_one() {
        let order = crate::singbox::RuleOrder {
            logical: vec![logical("c.com")],
            entries: vec![
                crate::singbox::RuleOrderEntry::Profile(0),
                crate::singbox::RuleOrderEntry::Logical(0),
            ],
            profile: None,
        };
        let buffer = compose_edit_buffer(vec![simple("a.com"), simple("b.com")], &order);
        assert_eq!(buffer, vec![simple("a.com"), simple("b.com"), logical("c.com")]);
    }

    /// Review regression (end to end on the editor side): a logical rule moved
    /// BEFORE the `MATCH` is saved as a profile `MATCH` plus an ordered
    /// sidecar entry, and reloading the buffer keeps it above the catch-all.
    #[test]
    fn a_logical_rule_moved_before_match_survives_a_save_and_reload() {
        let dir = temp_dir("order-round-trip");
        let profile = dir.join("profile.yaml");
        let home = dir.join("home");
        std::fs::create_dir_all(&home).expect("mkdir home");
        std::fs::write(&profile, PROFILE).expect("seed");

        persist_rules(
            &profile,
            &home,
            PROFILE,
            vec![
                logical("blocked.example"),
                IRouteRule::Raw {
                    clash_raw: "MATCH,DIRECT".into(),
                },
            ],
            CoreKind::SingBox,
            &[],
        )
        .expect("save");

        let saved_yaml = std::fs::read_to_string(&profile).expect("reread profile");
        assert!(saved_yaml.contains("MATCH,DIRECT"), "{saved_yaml}");
        assert!(!saved_yaml.contains("blocked.example"), "{saved_yaml}");

        let order = crate::singbox::load_rule_order(&home).expect("load order");
        let profile_rules = crate::routing::load_profile_rules(&saved_yaml).expect("load profile rules");
        let buffer = compose_edit_buffer(profile_rules, &order);
        assert!(matches!(buffer[0], IRouteRule::Logical { .. }), "{buffer:?}");
        assert_eq!(
            buffer[1],
            IRouteRule::Raw {
                clash_raw: "MATCH,DIRECT".into()
            }
        );
    }

    /// P1 (reviewer): a subscription refresh can replace the profile rule
    /// list under a saved order. Every stored index stays in range while
    /// repointing at a different rule, so the save records the identity of
    /// the list it ordered, and a later refresh is detected instead of being
    /// trusted.
    #[test]
    fn the_saved_order_records_which_profile_rule_list_it_belongs_to() {
        let dir = temp_dir("fingerprint");
        let profile = dir.join("profile.yaml");
        let home = dir.join("home");
        std::fs::create_dir_all(&home).expect("mkdir home");
        std::fs::write(&profile, PROFILE).expect("seed");

        persist_rules(
            &profile,
            &home,
            PROFILE,
            vec![
                simple("a.com"),
                logical("blocked.example"),
                IRouteRule::Raw {
                    clash_raw: "MATCH,DIRECT".into(),
                },
            ],
            CoreKind::SingBox,
            &[],
        )
        .expect("save");

        let stored = crate::singbox::load_rule_order(&home).expect("load order");
        let fingerprint = stored.profile.expect("the save records the profile rule list");
        // The fingerprint must describe the rules the indices address — i.e.
        // the ones the restart re-reads from the rewritten profile file, not
        // the ones the buffer happened to hold.
        let saved_yaml = std::fs::read_to_string(&profile).expect("reread profile");
        let reloaded = crate::routing::load_profile_rules(&saved_yaml).unwrap();
        assert_eq!(reloaded.len(), 2, "{saved_yaml}");
        assert_eq!(
            fingerprint,
            crate::singbox::profile_rule_fingerprint(&reloaded),
            "the fingerprint must describe the rules the indices address"
        );

        // The rules themselves round-trip through the profile file.
        let buffer = compose_edit_buffer(reloaded, &stored);
        assert!(matches!(buffer[1], IRouteRule::Logical { .. }), "{buffer:?}");

        // A refreshed subscription changes the identity.
        let refreshed: Vec<IRouteRule> = vec![simple("new1.com"), simple("new2.com"), simple("new3.com")];
        let (effective, note) = stored.resolve_profile_drift(crate::singbox::profile_rule_fingerprint(&refreshed));
        assert!(effective.entries.is_empty(), "the stale order must be dropped");
        assert!(note.expect("reported").contains("rule order reset"));
        // The fallback the generator uses: profile rules, then logical ones.
        let fallback = compose_edit_buffer(refreshed, &effective);
        assert_eq!(fallback.len(), 4);
        assert!(matches!(fallback[3], IRouteRule::Logical { .. }), "{fallback:?}");
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
            CoreKind::SingBox,
            &[],
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

        persist_rules(&profile, &home, PROFILE, vec![simple("a.com")], CoreKind::SingBox, &[]).expect("persist");

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

        persist_rules(
            &profile,
            &home,
            PROFILE,
            vec![simple("a.com"), logical("c.com")],
            CoreKind::SingBox,
            &[],
        )
        .expect("first");
        assert_eq!(
            crate::singbox::load_logical_rules(&home).unwrap(),
            vec![logical("c.com")]
        );

        persist_rules(
            &profile,
            &home,
            PROFILE,
            vec![simple("a.com"), logical("d.com")],
            CoreKind::SingBox,
            &[],
        )
        .expect("second");
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

        let error = persist_rules(
            &profile,
            &home,
            "rules: [oops\n",
            vec![logical("c.com")],
            CoreKind::SingBox,
            &[],
        )
        .expect_err("invalid yaml");
        assert!(!error.is_empty());
        assert_eq!(std::fs::read_to_string(&profile).unwrap(), PROFILE);
        assert!(!home.join(crate::singbox::LOGICAL_RULES_FILE).exists());
    }

    /// Saving rules while running mihomo must not touch the sing-box logical
    /// sidecar: the mihomo buffer holds no logical rule, so writing the empty
    /// partition deleted every logical rule the user had saved — data loss
    /// that only surfaced on the next core switch.
    #[test]
    fn a_mihomo_save_leaves_the_singbox_sidecar_untouched() {
        let dir = temp_dir("mihomo-sidecar");
        let profile = dir.join("profile.yaml");
        let home = dir.join("home");
        std::fs::create_dir_all(&home).expect("mkdir home");
        std::fs::write(&profile, PROFILE).expect("seed");
        crate::singbox::save_logical_rules(&home, &[logical("c.com")]).expect("seed sidecar");

        persist_rules(&profile, &home, PROFILE, vec![simple("a.com")], CoreKind::Mihomo, &[]).expect("persist");

        assert_eq!(
            crate::singbox::load_logical_rules(&home).unwrap(),
            vec![logical("c.com")],
            "a mihomo rule save must not delete the sing-box logical rules"
        );
        // The clash side of the save still happened.
        let after = std::fs::read_to_string(&profile).expect("reread");
        assert!(after.contains("DOMAIN,a.com,DIRECT"), "{after}");
    }

    /// P1 (reviewer), F1: the fingerprint and the `Profile(i)` indices must
    /// describe ONE representation — the COMPOSED rule list the generator
    /// actually interleaves. With a prepend fragment in the chain the editor
    /// used to fingerprint the RAW profile list, so a fresh save was
    /// immediately misdetected as subscription drift, demoted to
    /// append-after, and the logical rule the user had moved before `MATCH`
    /// landed after the catch-all.
    #[tokio::test]
    async fn a_saved_logical_rule_before_match_survives_a_prepend_fragment() {
        use crate::profile_store::store::ProfileStore;
        use clash_verge_core::config::PrfItem;

        let home = tempfile::tempdir().expect("tempdir");
        let _home_guard = crate::profile_store::store::tests::claim_test_app_home(home.path().to_path_buf()).await;
        std::fs::write(
            home.path().join("config.yaml"),
            "mode: rule\nmixed-port: 39191\nsecret: fragment-fixture-secret\nexternal-controller: 127.0.0.1:19991\n",
        )
        .expect("config");
        std::fs::write(
            home.path().join("profiles.yaml"),
            "current: Rbase\nitems:\n  - {uid: Rbase, type: remote, file: base.yaml, option: {rules: Rfrag}}\n  - {uid: Rfrag, type: rules, file: rules.yaml}\n",
        )
        .expect("profiles");
        std::fs::write(
            home.path().join("profiles/base.yaml"),
            "proxies: []\nrules:\n  - MATCH,DIRECT\n",
        )
        .expect("base profile");
        std::fs::write(
            home.path().join("profiles/rules.yaml"),
            "prepend:\n  - DOMAIN,fragment.example,DIRECT\nappend: []\ndelete: []\n",
        )
        .expect("rules fragment");

        let item: PrfItem = ProfileStore::snapshot()
            .await
            .expect("store")
            .items()
            .into_iter()
            .find(|item| item.uid.as_deref() == Some("Rbase"))
            .expect("base profile item");
        let path = clash_verge_core::utils::dirs::app_profiles_dir()
            .expect("profiles dir")
            .join("base.yaml");
        let yaml = std::fs::read_to_string(&path).expect("read base profile");

        // The editor buffer is the COMPOSED list: fragment rule, then MATCH.
        let composed = crate::runtime_config::composed_profile_rules(&item)
            .await
            .expect("compose");
        assert_eq!(composed.len(), 2, "{composed:?}");
        let buffer = compose_edit_buffer(composed, &crate::singbox::RuleOrder::default());
        assert_eq!(buffer.len(), 2, "{buffer:?}");
        // The user drags the logical block above the MATCH.
        let edited = vec![buffer[0].clone(), logical("blocked.example"), buffer[1].clone()];

        persist_rules_for_profile(&item, &path, home.path(), &yaml, edited, CoreKind::SingBox)
            .await
            .expect("save");

        // The fragment's own rule is NOT written into the profile file: the
        // chain prepends it again on every composition.
        let saved_yaml = std::fs::read_to_string(&path).expect("reread profile");
        assert!(saved_yaml.contains("MATCH,DIRECT"), "{saved_yaml}");
        assert!(
            !saved_yaml.contains("fragment.example"),
            "the chain contribution must not be duplicated into the profile file: {saved_yaml}"
        );

        // ...and the identity recorded for the saved indices is the composed
        // list, so generation sees no drift on the very next save/refresh.
        let after = crate::runtime_config::composed_profile_rules(&item)
            .await
            .expect("compose after save");
        let stored = crate::singbox::load_rule_order(home.path()).expect("load order");
        let (effective, note) = stored.resolve_profile_drift(crate::singbox::profile_rule_fingerprint(&after));
        assert!(note.is_none(), "a fresh save must not look like drift: {note:?}");
        assert_eq!(
            effective.entries,
            vec![
                crate::singbox::RuleOrderEntry::Profile(0),
                crate::singbox::RuleOrderEntry::Logical(0),
                crate::singbox::RuleOrderEntry::Profile(1),
            ],
            "the logical rule must stay recorded before the MATCH"
        );
        let rebuilt = compose_edit_buffer(after, &effective);
        assert!(matches!(rebuilt[1], IRouteRule::Logical { .. }), "{rebuilt:?}");
        assert_eq!(
            rebuilt[2],
            IRouteRule::Raw {
                clash_raw: "MATCH,DIRECT".into()
            },
            "the logical block must still precede the catch-all"
        );
    }

    /// A chain that contributes rules the edited list no longer contains at
    /// all (a rewritten fragment) must be refused, not silently duplicated
    /// into the profile file.
    #[test]
    fn a_non_contiguous_chain_contribution_is_refused() {
        let error = strip_chain_rules(&[simple("a.com")], &[simple("frag.example")])
            .expect_err("an unrelated contribution cannot be stripped");
        assert!(error.contains("Rules/Merge/Script"), "{error}");
        assert_eq!(
            strip_chain_rules(&[simple("a.com")], &[]).expect("no chain"),
            vec![simple("a.com")]
        );
        assert_eq!(
            strip_chain_rules(&[simple("frag.example"), simple("a.com")], &[simple("frag.example")])
                .expect("prepended chain"),
            vec![simple("a.com")]
        );
        assert_eq!(
            strip_chain_rules(&[simple("a.com"), simple("frag.example")], &[simple("frag.example")])
                .expect("appended chain"),
            vec![simple("a.com")]
        );
    }

    /// The atomic write follows the `singbox` convention: unique staging
    /// name, owner-only permissions (mirroring an existing target's mode),
    /// and no staging file left behind.
    #[test]
    fn the_profile_write_is_atomic_and_private() {
        let dir = temp_dir("atomic");
        let profile = dir.join("profile.yaml");
        std::fs::write(&profile, PROFILE).expect("seed");

        write_atomic(&profile, "proxies: []\nrules: []\n").expect("write");
        assert_eq!(std::fs::read_to_string(&profile).unwrap(), "proxies: []\nrules: []\n");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let before = std::fs::metadata(&profile).expect("stat").permissions().mode() & 0o777;
            std::fs::set_permissions(&profile, std::fs::Permissions::from_mode(0o600)).expect("tighten");
            write_atomic(&profile, "proxies: []\nrules: []\n").expect("rewrite");
            let after = std::fs::metadata(&profile).expect("stat").permissions().mode() & 0o777;
            assert_eq!(after, 0o600, "an existing target's mode is mirrored");
            assert_ne!(before, 0, "precondition: the seeded file had some mode");
        }

        // No staging file survives the write, under either naming convention.
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .expect("list")
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name != "profile.yaml")
            .collect();
        assert!(leftovers.is_empty(), "staging files left behind: {leftovers:?}");
    }
}
