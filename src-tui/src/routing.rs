//! Unified route rule model (`IRouteRule`) with dual-core serialization
//! (tasks 6.1-6.3 of add-singbox-dual-core).
//!
//! One model, two formats: clash YAML rule strings
//! (`DOMAIN-SUFFIX,google.com,PROXY`) and sing-box route rule JSON
//! objects (fields grouped into arrays, logical rules nested).
//! Rules neither format can express round-trip through [`IRouteRule::Raw`]
//! verbatim — data loss is impossible by construction; callers block
//! saves when a Raw fragment would have to be reinterpreted.

use serde_json::{Value, json};

/// A single match condition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MatchField {
    Domain(String),
    DomainSuffix(String),
    DomainKeyword(String),
    IpCidr(String),
    Port(u16),
    Process(String),
    /// A clash `RULE-SET,<provider>` reference (a `rule-providers` entry).
    RuleSet(String),
    /// A clash `GEOIP,<value>` / `GEOSITE,<value>` reference, kept verbatim.
    ///
    /// The geo databases have no sing-box route field — they are official
    /// SagerNet rule-sets — but the clash spelling must still survive the
    /// TUI rules editor: the original kind and value are kept on the model
    /// so `GEOIP,CN,DIRECT` is written back as `GEOIP,CN,DIRECT` instead
    /// of degenerating into the match-less `DIRECT` (#P0-1).
    GeoSet {
        kind: GeoKind,
        value: String,
    },
}

/// Which clash geo database a [`MatchField::GeoSet`] came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GeoKind {
    /// `GEOIP,<value>` — IP ranges.
    Ip,
    /// `GEOSITE,<value>` — domains.
    Site,
}

impl GeoKind {
    /// The clash rule header this kind serializes back to.
    pub fn clash_header(self) -> &'static str {
        match self {
            GeoKind::Ip => "GEOIP",
            GeoKind::Site => "GEOSITE",
        }
    }

    /// The sing-box geo rule-set tag prefix for this kind.
    pub fn tag_prefix(self) -> &'static str {
        match self {
            GeoKind::Ip => "geoip",
            GeoKind::Site => "geosite",
        }
    }
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
#[allow(dead_code)]
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
    #[allow(dead_code)]
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
    #[allow(dead_code)]
    pub fn is_raw(&self) -> bool {
        matches!(self, IRouteRule::Raw { .. })
    }
}

// ---------- clash YAML rule strings ----------

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
        // clash's own spelling for a rule-provider reference.
        MatchField::RuleSet(v) => format!("RULE-SET,{v}"),
        // The original clash geo spelling, preserved verbatim (#P0-1).
        MatchField::GeoSet { kind, value } => format!("{},{value}", kind.clash_header()),
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
        // #52: the geo databases have no sing-box route field; they are
        // official SagerNet `.srs` rule-sets. The clash kind and value stay
        // on the model so the rule round-trips through the clash
        // serializer (#P0-1), and `geo_field` gates the value against the
        // published rule-set names (#P0-2) — an unmappable value (or a
        // negated set such as `!cn`, which has no positive rule-set
        // reference) leaves the rule as verbatim Raw.
        "GEOIP" => geo_field(GeoKind::Ip, value)?,
        "GEOSITE" => geo_field(GeoKind::Site, value)?,
        _ => return None,
    })
}

/// Parse one clash geo reference, keeping the original kind and value.
///
/// The tag is only ever synthesized from the names published in
/// `SagerNet/sing-geoip` / `SagerNet/sing-geosite` (plus the explicit
/// pseudo-code table in [`crate::singbox::convert`]): a guessed tag such as
/// `geoip-lan` 404s on download and aborts the core. A value with no
/// sing-box form (or a negated set such as `!cn`, which has no positive
/// rule-set reference) yields `None`, and the caller keeps the rule as
/// verbatim Raw — clash keeps working, sing-box reports the drop.
fn geo_field(kind: GeoKind, value: &str) -> Option<MatchField> {
    use crate::singbox::convert::{GeoValueMatch, classify_geo_value};
    match classify_geo_value(kind.tag_prefix(), value)? {
        GeoValueMatch::RuleSet(_) | GeoValueMatch::PrivateNetworks => Some(MatchField::GeoSet {
            kind,
            value: value.to_string(),
        }),
    }
}

