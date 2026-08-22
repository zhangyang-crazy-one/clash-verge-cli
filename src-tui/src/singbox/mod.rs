//! sing-box runtime configuration generation.
//!
//! The generator produces the `singbox.json` runtime file consumed by
//! `sing-box run -c`. Node outbounds arrive pre-converted (see the
//! subscription converter), while inbound/route/clash_api plumbing is
//! generated here from TUI settings.

// Consumed from task 2.3 of add-singbox-dual-core onward (manager writes
// singbox.json; converter feeds outbounds). Until then this is scaffold
// exercised by unit tests only.
#[allow(dead_code)]
pub mod config_gen;
pub mod convert;

use serde_json::Value;

pub use config_gen::{ClashApiSettings, ConfigInput, GroupKind, GroupSpec, TunSettings, generate_config};

/// Task 7.4: durable rule-set storage (TUI-owned, separate from the
/// generated singbox.json which is regenerated on every apply).
pub const RULE_SETS_FILE: &str = "singbox-rule-sets.json";

pub fn load_rule_sets(home: &std::path::Path) -> Vec<Value> {
    std::fs::read_to_string(home.join(RULE_SETS_FILE))
        .ok()
        .and_then(|body| serde_json::from_str(&body).ok())
        .unwrap_or_default()
}

pub fn save_rule_sets(home: &std::path::Path, sets: &[Value]) -> std::io::Result<()> {
    let body = serde_json::to_string_pretty(sets)?;
    std::fs::write(home.join(RULE_SETS_FILE), body)
}
