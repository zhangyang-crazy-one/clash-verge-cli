//! Rules view: rules and rule providers.

use crate::app::{Action, App};

use super::Ctx;

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
    app.rules_selected_index = app.rules_selected_index.min(app.rules.len().saturating_sub(1));
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

pub(super) fn note_providers(app: &mut App, providers: Vec<crate::mihomo_api::types::RuleProvider>) {
    app.rule_providers_loading = false;
    app.rule_providers = providers;
    app.rules_selected_index = app.rules_selected_index.min(app.rule_providers.len().saturating_sub(1));
}

/// `Enter` with the providers panel focused: update that provider.
pub(super) fn update_selected_provider(app: &mut App, ctx: &Ctx) {
    if !app.rules_focus_providers {
        return;
    }
    let Some(provider) = app.rule_providers.get(app.rules_selected_index) else {
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