/// The private address ranges clash's `GEOIP,LAN`/`GEOIP,private`
/// pseudo-databases resolve to (mihomo's GeoIP database).
const PRIVATE_NETWORK_CIDRS: &[&str] = &[
    "0.0.0.0/8",
    "10.0.0.0/8",
    "100.64.0.0/10",
    "127.0.0.0/8",
    "169.254.0.0/16",
    "172.16.0.0/12",
    "192.168.0.0/16",
    "::1/128",
    "fc00::/7",
    "fe80::/10",
];

/// Clash modifiers that only affect clash's own resolution behaviour and
/// carry no sing-box equivalent — and therefore need no warning beyond the
/// conversion report.
///
/// `no-resolve` is the common one: sing-box never resolves a domain for an
/// `ip_cidr` rule, which is exactly what `no-resolve` asks for.
const TOLERATED_MODIFIERS: &[&str] = &["no-resolve"];

/// Split a clash rule string into its fields and modifiers.
///
/// Clash appends modifiers after the target (`IP-CIDR,10.0.0.0/8,DIRECT,
/// no-resolve`) or before it (`IP-CIDR,10.0.0.0/8,no-resolve,DIRECT`);
/// both spellings occur in real subscriptions. Returns
/// `(fields, modifiers)` where `fields` still has the target last.
pub fn split_clash_rule(rule: &str) -> (Vec<String>, Vec<String>) {
    let parts: Vec<String> = rule.split(',').map(str::trim).map(str::to_string).collect();
    let mut modifiers: Vec<String> = Vec::new();
    let mut fields: Vec<String> = Vec::new();
    for (index, part) in parts.iter().enumerate() {
        let is_modifier = TOLERATED_MODIFIERS
            .iter()
            .any(|modifier| part.eq_ignore_ascii_case(modifier));
        // The first field is the rule kind and can never be a modifier; the
        // last non-modifier part is the target.
        if is_modifier && index > 0 {
            modifiers.push(part.clone());
        } else {
            fields.push(part.clone());
        }
    }
    (fields, modifiers)
}

