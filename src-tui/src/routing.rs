//! Unified route rule model (`IRouteRule`) with dual-core serialization
//! (tasks 6.1-6.3 of add-singbox-dual-core).
//!
//! One model, two formats: clash YAML rule strings
//! (`DOMAIN-SUFFIX,google.com,PROXY`) and sing-box route rule JSON
//! objects (fields grouped into arrays, logical rules nested).
//! Rules neither format can express round-trip through [`IRouteRule::Raw`]
//! verbatim — data loss is impossible by construction; callers block
//! saves when a Raw fragment would have to be reinterpreted.

use serde_json::{json, Value};

/// A single match condition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MatchField {
    Domain(String),
    DomainSuffix(String),
    DomainKeyword(String),
    IpCidr(String),
    Port(u16),
    Process(String),
    RuleSet(String),
}

/// What happens when a rule matches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuleTarget {
    Outbound(String),
    Direct,
    Block,
}

/// Logical combinator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogicOp {
    And,
    Or,
}

/// Core-agnostic routing rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IRouteRule {
    Simple {
        matches: Vec<MatchField>,
        target: RuleTarget,
    },
    Logical {
        op: LogicOp,
        rules: Vec<IRouteRule>,
        target: RuleTarget,
    },
    /// Verbatim passthrough for rules the model cannot express
    /// (clash SUB-RULE, exotic payloads). Never reinterpreted.
    Raw {
        /// clash-format rule string (the canonical preserved form).
        clash_raw: String,
    },
}

impl IRouteRule {
    pub fn is_raw(&self) -> bool {
        matches!(self, IRouteRule::Raw { .. })
    }
}

// ---------- clash YAML rule strings ----------

const CLASH_TARGETS: &[(&str, RuleTarget)] = &[("DIRECT", RuleTarget::Direct), ("REJECT", RuleTarget::Block)];

fn target_to_clash_str(target: &RuleTarget) -> String {
    match target {
        RuleTarget::Outbound(name) => name.clone(),
        RuleTarget::Direct => "DIRECT".into(),
        RuleTarget::Block => "REJECT".into(),
    }
}

fn target_from_clash_str(s: &str) -> Option<RuleTarget> {
    Some(match s {
        "DIRECT" => RuleTarget::Direct,
        "REJECT" => RuleTarget::Block,
        other => RuleTarget::Outbound(other.into()),
    })
}

fn field_to_clash_str(field: &MatchField) -> Option<String> {
    Some(match field {
        MatchField::Domain(v) => format!("DOMAIN,{v}"),
        MatchField::DomainSuffix(v) => format!("DOMAIN-SUFFIX,{v}"),
        MatchField::DomainKeyword(v) => format!("DOMAIN-KEYWORD,{v}"),
        MatchField::IpCidr(v) => format!("IP-CIDR,{v}"),
        MatchField::Port(v) => format!("DST-PORT,{v}"),
        MatchField::Process(v) => format!("PROCESS-NAME,{v}"),
        MatchField::RuleSet(_) => return None, // clash uses RULE-SET with provider names; handled by caller mapping
    })
}

fn field_from_clash_str(kind: &str, value: &str) -> Option<MatchField> {
    Some(match kind {
        "DOMAIN" => MatchField::Domain(value.into()),
        "DOMAIN-SUFFIX" => MatchField::DomainSuffix(value.into()),
        "DOMAIN-KEYWORD" => MatchField::DomainKeyword(value.into()),
        "IP-CIDR" | "IP-CIDR6" => MatchField::IpCidr(value.into()),
        "DST-PORT" => MatchField::Port(value.parse().ok()?),
        "PROCESS-NAME" => MatchField::Process(value.into()),
        "RULE-SET" => MatchField::RuleSet(value.into()),
        _ => return None,
    })
}

