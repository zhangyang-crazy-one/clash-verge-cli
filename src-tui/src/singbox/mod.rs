//! sing-box runtime configuration generation.
//!
//! The generator produces the `singbox.json` runtime file consumed by
//! `sing-box run -c`. Node outbounds arrive pre-converted (see the
//! subscription converter), while inbound/route/clash_api plumbing is
//! generated here from TUI settings.

// Consumed from task 2.3 of add-singbox-dual-core onward (manager writes
// singbox.json; converter feeds outbounds).
pub mod capabilities;
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

fn read_json_file<T: serde::de::DeserializeOwned + Default>(home: &std::path::Path, name: &str) -> Result<T, String> {
    let path = home.join(name);
    let body = match std::fs::read_to_string(&path) {
        Ok(body) => body,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(T::default()),
        Err(error) => return Err(format!("read {}: {error}", path.display())),
    };
    serde_json::from_str(&body).map_err(|error| format!("parse {}: {error}", path.display()))
}

pub fn load_rule_sets(home: &std::path::Path) -> Result<Vec<Value>, String> {
    let sets: Vec<Value> = read_json_file(home, RULE_SETS_FILE)?;
    validate_rule_sets(&sets).map_err(|error| format!("{}: {error}", home.join(RULE_SETS_FILE).display()))?;
    Ok(sets)
}

pub(crate) fn validate_rule_sets(sets: &[Value]) -> Result<(), String> {
    let mut tags = std::collections::HashSet::new();
    for (index, item) in sets.iter().enumerate() {
        let tag = item
            .get("tag")
            .and_then(Value::as_str)
            .filter(|tag| !tag.trim().is_empty());
        if !item.is_object() || tag.is_none() {
            return Err(format!("rule-set {index} must be an object with a non-empty tag"));
        }
        let Some(tag) = tag else { continue };
        if !tags.insert(tag) {
            return Err(format!("duplicate rule-set tag {tag:?}"));
        }
        if !matches!(item.get("type").and_then(Value::as_str), Some("local" | "remote")) {
            return Err(format!("rule-set {tag:?} has unsupported type"));
        }
        match item.get("type").and_then(Value::as_str) {
            Some("remote") if item.get("url").and_then(Value::as_str).is_none_or(str::is_empty) => {
                return Err(format!("remote rule-set {tag:?} requires a URL"));
            }
            Some("local") if item.get("path").and_then(Value::as_str).is_none_or(str::is_empty) => {
                return Err(format!("local rule-set {tag:?} requires a path"));
            }
            _ => {}
        }
    }
    Ok(())
}

pub fn save_rule_sets(home: &std::path::Path, sets: &[Value]) -> std::io::Result<()> {
    validate_rule_sets(sets).map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))?;
    if home.join(LOGICAL_RULES_FILE).is_file() {
        let rules =
            load_logical_rules(home).map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        validate_rule_set_references_against(&rules, sets)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))?;
    }
    let body = serde_json::to_string_pretty(sets)?;
    atomic_write(home, RULE_SETS_FILE, body.as_bytes())
}

/// Load stored logical rules. Parse and reference failures are returned to
/// the caller rather than converted into an empty/default settings value.
pub fn load_logical_rules(home: &std::path::Path) -> Result<Vec<crate::routing::IRouteRule>, String> {
    let values: Vec<Value> = read_json_file(home, LOGICAL_RULES_FILE)?;
    let rules: Vec<_> = values
        .into_iter()
        .enumerate()
        .map(|(index, value)| {
            let rule = crate::routing::from_singbox_json(&value)
                .filter(|rule| matches!(rule, crate::routing::IRouteRule::Logical { .. }))
                .ok_or_else(|| format!("{} entry {index} is not a supported logical rule", LOGICAL_RULES_FILE))?;
            // `from_singbox_json` intentionally extracts only the rule model's
            // supported fields. Require exact semantic round-trip here so an
            // edited/corrupt durable JSON entry with extra fields or values
            // cannot be partially accepted and then truncated by to_singbox_json.
            let round_trip = crate::routing::to_singbox_json(&rule)
                .ok_or_else(|| format!("{} entry {index} is not sing-box expressible", LOGICAL_RULES_FILE))?;
            if round_trip != value {
                return Err(format!(
                    "{} entry {index} contains unsupported or lossy rule fields",
                    LOGICAL_RULES_FILE
                ));
            }
            Ok(rule)
        })
        .collect::<Result<_, _>>()?;
    validate_rule_set_references(home, &rules)
        .map_err(|error| format!("{}: {error}", home.join(LOGICAL_RULES_FILE).display()))?;
    Ok(rules)
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
    validate_rule_set_references(home, rules)?;
    let body = serde_json::to_string_pretty(&values).map_err(|e| e.to_string())?;
    atomic_write(home, LOGICAL_RULES_FILE, body.as_bytes()).map_err(|e| e.to_string())
}