/// Parse a clash rule string. Unrecognized kinds become Raw passthrough.
///
/// Trailing modifiers (`no-resolve`) are dropped — sing-box applies the
/// equivalent behaviour by default for IP rules (#52) — so an
/// `IP-CIDR,...,DIRECT,no-resolve` rule converts like its plain form
/// instead of degenerating into a rule targeting `no-resolve`.
pub fn from_clash_rule_str(rule: &str) -> IRouteRule {
    let (mut parts, _modifiers) = split_clash_rule(rule);
    if parts.len() < 2 {
        return IRouteRule::Raw { clash_raw: rule.into() };
    }
    // SUB-RULE and other exotic headers pass through verbatim.
    if matches!(parts[0].as_str(), "SUB-RULE" | "AND" | "OR" | "NOT") {
        return IRouteRule::Raw { clash_raw: rule.into() };
    }
    let target = parts.pop().expect("length checked above");
    let Some(target) = target_from_clash_str(&target) else {
        return IRouteRule::Raw { clash_raw: rule.into() };
    };
    if parts.len() != 2 {
        // More than one match field in one clash rule (or a modifier we do
        // not tolerate) has no representation in this model.
        return IRouteRule::Raw { clash_raw: rule.into() };
    }
    let value = parts.pop().expect("length checked above");
    let Some(field) = field_from_clash_str(&parts[0], &value) else {
        return IRouteRule::Raw { clash_raw: rule.into() };
    };
    IRouteRule::Simple {
        matches: vec![field],
        target,
    }
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

/// Write one match field into a sing-box route rule object.
///
/// Returns `false` for a field this model cannot express for sing-box, so
/// the caller drops the whole rule instead of emitting one that silently
/// matches everything.
#[allow(dead_code)]
fn field_to_singbox(field: &MatchField, rule: &mut Value) -> bool {
    let (key, value) = match field {
        MatchField::Domain(v) => ("domain", json!([v])),
        MatchField::DomainSuffix(v) => ("domain_suffix", json!([v])),
        MatchField::DomainKeyword(v) => ("domain_keyword", json!([v])),
        MatchField::IpCidr(v) => ("ip_cidr", json!([v])),
        MatchField::Port(v) => ("port", json!([v])),
        MatchField::Process(v) => ("process_name", json!([v])),
        MatchField::RuleSet(v) => ("rule_set", json!([v])),
        MatchField::GeoSet { kind, value } => {
            match crate::singbox::convert::classify_geo_value(kind.tag_prefix(), value) {
                // clash's private-range pseudo-databases have no published
                // rule-set, so they become the literal ranges they mean.
                Some(crate::singbox::convert::GeoValueMatch::PrivateNetworks) => {
                    ("ip_cidr", json!(PRIVATE_NETWORK_CIDRS))
                }
                Some(crate::singbox::convert::GeoValueMatch::RuleSet(tag)) => ("rule_set", json!([tag])),
                // Only reachable for a hand-built model; a clash-parsed rule
                // was validated at parse time.
                None => return false,
            }
        }
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
    true
}

#[allow(dead_code)]
fn target_to_singbox(target: &RuleTarget, rule: &mut Value) {
    rule["outbound"] = match target {
        RuleTarget::Outbound(name) => json!(name),
        RuleTarget::Direct => json!("direct"),
        RuleTarget::Block => json!("block"),
    };
}

#[allow(dead_code)]
fn target_from_singbox(rule: &Value) -> Option<RuleTarget> {
    let outbound = rule.get("outbound")?.as_str()?;
    Some(match outbound {
        "direct" => RuleTarget::Direct,
        "block" => RuleTarget::Block,
        other => RuleTarget::Outbound(other.into()),
    })
}

#[allow(dead_code)]
fn field_from_singbox(rule: &Value, key: &str, make: fn(String) -> MatchField) -> Option<MatchField> {
    rule.get(key)?.as_array()?.first()?.as_str().map(|v| make(v.into()))
}

#[allow(dead_code)]
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
        // A port outside the u16 range is not a port: truncating it would
        // silently route a `70000` rule as `4464`, so the object is not
        // expressible and the caller keeps the raw JSON.
        let port = u16::try_from(port.as_u64()?).ok()?;
        matches.push(MatchField::Port(port));
    }
    let target = target_from_singbox(rule)?;
    Some(IRouteRule::Simple { matches, target })
}