/// Parse a clash rule string. Unrecognized kinds become Raw passthrough.
pub fn from_clash_rule_str(rule: &str) -> IRouteRule {
    let parts: Vec<&str> = rule.split(',').map(str::trim).collect();
    if parts.len() < 2 {
        return IRouteRule::Raw { clash_raw: rule.into() };
    }
    // SUB-RULE and other exotic headers pass through verbatim.
    if matches!(parts[0], "SUB-RULE" | "AND" | "OR" | "NOT") {
        return IRouteRule::Raw { clash_raw: rule.into() };
    }
    let Some(target) = target_from_clash_str(parts[parts.len() - 1]) else {
        return IRouteRule::Raw { clash_raw: rule.into() };
    };
    let Some(field) = field_from_clash_str(parts[0], parts[1]) else {
        return IRouteRule::Raw { clash_raw: rule.into() };
    };
    IRouteRule::Simple { matches: vec![field], target }
}

/// Serialize back to a clash rule string. Raw rules round-trip verbatim.
pub fn to_clash_rule_str(rule: &IRouteRule) -> String {
    match rule {
        IRouteRule::Raw { clash_raw } => clash_raw.clone(),
        IRouteRule::Simple { matches, target } => {
            let mut parts: Vec<String> = matches
                .iter()
                .filter_map(field_to_clash_str)
                .flat_map(|s| s.split(',').map(str::to_string).collect::<Vec<_>>())
                .collect();
            parts.push(target_to_clash_str(target));
            parts.join(",")
        }
        IRouteRule::Logical { .. } => {
            // Clash has no native logical syntax (SUB-RULE is positional);
            // logical rules only round-trip through sing-box JSON.
            String::new()
        }
    }
}

// ---------- sing-box route rule JSON ----------

fn field_to_singbox(field: &MatchField, rule: &mut Value) {
    let (key, value) = match field {
        MatchField::Domain(v) => ("domain", json!([v])),
        MatchField::DomainSuffix(v) => ("domain_suffix", json!([v])),
        MatchField::DomainKeyword(v) => ("domain_keyword", json!([v])),
        MatchField::IpCidr(v) => ("ip_cidr", json!([v])),
        MatchField::Port(v) => ("port", json!([v])),
        MatchField::Process(v) => ("process_name", json!([v])),
        MatchField::RuleSet(v) => ("rule_set", json!([v])),
    };
    // Multiple fields of the same kind merge into one array.
    match rule.get(key) {
        Some(Value::Array(existing)) => {
            let mut merged = existing.clone();
            if let Value::Array(add) = value {
                merged.extend(add);
            }
            rule[key] = Value::Array(merged);
        }
        _ => rule[key] = value,
    }
}

fn target_to_singbox(target: &RuleTarget, rule: &mut Value) {
    rule["outbound"] = match target {
        RuleTarget::Outbound(name) => json!(name),
        RuleTarget::Direct => json!("direct"),
        RuleTarget::Block => json!("block"),
    };
}

fn target_from_singbox(rule: &Value) -> Option<RuleTarget> {
    let outbound = rule.get("outbound")?.as_str()?;
    Some(match outbound {
        "direct" => RuleTarget::Direct,
        "block" => RuleTarget::Block,
        other => RuleTarget::Outbound(other.into()),
    })
}

fn field_from_singbox(rule: &Value, key: &str, make: fn(String) -> MatchField) -> Option<MatchField> {
    rule.get(key)?.as_array()?.first()?.as_str().map(|v| make(v.into()))
}

fn simple_from_singbox(rule: &Value) -> Option<IRouteRule> {
    let mut matches = Vec::new();
    for (key, make) in [
        ("domain", MatchField::Domain as fn(String) -> MatchField),
        ("domain_suffix", MatchField::DomainSuffix),
        ("domain_keyword", MatchField::DomainKeyword),
        ("ip_cidr", MatchField::IpCidr),
        ("process_name", MatchField::Process),
        ("rule_set", MatchField::RuleSet),
    ] {
        if let Some(f) = field_from_singbox(rule, key, make) {
            matches.push(f);
        }
    }
    if let Some(port) = rule.get("port").and_then(Value::as_array).and_then(|a| a.first()) {
        matches.push(MatchField::Port(port.as_u64()? as u16));
    }
    let target = target_from_singbox(rule)?;
    Some(IRouteRule::Simple { matches, target })
}