fn validate_rule_set_references(home: &std::path::Path, rules: &[crate::routing::IRouteRule]) -> Result<(), String> {
    validate_rule_set_references_against(rules, &load_rule_sets(home)?)
}

fn validate_rule_set_references_against(rules: &[crate::routing::IRouteRule], sets: &[Value]) -> Result<(), String> {
    use crate::routing::{IRouteRule, MatchField};
    let known: std::collections::HashSet<String> = sets
        .iter()
        .filter_map(|set| set.get("tag").and_then(Value::as_str).map(str::to_owned))
        .collect();
    fn visit(rule: &IRouteRule, known: &std::collections::HashSet<String>) -> Result<(), String> {
        match rule {
            IRouteRule::Simple { matches, .. } => {
                for field in matches {
                    if let MatchField::RuleSet(tag) = field
                        && !known.contains(tag)
                    {
                        return Err(format!("logical rule references unknown rule-set {tag:?}"));
                    }
                }
            }
            IRouteRule::Logical { rules, .. } => {
                for child in rules {
                    visit(child, known)?;
                }
            }
            IRouteRule::Raw { .. } => return Err("logical rule storage does not accept raw rules".into()),
        }
        Ok(())
    }
    for rule in rules {
        visit(rule, &known)?;
    }
    Ok(())
}

pub fn load_dns_spec(home: &std::path::Path) -> Result<DnsConfigSpec, String> {
    let spec: DnsConfigSpec = read_json_file(home, DNS_CONFIG_FILE)?;
    validate_dns_references(&spec).map_err(|error| format!("{}: {error}", home.join(DNS_CONFIG_FILE).display()))?;
    Ok(spec)
}

pub fn save_dns_spec(home: &std::path::Path, spec: &DnsConfigSpec) -> Result<(), String> {
    validate_dns_references(spec)?;
    let body = serde_json::to_string_pretty(spec).map_err(|e| e.to_string())?;
    atomic_write(home, DNS_CONFIG_FILE, body.as_bytes()).map_err(|e| e.to_string())
}

fn validate_dns_references(spec: &DnsConfigSpec) -> Result<(), String> {
    spec.validate()?;
    if let Some(resolver) = spec.domain_resolver.as_deref()
        && !spec.servers.iter().any(|server| server.tag == resolver)
    {
        return Err(format!("domain_resolver references unknown DNS server {resolver:?}"));
    }
    Ok(())
}

fn atomic_write(home: &std::path::Path, name: &str, body: &[u8]) -> std::io::Result<()> {
    atomic_write_using(home, name, body, |from, to| std::fs::rename(from, to))
}