/// Convert an IRouteRule into a sing-box route rule JSON object.
/// Raw rules cannot be represented — callers must splice them verbatim
/// into the profile (the generator keeps them out of this path).
#[allow(dead_code)]
pub fn to_singbox_json(rule: &IRouteRule) -> Option<Value> {
    match rule {
        IRouteRule::Raw { .. } => None,
        IRouteRule::Simple { matches, target } => {
            let mut rule = json!({});
            for field in matches {
                if !field_to_singbox(field, &mut rule) {
                    return None;
                }
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
#[allow(dead_code)]
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
            matches: vec![
                MatchField::DomainSuffix("google.com".into()),
                MatchField::IpCidr("10.0.0.0/8".into()),
            ],
            target: RuleTarget::Outbound("PROXY".into()),
        };
        let json = to_singbox_json(&simple).expect("simple");
        assert_eq!(json["domain_suffix"], json!(["google.com"]));
        assert_eq!(from_singbox_json(&json), Some(simple));

        let logical = IRouteRule::Logical {
            op: LogicOp::Or,
            rules: vec![
                IRouteRule::Simple {
                    matches: vec![MatchField::Domain("a.com".into())],
                    target: RuleTarget::Direct,
                },
                IRouteRule::Simple {
                    matches: vec![MatchField::Port(443)],
                    target: RuleTarget::Direct,
                },
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
        let raw = IRouteRule::Raw {
            clash_raw: "SUB-RULE,x,DIRECT".into(),
        };
        assert!(to_singbox_json(&raw).is_none());
    }

    #[test]
    fn no_resolve_modifier_is_dropped_and_the_rule_still_converts() {
        // #52: `no-resolve` is the most common modifier in real
        // subscriptions and sing-box never resolves domains for an
        // ip_cidr rule anyway, so the plain form is the correct mapping.
        for (rule, cidr) in [
            ("IP-CIDR,10.0.0.0/8,DIRECT,no-resolve", "10.0.0.0/8"),
            ("IP-CIDR,10.0.0.0/8,no-resolve,DIRECT", "10.0.0.0/8"),
            ("IP-CIDR6,2001:db8::/32,REJECT,no-resolve", "2001:db8::/32"),
        ] {
            let model = from_clash_rule_str(rule);
            let json = to_singbox_json(&model).unwrap_or_else(|| panic!("{rule} must convert: {model:?}"));
            assert_eq!(json["ip_cidr"], json!([cidr]), "{rule}");
        }
        // The clash form is preserved without the modifier it no longer needs.
        assert_eq!(
            to_clash_rule_str(&from_clash_rule_str("IP-CIDR,10.0.0.0/8,DIRECT,no-resolve")),
            "IP-CIDR,10.0.0.0/8,DIRECT"
        );
    }

    #[test]
    fn geoip_and_geosite_map_to_official_rule_set_tags() {
        let json = to_singbox_json(&from_clash_rule_str("GEOIP,CN,DIRECT")).expect("geoip");
        assert_eq!(json["rule_set"], json!(["geoip-cn"]));
        let json = to_singbox_json(&from_clash_rule_str("GEOSITE,geolocation-!cn,REJECT")).expect("geosite");
        assert_eq!(json["rule_set"], json!(["geosite-geolocation-!cn"]));
        // A negated geo set has no positive rule-set reference.
        assert!(to_singbox_json(&from_clash_rule_str("GEOIP,!cn,DIRECT")).is_none());
        assert!(to_singbox_json(&from_clash_rule_str("GEOSITE,!cn,DIRECT")).is_none());
    }

    #[test]
    fn multiple_match_fields_in_one_clash_rule_stay_raw() {
        // e.g. `IP-CIDR,10.0.0.0/8,udp,DIRECT`: the protocol qualifier has
        // no sing-box route-rule equivalent, so the rule is not silently
        // reinterpreted as something weaker.
        let model = from_clash_rule_str("IP-CIDR,10.0.0.0/8,udp,DIRECT");
        assert!(model.is_raw(), "{model:?}");
        assert_eq!(to_clash_rule_str(&model), "IP-CIDR,10.0.0.0/8,udp,DIRECT");
    }

    #[test]
    fn geo_and_rule_set_rules_round_trip_through_clash() {
        // #P0-1: the rules editor rewrites the whole `rules:` buffer on
        // save; before this fix every GEOIP/GEOSITE/RULE-SET line was
        // written back as the bare target string (`DIRECT`), silently
        // deleting the match condition of every geo rule in the profile.
        for rule in [
            "GEOIP,CN,DIRECT",
            "GEOIP,cn,REJECT",
            "GEOSITE,category-ads-all,REJECT",
            "GEOSITE,geolocation-!cn,DIRECT",
            "RULE-SET,x,PROXY",
        ] {
            roundtrip_clash(rule);
        }
        // `no-resolve` is dropped (see the test below), but the match
        // condition must survive: the bug turned this line into `DIRECT`.
        assert_eq!(
            to_clash_rule_str(&from_clash_rule_str("GEOIP,CN,DIRECT,no-resolve")),
            "GEOIP,CN,DIRECT"
        );
        assert_eq!(
            to_clash_rule_str(&from_clash_rule_str("RULE-SET,x,PROXY,no-resolve")),
            "RULE-SET,x,PROXY"
        );
    }

    #[test]
    fn rule_set_and_geo_set_rules_keep_their_match_condition() {
        for rule in ["GEOIP,CN,DIRECT", "GEOSITE,category-ads-all,REJECT", "RULE-SET,x,PROXY"] {
            let back = to_clash_rule_str(&from_clash_rule_str(rule));
            assert_ne!(back, "DIRECT", "{rule} lost its match condition");
            assert_ne!(back, "REJECT", "{rule} lost its match condition");
            assert_eq!(back.split(',').count(), 3, "{rule} -> {back}");
        }
    }

    #[test]
    fn geo_references_keep_their_original_kind_on_the_model() {
        assert_eq!(
            from_clash_rule_str("GEOIP,CN,DIRECT"),
            IRouteRule::Simple {
                matches: vec![MatchField::GeoSet {
                    kind: GeoKind::Ip,
                    value: "CN".into(),
                }],
                target: RuleTarget::Direct,
            }
        );
        assert_eq!(
            from_clash_rule_str("GEOSITE,category-ads-all,REJECT"),
            IRouteRule::Simple {
                matches: vec![MatchField::GeoSet {
                    kind: GeoKind::Site,
                    value: "category-ads-all".into(),
                }],
                target: RuleTarget::Block,
            }
        );
    }

    #[test]
    fn unpublished_geo_values_stay_raw_instead_of_guessing_a_tag() {
        // A guessed tag (the old `geoip-<value>` synthesis) 404s at
        // rule-set initialization and aborts sing-box, so an unmappable
        // value must stay verbatim Raw: clash keeps the rule, sing-box
        // reports the drop.
        for rule in [
            "GEOIP,ZZ,DIRECT",
            "GEOIP,telegram,DIRECT",
            "GEOSITE,not-a-published-site,DIRECT",
        ] {
            let model = from_clash_rule_str(rule);
            assert!(model.is_raw(), "{rule} must not become a rule-set: {model:?}");
            assert_eq!(to_clash_rule_str(&model), rule);
            assert!(to_singbox_json(&model).is_none(), "{rule}");
        }
    }

    #[test]
    fn geoip_private_ranges_map_to_literal_cidrs_without_a_download() {
        // `GEOIP,LAN` is a clash pseudo-database, not a downloadable set:
        // the SagerNet repositories publish no `geoip-lan`/`geoip-private`
        // (verified 404), and `geosite-private` would match private
        // *domains* instead of private IPs. The equivalent literal ranges
        // keep `GEOIP,LAN,DIRECT` meaning "private traffic goes direct".
        for rule in ["GEOIP,LAN,DIRECT", "GEOIP,private,REJECT", "GEOIP,local,DIRECT"] {
            roundtrip_clash(rule);
            let json = to_singbox_json(&from_clash_rule_str(rule)).unwrap_or_else(|| panic!("{rule}"));
            let cidrs = json["ip_cidr"].as_array().expect("ip_cidr array").clone();
            for expected in ["10.0.0.0/8", "172.16.0.0/12", "192.168.0.0/16", "fc00::/7"] {
                assert!(cidrs.contains(&json!(expected)), "{rule}: {cidrs:?} lacks {expected}");
            }
        }
        assert!(crate::singbox::convert::geo_rule_set("geoip-lan").is_none());
        assert!(crate::singbox::convert::geo_rule_set("geoip-private").is_none());
        assert!(crate::singbox::convert::geo_rule_set("geoip-local").is_none());
    }

    #[test]
    fn geosite_private_maps_to_the_published_private_set() {
        let json = to_singbox_json(&from_clash_rule_str("GEOSITE,private,DIRECT")).expect("geosite");
        assert_eq!(json["rule_set"], json!(["geosite-private"]));
        assert!(crate::singbox::convert::geo_rule_set("geosite-private").is_some());
        roundtrip_clash("GEOSITE,private,DIRECT");
    }

    #[test]
    fn singbox_ports_outside_the_u16_range_are_rejected_not_truncated() {
        // #P2: `port: 70000` used to become `4464`, silently routing on a
        // different port than the profile asked for.
        assert_eq!(from_singbox_json(&json!({"port": [70000], "outbound": "direct"})), None);
        assert_eq!(from_singbox_json(&json!({"port": [65536], "outbound": "direct"})), None);
        assert_eq!(
            from_singbox_json(&json!({"port": [443], "outbound": "direct"})),
            Some(IRouteRule::Simple {
                matches: vec![MatchField::Port(443)],
                target: RuleTarget::Direct,
            })
        );
    }
}

// ---------- Task 7.1 core: profile rules load/save ----------

/// Load the `rules:` list of a clash profile document into the unified
/// model. Unexpressible entries come back as Raw passthrough.
pub fn load_profile_rules(config_yaml: &str) -> Result<Vec<IRouteRule>, String> {
    let doc: serde_yaml_ng::Value = serde_yaml_ng::from_str(config_yaml).map_err(|e| format!("invalid YAML: {e}"))?;
    load_rules_from_doc(&doc)
}

fn load_rules_from_doc(doc: &serde_yaml_ng::Value) -> Result<Vec<IRouteRule>, String> {
    let Some(rules_seq) = doc.get("rules").and_then(|v| v.as_sequence().cloned()) else {
        return Ok(Vec::new());
    };
    Ok(rules_seq
        .iter()
        .filter_map(|r| r.as_str())
        .map(from_clash_rule_str)
        .collect())
}

/// Write the rule list back into a clash profile document, preserving
/// everything else verbatim. Logical rules cannot live in clash YAML —
/// callers must refuse the save instead of losing them.
pub fn save_profile_rules(config_yaml: &str, rules: &[IRouteRule]) -> Result<String, String> {
    if rules.iter().any(|r| matches!(r, IRouteRule::Logical { .. })) {
        return Err("logical rules cannot be saved to a clash profile — switch to sing-box".into());
    }
    let mut doc: serde_yaml_ng::Value =
        serde_yaml_ng::from_str(config_yaml).map_err(|e| format!("invalid YAML: {e}"))?;
    let seq: Vec<serde_yaml_ng::Value> = rules
        .iter()
        .map(|r| {
            let raw = match r {
                IRouteRule::Raw { clash_raw } => clash_raw.clone(),
                other => to_clash_rule_str(other),
            };
            serde_yaml_ng::Value::String(raw)
        })
        .collect();
    if let Some(mapping) = doc.as_mapping_mut() {
        use serde_yaml_ng::mapping::Entry;
        let entry = mapping.entry(serde_yaml_ng::Value::String("rules".into()));
        match entry {
            Entry::Occupied(mut o) => {
                o.insert(serde_yaml_ng::Value::Sequence(seq));
            }
            Entry::Vacant(v) => {
                v.insert(serde_yaml_ng::Value::Sequence(seq));
            }
        }
    }
    serde_yaml_ng::to_string(&doc).map_err(|e| format!("serialize failed: {e}"))
}

#[cfg(test)]
mod profile_rules_tests {
    use super::*;

    const SAMPLE: &str =
        "mode: rule\nproxies: []\nrules:\n  - DOMAIN,example.com,PROXY\n  - IP-CIDR,10.0.0.0/8,DIRECT\n";

    #[test]
    fn loads_and_saves_profile_rules_round_trip() {
        let rules = load_profile_rules(SAMPLE).expect("load");
        assert_eq!(rules.len(), 2);

        let mut edited = rules.clone();
        edited.remove(1);
        let saved = save_profile_rules(SAMPLE, &edited).expect("save");

        let reloaded = load_profile_rules(&saved).expect("reload");
        assert_eq!(reloaded.len(), 1);
        assert!(saved.contains("mode: rule"), "non-rule keys survive");
    }

    #[test]
    fn raw_rules_survive_save_verbatim() {
        let rules = vec![from_clash_rule_str("SUB-RULE,(AND((DOMAIN,b.com))),DIRECT")];
        let saved = save_profile_rules(SAMPLE, &rules).expect("save");
        assert!(saved.contains("SUB-RULE,(AND((DOMAIN,b.com))),DIRECT"), "{saved}");
    }

    #[test]
    fn logical_rules_block_mihomo_save() {
        let rules = vec![IRouteRule::Logical {
            op: LogicOp::Or,
            rules: vec![],
            target: RuleTarget::Direct,
        }];
        let err = save_profile_rules(SAMPLE, &rules).expect_err("must block");
        assert!(err.contains("cannot be saved"), "{err}");
    }

    #[test]
    fn profile_without_rules_yields_empty() {
        assert!(load_profile_rules("mode: rule").expect("load").is_empty());
    }

    #[test]
    fn geo_rules_survive_the_editor_save_round_trip() {
        // The rules editor path: load -> (unchanged) -> save must not
        // rewrite `GEOIP,CN,DIRECT` as the bare `DIRECT`.
        let profile = "mode: rule\nrules:\n  - GEOIP,CN,DIRECT\n  - GEOSITE,category-ads-all,REJECT\n  - RULE-SET,ads,REJECT\n  - GEOIP,LAN,DIRECT\n";
        let rules = load_profile_rules(profile).expect("load");
        assert_eq!(rules.len(), 4);
        let saved = save_profile_rules(profile, &rules).expect("save");
        for rule in [
            "GEOIP,CN,DIRECT",
            "GEOSITE,category-ads-all,REJECT",
            "RULE-SET,ads,REJECT",
            "GEOIP,LAN,DIRECT",
        ] {
            assert!(saved.contains(rule), "{rule} was lost:\n{saved}");
        }
        // And the reloaded profile is the one the user started with.
        assert_eq!(load_profile_rules(&saved).expect("reload"), rules);
    }
}

/// Human-readable one-line description for list rendering (task 7.3).
/// Logical rules have no clash string form, so they get a summary.
pub fn describe(rule: &IRouteRule) -> String {
    match rule {
        IRouteRule::Logical { op, rules, target } => {
            let op_str = match op {
                LogicOp::And => "AND",
                LogicOp::Or => "OR",
            };
            let target = target_to_clash_str(target);
            format!("({op_str}: {} rules -> {target})", rules.len())
        }
        other => to_clash_rule_str(other),
    }
}

#[cfg(test)]
mod describe_tests {
    use super::*;

    #[test]
    fn logical_rules_describe_as_summary() {
        let rule = IRouteRule::Logical {
            op: LogicOp::Or,
            rules: vec![
                IRouteRule::Simple {
                    matches: vec![MatchField::Port(443)],
                    target: RuleTarget::Direct,
                },
                IRouteRule::Simple {
                    matches: vec![MatchField::Domain("a.com".into())],
                    target: RuleTarget::Direct,
                },
            ],
            target: RuleTarget::Direct,
        };
        assert_eq!(describe(&rule), "(OR: 2 rules -> DIRECT)");
    }
}

// ---------- Task 7.2: structured rule form ----------

/// Parse one `kind=value>target` spec from the rule editor form into a
/// `Simple` rule. Kinds: `domain`/`suffix`/`keyword`/`ip`/`port`/`process`/
/// `set`; target is `DIRECT`, `REJECT`, or an outbound group name.
///
/// This is the structured construction path (task 7.2): the user picks from
/// validated match kinds instead of memorizing clash rule syntax. Raw clash
/// strings and sing-box JSON remain available via the existing inputs.
pub fn build_simple_rule(spec: &str) -> Result<IRouteRule, String> {
    let spec = spec.trim();
    let Some((left, target)) = spec.split_once('>') else {
        return Err("expected kind=value>target (missing '>')".into());
    };
    let target = target.trim();
    if target.is_empty() {
        return Err("empty target after '>'".into());
    }
    let target = match target {
        "DIRECT" => RuleTarget::Direct,
        "REJECT" => RuleTarget::Block,
        other => RuleTarget::Outbound(other.to_string()),
    };
    let Some((kind, value)) = left.split_once('=') else {
        return Err("expected kind=value>target (missing '=')".into());
    };
    let value = value.trim();
    if value.is_empty() {
        return Err("empty match value".into());
    }
    let field = match kind.trim() {
        "domain" => MatchField::Domain(value.into()),
        "suffix" => MatchField::DomainSuffix(value.into()),
        "keyword" => MatchField::DomainKeyword(value.into()),
        "ip" | "cidr" => MatchField::IpCidr(value.into()),
        "port" => MatchField::Port(value.parse().map_err(|_| format!("invalid port: {value}"))?),
        "process" => MatchField::Process(value.into()),
        "set" => MatchField::RuleSet(value.into()),
        other => {
            return Err(format!(
                "unknown kind '{other}' (use domain/suffix/keyword/ip/port/process/set)"
            ));
        }
    };
    Ok(IRouteRule::Simple {
        matches: vec![field],
        target,
    })
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod form_tests {
    use super::*;

    #[test]
    fn builds_simple_rules_from_form_specs() {
        let built = build_simple_rule("suffix=google.com > PROXY").expect("suffix");
        assert_eq!(
            built,
            IRouteRule::Simple {
                matches: vec![MatchField::DomainSuffix("google.com".into())],
                target: RuleTarget::Outbound("PROXY".into()),
            }
        );

        assert_eq!(
            build_simple_rule("ip=10.0.0.0/8>DIRECT").expect("ip"),
            IRouteRule::Simple {
                matches: vec![MatchField::IpCidr("10.0.0.0/8".into())],
                target: RuleTarget::Direct,
            }
        );
        assert_eq!(
            build_simple_rule("set=geoip>REJECT").expect("set"),
            IRouteRule::Simple {
                matches: vec![MatchField::RuleSet("geoip".into())],
                target: RuleTarget::Block,
            }
        );
        assert_eq!(
            build_simple_rule("port=443>PROXY").expect("port"),
            IRouteRule::Simple {
                matches: vec![MatchField::Port(443)],
                target: RuleTarget::Outbound("PROXY".into()),
            }
        );
    }

    #[test]
    fn form_specs_are_validated() {
        assert!(build_simple_rule("no-separator").is_err());
        assert!(build_simple_rule("domain=a.com>").is_err(), "empty target");
        assert!(build_simple_rule("domain=>PROXY").is_err(), "empty value");
        assert!(build_simple_rule("bogus=x>PROXY").is_err(), "unknown kind");
        assert!(build_simple_rule("port=http>PROXY").is_err(), "bad port");
    }

    #[test]
    fn form_rules_serialize_to_clash_and_singbox() {
        let built = build_simple_rule("process=ssh>REJECT").expect("process");
        assert_eq!(to_clash_rule_str(&built), "PROCESS-NAME,ssh,REJECT");
        let json = to_singbox_json(&built).expect("singbox form");
        assert_eq!(json["process_name"], json!(["ssh"]));
        assert_eq!(json["outbound"], "block");
    }
}
