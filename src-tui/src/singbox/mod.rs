//! sing-box runtime configuration generation.
//!
//! The generator produces the `singbox.json` runtime file consumed by
//! `sing-box run -c`. Node outbounds arrive pre-converted (see the
//! subscription converter), while inbound/route/clash_api plumbing is
//! generated here from TUI settings.

// Consumed from task 2.3 of add-singbox-dual-core onward (manager writes
// singbox.json; converter feeds outbounds).
#[allow(dead_code)]
pub mod config_gen;
pub mod convert;
pub mod dns;

use serde_json::Value;

pub use config_gen::{ClashApiSettings, ConfigInput, GroupKind, GroupSpec, TunSettings, generate_config};
pub use dns::DnsConfigSpec;

/// Task 7.4: durable rule-set storage (TUI-owned, separate from the
/// generated singbox.json which is regenerated on every apply).
pub const RULE_SETS_FILE: &str = "singbox-rule-sets.json";

/// Task 7.5: durable storage for logical route rules. Logical rules have no
/// clash YAML form (`routing::save_profile_rules` rejects them), so they
/// live here as sing-box JSON objects and are appended after profile-derived
/// rules during generation.
pub const LOGICAL_RULES_FILE: &str = "singbox-rules.json";

/// Task 8.1: structured DNS settings (1.12+ new format), TUI-owned.
pub const DNS_CONFIG_FILE: &str = "singbox-dns.json";

fn read_json_file<T: serde::de::DeserializeOwned>(home: &std::path::Path, name: &str) -> Option<T> {
    let body = std::fs::read_to_string(home.join(name)).ok()?;
    serde_json::from_str(&body).ok()
}

pub fn load_rule_sets(home: &std::path::Path) -> Vec<Value> {
    read_json_file(home, RULE_SETS_FILE).unwrap_or_default()
}

pub fn save_rule_sets(home: &std::path::Path, sets: &[Value]) -> std::io::Result<()> {
    let body = serde_json::to_string_pretty(sets)?;
    std::fs::write(home.join(RULE_SETS_FILE), body)
}

/// Load stored logical rules. Entries that fail to parse back into the
/// unified model are dropped defensively instead of poisoning generation.
pub fn load_logical_rules(home: &std::path::Path) -> Vec<crate::routing::IRouteRule> {
    let values: Vec<Value> = read_json_file(home, LOGICAL_RULES_FILE).unwrap_or_default();
    values
        .iter()
        .filter_map(crate::routing::from_singbox_json)
        .filter(|rule| matches!(rule, crate::routing::IRouteRule::Logical { .. }))
        .collect()
}

/// Persist logical rules as sing-box JSON objects (lossless round-trip;
/// see the 6.3 property tests in `routing`).
pub fn save_logical_rules(home: &std::path::Path, rules: &[crate::routing::IRouteRule]) -> Result<(), String> {
    if rules
        .iter()
        .any(|r| !matches!(r, crate::routing::IRouteRule::Logical { .. }))
    {
        return Err("logical rule storage accepts only IRouteRule::Logical entries".into());
    }
    let values: Vec<Value> = rules
        .iter()
        .map(crate::routing::to_singbox_json)
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| "logical rule storage requires sing-box-expressible rules".to_string())?;
    let body = serde_json::to_string_pretty(&values).map_err(|e| e.to_string())?;
    std::fs::write(home.join(LOGICAL_RULES_FILE), body).map_err(|e| e.to_string())
}

pub fn load_dns_spec(home: &std::path::Path) -> Option<DnsConfigSpec> {
    read_json_file(home, DNS_CONFIG_FILE)
}

pub fn save_dns_spec(home: &std::path::Path, spec: &DnsConfigSpec) -> Result<(), String> {
    let body = serde_json::to_string_pretty(spec).map_err(|e| e.to_string())?;
    std::fs::write(home.join(DNS_CONFIG_FILE), body).map_err(|e| e.to_string())
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod storage_tests {
    use super::*;
    use crate::routing::{IRouteRule, LogicOp, MatchField, RuleTarget};
    use crate::singbox::dns::{DnsServerKind, DnsServerSpec};

    fn temp_home(name: &str) -> std::path::PathBuf {
        // Unique per test: parallel tests share the process, and one test's
        // cleanup must not delete another's directory mid-write.
        let dir = std::env::temp_dir().join(format!("singbox-mod-test-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        dir
    }

    #[test]
    fn logical_rules_round_trip_through_sidecar() {
        let home = temp_home("logical");
        let rules = vec![IRouteRule::Logical {
            op: LogicOp::Or,
            rules: vec![IRouteRule::Simple {
                matches: vec![MatchField::Domain("a.com".into())],
                target: RuleTarget::Direct,
            }],
            target: RuleTarget::Block,
        }];

        save_logical_rules(&home, &rules).expect("save");
        assert_eq!(load_logical_rules(&home), rules);

        // Simple/Raw rules are not sidecar material — saving them is a
        // programming error the storage refuses rather than silently dropping.
        let mixed = vec![
            IRouteRule::Logical {
                op: LogicOp::And,
                rules: vec![],
                target: RuleTarget::Direct,
            },
            IRouteRule::Simple {
                matches: vec![MatchField::Port(443)],
                target: RuleTarget::Direct,
            },
        ];
        assert!(save_logical_rules(&home, &mixed).is_err());

        // Corrupt entries are dropped on load, not propagated.
        std::fs::write(home.join(LOGICAL_RULES_FILE), "[{\"bogus\":1}]").expect("write");
        assert!(load_logical_rules(&home).is_empty());

        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn dns_spec_round_trips_through_storage() {
        let home = temp_home("dns");
        assert_eq!(load_dns_spec(&home), None);

        let spec = DnsConfigSpec {
            servers: vec![DnsServerSpec {
                tag: "dns-local".into(),
                kind: DnsServerKind::Local,
                server: None,
                server_port: None,
                detour: None,
                inet4_range: None,
                inet6_range: None,
            }],
            rules: Vec::new(),
            domain_resolver: None,
        };
        save_dns_spec(&home, &spec).expect("save");
        assert_eq!(load_dns_spec(&home), Some(spec));

        let _ = std::fs::remove_dir_all(&home);
    }
}