fn atomic_write_using(
    home: &std::path::Path,
    name: &str,
    body: &[u8],
    rename: impl FnOnce(&std::path::Path, &std::path::Path) -> std::io::Result<()>,
) -> std::io::Result<()> {
    use std::io::Write as _;
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);
    std::fs::create_dir_all(home)?;
    let target = home.join(name);
    let sequence = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
    let temp = home.join(format!(".{name}.{}.{}.tmp", std::process::id(), sequence));
    let result = (|| {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut file = options.open(&temp)?;
        file.write_all(body)?;
        file.sync_all()?;
        rename(&temp, &target)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
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
        let dir = std::env::temp_dir().join(format!(
            "singbox-mod-test-{}-{name}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
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
        assert_eq!(load_logical_rules(&home).unwrap(), rules);

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

        let with_missing_set = vec![IRouteRule::Logical {
            op: LogicOp::And,
            rules: vec![IRouteRule::Simple {
                matches: vec![MatchField::RuleSet("missing-set".into())],
                target: RuleTarget::Direct,
            }],
            target: RuleTarget::Direct,
        }];
        assert!(
            save_logical_rules(&home, &with_missing_set)
                .unwrap_err()
                .contains("unknown rule-set")
        );
        save_rule_sets(
            &home,
            &[serde_json::json!({"tag":"known-set","type":"local","path":"rules.srs"})],
        )
        .unwrap();
        let with_known_set = vec![IRouteRule::Logical {
            op: LogicOp::And,
            rules: vec![IRouteRule::Simple {
                matches: vec![MatchField::RuleSet("known-set".into())],
                target: RuleTarget::Direct,
            }],
            target: RuleTarget::Direct,
        }];
        save_logical_rules(&home, &with_known_set).expect("known rule-set reference");
        assert_eq!(load_logical_rules(&home).unwrap(), with_known_set);
        assert!(
            save_rule_sets(&home, &[])
                .unwrap_err()
                .to_string()
                .contains("unknown rule-set")
        );
        assert_eq!(load_rule_sets(&home).unwrap()[0]["tag"], "known-set");

        // Corrupt entries are dropped on load, not propagated.
        std::fs::write(home.join(LOGICAL_RULES_FILE), "[{\"bogus\":1}]").expect("write");
        assert!(load_logical_rules(&home).is_err());

        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn stored_logical_rule_rejects_unmodeled_fields_instead_of_truncating() {
        let home = temp_home("lossy-logical");
        std::fs::write(
            home.join(LOGICAL_RULES_FILE),
            r#"[{"type":"logical","mode":"and","rules":[{"domain":["example.com"],"domain_regex":[".*"],"outbound":"direct"}],"outbound":"direct"}]"#,
        )
        .expect("write fixture");

        let error = load_logical_rules(&home).unwrap_err();
        assert!(error.contains("unsupported or lossy rule fields"), "{error}");
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn dns_spec_round_trips_through_storage() {
        let home = temp_home("dns");
        assert_eq!(load_dns_spec(&home).unwrap(), DnsConfigSpec::default());

        let spec = DnsConfigSpec {
            servers: vec![DnsServerSpec {
                tag: "dns-local".into(),
                kind: DnsServerKind::Local,
                server: None,
                server_port: None,
                detour: None,
                path: None,
                inet4_range: None,
                inet6_range: None,
            }],
            rules: Vec::new(),
            domain_resolver: None,
        };
        save_dns_spec(&home, &spec).expect("save");
        assert_eq!(load_dns_spec(&home).unwrap(), spec);
        let mut invalid = spec.clone();
        invalid.domain_resolver = Some("missing-server".into());
        assert!(
            save_dns_spec(&home, &invalid)
                .unwrap_err()
                .contains("unknown DNS bootstrap resolver")
        );
        assert_eq!(
            load_dns_spec(&home).unwrap(),
            spec,
            "invalid reference does not replace prior settings"
        );

        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn malformed_storage_is_reported_and_atomic_validation_preserves_previous_file() {
        let home = temp_home("malformed");
        std::fs::write(home.join(DNS_CONFIG_FILE), "{").expect("corrupt fixture");
        assert!(load_dns_spec(&home).unwrap_err().contains("parse"));
        std::fs::write(
            home.join(DNS_CONFIG_FILE),
            r#"{"servers":[{"tag":"upstream","kind":"udp"}]}"#,
        )
        .expect("semantically invalid fixture");
        let error = load_dns_spec(&home).unwrap_err();
        assert!(
            error.contains(DNS_CONFIG_FILE) && error.contains("requires an address"),
            "{error}"
        );
        let old = "[{}]";
        std::fs::write(home.join(RULE_SETS_FILE), old).expect("old data");
        assert!(save_rule_sets(&home, &[serde_json::json!({ "tag": "" })]).is_err());
        assert_eq!(std::fs::read_to_string(home.join(RULE_SETS_FILE)).unwrap(), old);
        assert!(
            !std::fs::read_dir(&home).unwrap().any(|entry| entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".tmp"))
        );
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn invalid_rule_set_reference_and_remote_schema_do_not_replace_valid_storage() {
        let home = temp_home("rule-set-validation");
        let valid = [serde_json::json!({"tag":"old","type":"local","path":"old.srs"})];
        save_rule_sets(&home, &valid).expect("initial save");
        assert!(save_rule_sets(&home, &[serde_json::json!({"tag":"remote","type":"remote"})]).is_err());
        assert_eq!(load_rule_sets(&home).unwrap(), valid);
        let dangling = vec![IRouteRule::Logical {
            op: LogicOp::And,
            rules: vec![IRouteRule::Simple {
                matches: vec![MatchField::RuleSet("missing".into())],
                target: RuleTarget::Direct,
            }],
            target: RuleTarget::Direct,
        }];
        assert!(
            save_logical_rules(&home, &dangling)
                .unwrap_err()
                .contains("unknown rule-set")
        );
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn atomic_rename_failure_keeps_previous_json_and_cleans_temporary_file() {
        let home = temp_home("rename-failure");
        let path = home.join(DNS_CONFIG_FILE);
        std::fs::write(&path, "{\"servers\": []}").expect("seed old config");
        let error = atomic_write_using(&home, DNS_CONFIG_FILE, b"{\"servers\":[{}]}", |_, _| {
            Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "injected rename failure",
            ))
        })
        .unwrap_err();
        assert!(error.to_string().contains("injected rename failure"));
        assert_eq!(std::fs::read_to_string(path).unwrap(), "{\"servers\": []}");
        assert!(
            std::fs::read_dir(&home).unwrap().all(|entry| !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".tmp"))
        );
        let _ = std::fs::remove_dir_all(home);
    }
}