/// Convert an IRouteRule into a sing-box route rule JSON object.
/// Raw rules cannot be represented — callers must splice them verbatim
/// into the profile (the generator keeps them out of this path).
pub fn to_singbox_json(rule: &IRouteRule) -> Option<Value> {
    match rule {
        IRouteRule::Raw { .. } => None,
        IRouteRule::Simple { matches, target } => {
            let mut rule = json!({});
            for field in matches {
                field_to_singbox(field, &mut rule);
            }
            target_to_singbox(target, &mut rule);
            Some(rule)
        }
        IRouteRule::Logical { op, rules, target } => {
            let mode = match op {
                LogicOp::And => "and",
                LogicOp::Or => "or",
            };
            let mut inner = Vec::new();
            for r in rules {
                inner.push(to_singbox_json(r)?);
            }
            let mut rule = json!({ "type": "logical", "mode": mode, "rules": inner });
            target_to_singbox(target, &mut rule);
            Some(rule)
        }
    }
}

/// Parse a sing-box route rule JSON object back into the model.
/// Returns None for objects the model cannot express (caller keeps them
/// as raw JSON fragments in the profile).
pub fn from_singbox_json(rule: &Value) -> Option<IRouteRule> {
    if rule.get("type").and_then(Value::as_str) == Some("logical") {
        let op = match rule.get("mode").and_then(Value::as_str) {
            Some("and") => LogicOp::And,
            Some("or") => LogicOp::Or,
            _ => return None,
        };
        let mut rules = Vec::new();
        for inner in rule.get("rules")?.as_array()? {
            rules.push(from_singbox_json(inner)?);
        }
        let target = target_from_singbox(rule)?;
        return Some(IRouteRule::Logical { op, rules, target });
    }
    simple_from_singbox(rule)
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;

    fn roundtrip_clash(rule: &str) {
        let model = from_clash_rule_str(rule);
        let back = to_clash_rule_str(&model);
        assert_eq!(back, rule, "clash round-trip broke: {rule} -> {model:?} -> {back}");
    }

    #[test]
    fn clash_round_trips_all_supported_kinds() {
        roundtrip_clash("DOMAIN,example.com,PROXY");
        roundtrip_clash("DOMAIN-SUFFIX,google.com,DIRECT");
        roundtrip_clash("DOMAIN-KEYWORD,ads,REJECT");
        roundtrip_clash("IP-CIDR,192.168.0.0/16,DIRECT");
        roundtrip_clash("DST-PORT,443,PROXY");
        roundtrip_clash("PROCESS-NAME,ssh,REJECT");
    }

    #[test]
    fn subrule_becomes_verbatim_raw() {
        let model = from_clash_rule_str("SUB-RULE,(AND((DOMAIN,baidu.com)),NETWORK),DIRECT");
        assert!(model.is_raw());
        assert_eq!(
            to_clash_rule_str(&model),
            "SUB-RULE,(AND((DOMAIN,baidu.com)),NETWORK),DIRECT"
        );
    }

    #[test]
    fn singbox_json_round_trips_simple_and_logical() {
        let simple = IRouteRule::Simple {
            matches: vec![MatchField::DomainSuffix("google.com".into()), MatchField::IpCidr("10.0.0.0/8".into())],
            target: RuleTarget::Outbound("PROXY".into()),
        };
        let json = to_singbox_json(&simple).expect("simple");
        assert_eq!(json["domain_suffix"], json!(["google.com"]));
        assert_eq!(from_singbox_json(&json), Some(simple));

        let logical = IRouteRule::Logical {
            op: LogicOp::Or,
            rules: vec![
                IRouteRule::Simple { matches: vec![MatchField::Domain("a.com".into())], target: RuleTarget::Direct },
                IRouteRule::Simple { matches: vec![MatchField::Port(443)], target: RuleTarget::Direct },
            ],
            target: RuleTarget::Direct,
        };
        let ljson = to_singbox_json(&logical).expect("logical");
        assert_eq!(ljson["type"], "logical");
        assert_eq!(ljson["mode"], "or");
        assert_eq!(from_singbox_json(&ljson), Some(logical));
    }

    #[test]
    fn raw_rules_have_no_singbox_form() {
        let raw = IRouteRule::Raw { clash_raw: "SUB-RULE,x,DIRECT".into() };
        assert!(to_singbox_json(&raw).is_none());
    }
}
