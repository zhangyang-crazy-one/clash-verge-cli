//! Clash routing mode (`rule` / `global` / `direct`).

use crate::mihomo_api::MihomoApi;
use crate::runtime_config::RUNTIME_CONFIG_IO;

/// The mode after `current` in the TUI cycle: rule → global → direct → rule.
pub fn next_clash_mode(current: &str) -> &'static str {
    match current.to_ascii_lowercase().as_str() {
        "global" => "direct",
        "direct" => "rule",
        _ => "global",
    }
}

/// The running core's mode, or the configured one when it is not running.
pub async fn current_mode(api: &MihomoApi, core_running: bool) -> String {
    if core_running && let Ok(mode) = api.get_mode().await {
        return mode;
    }
    clash_verge_core::config::IClashTemp::new()
        .await
        .get_mode()
        .unwrap_or_else(|| "rule".into())
}

/// Persist `mode` to `clash.yaml` and, when the core runs, apply it live.
/// A failed live update rolls the file back so the next start does not adopt
/// a mode that was reported as failed.
pub async fn apply_clash_mode(api: &MihomoApi, mode: &str, core_running: bool) -> Result<String, String> {
    // Serialize with runtime commits so a stale IClashTemp snapshot cannot
    // overwrite a concurrent profile/TUN write to clash.yaml.
    let previous_mode = {
        let _guard = RUNTIME_CONFIG_IO.lock().await;
        let mut clash = clash_verge_core::config::IClashTemp::new().await;
        let previous = clash.get_mode().unwrap_or_else(|| "rule".into());
        let mut patch = serde_yaml_ng::Mapping::new();
        patch.insert("mode".into(), mode.into());
        clash.patch_config(&patch);
        clash.save_config().await.map_err(|error| error.to_string())?;
        previous
    };
    if core_running && let Err(error) = api.patch_mode(mode).await {
        let _guard = RUNTIME_CONFIG_IO.lock().await;
        let mut clash = clash_verge_core::config::IClashTemp::new().await;
        let mut patch = serde_yaml_ng::Mapping::new();
        patch.insert("mode".into(), previous_mode.into());
        clash.patch_config(&patch);
        let _ = clash.save_config().await;
        return Err(error.to_string());
    }
    Ok(mode.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn next_mode_cycles_and_treats_unknown_as_rule() {
        assert_eq!(next_clash_mode("rule"), "global");
        assert_eq!(next_clash_mode("GLOBAL"), "direct");
        assert_eq!(next_clash_mode("direct"), "rule");
        assert_eq!(next_clash_mode("script"), "global");
    }
}
