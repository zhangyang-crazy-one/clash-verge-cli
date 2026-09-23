//! `provider list|update`.

use crate::mihomo_manager::manager::MihomoManager;

pub async fn list(manager: &MihomoManager, json: bool) -> anyhow::Result<()> {
    let api = super::running_api(manager).await?;
    let mut providers: Vec<_> = api.get_rule_providers().await?.providers.into_values().collect();
    providers.sort_by(|a, b| a.name.cmp(&b.name));
    if json {
        println!("{}", serde_json::to_string_pretty(&providers)?);
        return Ok(());
    }
    if providers.is_empty() {
        println!("(no rule providers)");
        return Ok(());
    }
    let rows = providers.iter().map(|provider| {
        vec![
            provider.name.clone(),
            provider.behavior.clone(),
            provider.rule_count.to_string(),
            provider.vehicle_type.clone(),
            provider.updated_at.clone().unwrap_or_else(|| "-".into()),
        ]
    });
    print!(
        "{}",
        super::table(&["PROVIDER", "BEHAVIOR", "RULES", "VEHICLE", "UPDATED"], rows)
    );
    Ok(())
}

pub async fn update(manager: &MihomoManager, name: Option<&str>, all: bool) -> anyhow::Result<()> {
    let api = super::running_api(manager).await?;
    let names: Vec<String> = if all {
        let mut names: Vec<String> = api.get_rule_providers().await?.providers.into_keys().collect();
        names.sort();
        names
    } else {
        vec![
            name.ok_or_else(|| anyhow::anyhow!("provide a provider name or pass --all"))?
                .to_string(),
        ]
    };
    let mut failed = 0;
    for name in &names {
        match api.update_rule_provider(name).await {
            Ok(()) => println!("updated {name}"),
            Err(error) => {
                failed += 1;
                eprintln!("failed to update {name}: {error}");
            }
        }
    }
    if failed > 0 {
        anyhow::bail!("{failed} of {} rule provider update(s) failed", names.len());
    }
    Ok(())
}
