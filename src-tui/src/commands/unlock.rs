//! `unlock`: which streaming and AI services the current exit node reaches.

use crate::mihomo_manager::manager::MihomoManager;
use crate::services::unlock::{self, Service, Verdict};

pub async fn run(manager: &MihomoManager, services: &[Service], json: bool) -> anyhow::Result<()> {
    let api = super::running_api(manager).await?;
    let report = unlock::run(&api, services).await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }
    if !report.exit.is_empty() {
        println!("Exit: {}\n", report.exit.join(" → "));
    }
    let rows = report.results.iter().map(|result| {
        vec![
            result.service.name().to_string(),
            verdict_label(result.verdict).to_string(),
            result.region.clone().unwrap_or_else(|| "-".into()),
            result.detail.clone().unwrap_or_default(),
        ]
    });
    print!("{}", super::table(&["SERVICE", "STATUS", "REGION", "DETAIL"], rows));
    Ok(())
}

const fn verdict_label(verdict: Verdict) -> &'static str {
    match verdict {
        Verdict::Available => "available",
        Verdict::OriginalsOnly => "originals only",
        Verdict::Unavailable => "unavailable",
        Verdict::Failed => "failed",
    }
}
