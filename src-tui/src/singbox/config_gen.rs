//! Skeleton generator for the sing-box runtime config (`singbox.json`).
//!
//! Shape verified against sing-box 1.13 docs/examples:
//! - mixed inbound on a local port for SOCKS/HTTP
//! - optional tun inbound with an explicit `interface_name` so the
//!   lifecycle resource barrier can poll a deterministic device
//! - selector/urltest outbound groups derived from the profile structure;
//!   node outbounds arrive pre-converted and are spliced verbatim
//! - `experimental.clash_api` bound to a local TCP address (sing-box's
//!   clash_api does not support unix sockets)
//!
//! Route rules and DNS arrive pre-built (`ConfigInput::route_rules` /
//! `ConfigInput::dns`) so this module stays a pure shape assembler; unknown
//! fields survive because generation starts from scratch each time the
//! profile changes.

use serde_json::{Value, json};
use std::net::SocketAddr;

/// A selector or urltest outbound group derived from the profile.
#[derive(Debug, Clone)]
pub struct GroupSpec {
    pub name: String,
    pub kind: GroupKind,
    /// Member tags — node outbound tags or other group tags.
    pub members: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupKind {
    Selector,
    UrlTest,
}

/// TUN inbound settings (only applied when [`ConfigInput::enable_tun`]).
#[derive(Debug, Clone)]
pub struct TunSettings {
    /// gVisor | system | mixed
    pub stack: String,
    pub mtu: u16,
}

/// sing-box `experimental.clash_api` settings. TCP only — sing-box's
/// clash_api has no unix-socket support.
#[derive(Debug, Clone)]
pub struct ClashApiSettings {
    pub listen: SocketAddr,
    pub secret: String,
}

/// Everything the generator needs. Node outbounds must already be valid
/// sing-box outbound objects with unique `tag`s (the converter guarantees
/// this); groups reference those tags by name.
#[derive(Debug, Clone)]
pub struct ConfigInput {
    /// Pre-converted node outbounds (full JSON objects).
    pub outbounds: Vec<Value>,
    pub groups: Vec<GroupSpec>,
    pub mixed_port: u16,
    pub enable_tun: bool,
    pub tun: TunSettings,
    pub clash_api: ClashApiSettings,
    /// sing-box rule-set definitions emitted beneath `route`; rules reference
    /// them by tag.
    pub rule_sets: Vec<Value>,
    /// Pre-converted sing-box route rule objects (task 7.5): profile rules
    /// via `routing::to_singbox_json` plus stored logical rules.
    pub route_rules: Vec<Value>,
    /// Pre-built DNS section (task 8.1, 1.12+ new format); omitted when None.
    pub dns: Option<Value>,
}

const TUN_INTERFACE_NAME: &str = "sb-tun0";
const DIRECT_TAG: &str = "direct";
/// Default urltest probe URL (https mandatory: sing-box silently drops
/// http URLs and falls back to its own default).
pub const URLTEST_URL: &str = "https://www.gstatic.com/generate_204";

/// Generate the sing-box runtime config skeleton.
///
/// Errors when two outbounds or groups share a tag — that would make the
/// core reject the whole file at start, and we prefer failing here where
/// we can point at the offending name.
pub fn generate_config(input: &ConfigInput) -> Result<Value, String> {
    generate_config_for_version(input, crate::singbox::capabilities::SING_BOX_1_14_2.version)
}

pub fn generate_config_for_version(input: &ConfigInput, version: &str) -> Result<Value, String> {
    let matrix = crate::singbox::capabilities::require_for_version(version)?;
    if input.enable_tun {
        validate_tun_stack(&input.tun.stack)?;
        if input.tun.stack == "gvisor" && !matrix.tun_gvisor {
            return Err(format!("sing-box {} does not support gvisor TUN stack", matrix.version));
        }
    }
    if !input.rule_sets.is_empty() && !matrix.route_rule_sets_nested {
        return Err(format!(
            "sing-box {} does not support nested route rule sets",
            matrix.version
        ));
    }
    if !input.route_rules.is_empty() && !matrix.route_rule_references {
        return Err(format!(
            "sing-box {} has no reviewed route-reference support",
            matrix.version
        ));
    }
    let mut tags = std::collections::HashSet::new();
    for outbound in &input.outbounds {
        let tag = outbound
            .get("tag")
            .and_then(Value::as_str)
            .ok_or_else(|| "node outbound missing tag".to_string())?;
        if is_reserved_tag(tag) {
            return Err(format!(
                "outbound tag {tag:?} is reserved for a built-in policy outbound"
            ));
        }
        if !tags.insert(tag.to_string()) {
            return Err(format!("duplicate outbound tag: {tag}"));
        }
    }
    for group in &input.groups {
        if is_reserved_tag(&group.name) {
            return Err(format!(
                "group tag {:?} is reserved for a built-in policy outbound",
                group.name
            ));
        }
        if !tags.insert(group.name.clone()) {
            return Err(format!("duplicate outbound tag: {}", group.name));
        }
    }

    // Empty groups are rejected by sing-box at startup — skip them instead.
    let groups: Vec<&GroupSpec> = input.groups.iter().filter(|g| !g.members.is_empty()).collect();
    let final_outbound = groups
        .iter()
        .find(|g| g.kind == GroupKind::Selector)
        .map(|g| g.name.as_str())
        .unwrap_or(DIRECT_TAG);

    let inbounds = control_plane_inbounds(input.mixed_port, input.enable_tun, &input.tun);

    let mut outbounds: Vec<Value> = input.outbounds.clone();
    for group in &groups {
        match group.kind {
            GroupKind::Selector => {
                // Selectors always expose an explicit direct fallback.
                let mut members: Vec<String> = group
                    .members
                    .iter()
                    .map(|member| normalize_policy_tag(member))
                    .collect();
                members.push(DIRECT_TAG.to_string());
                outbounds.push(json!({
                    "type": "selector",
                    "tag": group.name,
                    "outbounds": members,
                    "default": group.members.first().map(|member| normalize_policy_tag(member)).unwrap_or_else(|| DIRECT_TAG.into()),
                }));
            }
            GroupKind::UrlTest => {
                outbounds.push(json!({
                    "type": "urltest",
                    "tag": group.name,
                    "outbounds": group.members.iter().map(|member| normalize_policy_tag(member)).collect::<Vec<_>>(),
                    "url": URLTEST_URL,
                    "interval": "3m",
                    "tolerance": 50,
                }));
            }
        }
    }
    outbounds.push(json!({ "type": "direct", "tag": DIRECT_TAG }));
    outbounds.push(json!({ "type": "block", "tag": "block" }));

    let mut config = json!({
        "log": {
            "level": "info",
            "timestamp": true,
        },
        "inbounds": inbounds,
        "outbounds": outbounds,
        "route": {
            "final": final_outbound,
            "auto_detect_interface": true,
        },
        "experimental": {
            "clash_api": {
                "external_controller": input.clash_api.listen.to_string(),
                "secret": input.clash_api.secret,
            }
        }
    });
    if !input.rule_sets.is_empty() {
        config["route"]["rule_set"] = Value::Array(input.rule_sets.clone());
    }
    if !input.route_rules.is_empty() {
        let mut rules = input.route_rules.clone();
        normalize_rule_targets(&mut rules);
        config["route"]["rules"] = Value::Array(rules);
    }
    if let Some(dns) = &input.dns {
        config["dns"] = dns.clone();
    }
    validate_references(&config)?;
    Ok(config)
}

pub fn validate_tun_stack(stack: &str) -> Result<(), String> {
    if matches!(stack, "system" | "gvisor" | "mixed") {
        Ok(())
    } else {
        Err(format!(
            "unsupported sing-box TUN stack {stack:?}; supported stacks are system, gvisor, mixed"
        ))
    }
}

fn normalize_policy_tag(tag: &str) -> String {
    match tag {
        "DIRECT" => "direct".into(),
        "REJECT" => "block".into(),
        other => other.into(),
    }
}

fn is_reserved_tag(tag: &str) -> bool {
    matches!(tag.to_ascii_uppercase().as_str(), "DIRECT" | "REJECT" | "BLOCK")
}

fn normalize_rule_targets(rules: &mut [Value]) {
    for rule in rules {
        if let Some(target) = rule.get("outbound").and_then(Value::as_str) {
            let normalized = normalize_policy_tag(target);
            rule["outbound"] = Value::String(normalized);
        }
        if let Some(children) = rule.get_mut("rules").and_then(Value::as_array_mut) {
            normalize_rule_targets(children);
        }
    }
}

/// Reject route and outbound references that became dangling after the
/// converter skipped unsupported nodes or empty groups.
fn validate_references(config: &Value) -> Result<(), String> {
    let mut tags = std::collections::HashSet::new();
    for (index, outbound) in config["outbounds"].as_array().into_iter().flatten().enumerate() {
        let tag = outbound
            .get("tag")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("outbounds[{index}] has no tag"))?;
        if !tags.insert(tag) {
            return Err(format!("duplicate outbound tag: {tag}"));
        }
    }
    let route = &config["route"];
    let dns_servers = config["dns"]["servers"].as_array().cloned().unwrap_or_default();
    let dns_servers_by_tag: std::collections::HashMap<&str, &Value> = dns_servers
        .iter()
        .filter_map(|server| Some((server.get("tag")?.as_str()?, server)))
        .collect();
    let dns_tags: std::collections::HashSet<&str> = dns_servers
        .iter()
        .filter_map(|server| server.get("tag").and_then(Value::as_str))
        .collect();
    if dns_tags.len() != dns_servers.len() {
        return Err("DNS servers must have unique string tags".into());
    }
    for outbound in config["outbounds"].as_array().into_iter().flatten() {
        for key in ["detour", "default"] {
            if let Some(target) = outbound.get(key).and_then(Value::as_str)
                && !tags.contains(target)
            {
                return Err(format!("outbound {key} references missing outbound {target:?}"));
            }
        }
        if let Some(members) = outbound.get("outbounds").and_then(Value::as_array) {
            let tag = outbound.get("tag").and_then(Value::as_str).unwrap_or("<group>");
            for member in members.iter().filter_map(Value::as_str) {
                if !tags.contains(member) {
                    return Err(format!("outbound group {tag:?} references missing member {member:?}"));
                }
            }
        }
    }
    if let Some(final_tag) = route.get("final").and_then(Value::as_str)
        && !tags.contains(final_tag)
    {
        return Err(format!("route.final references missing outbound {final_tag:?}"));
    }
    if let Some(resolver) = route.get("default_domain_resolver").and_then(Value::as_str)
        && !dns_tags.contains(resolver)
    {
        return Err(format!(
            "route.default_domain_resolver references missing DNS server {resolver:?}"
        ));
    }
    if let Some(resolver) = route.get("default_domain_resolver").and_then(Value::as_str)
        && dns_servers_by_tag
            .get(resolver)
            .is_some_and(|server| server.get("type").and_then(Value::as_str) == Some("fakeip"))
    {
        return Err("route.default_domain_resolver cannot reference a fake-IP DNS server".into());
    }
    for (index, server) in dns_servers.iter().enumerate() {
        if let Some(detour) = server.get("detour").and_then(Value::as_str)
            && !tags.contains(detour)
        {
            return Err(format!("dns.servers[{index}] references missing outbound {detour:?}"));
        }
        if let Some(resolver) = server.get("domain_resolver").and_then(Value::as_str) {
            if !dns_tags.contains(resolver) {
                return Err(format!(
                    "dns.servers[{index}].domain_resolver references missing DNS server {resolver:?}"
                ));
            }
            if dns_servers_by_tag
                .get(resolver)
                .is_some_and(|server| server.get("type").and_then(Value::as_str) == Some("fakeip"))
            {
                return Err(format!(
                    "dns.servers[{index}].domain_resolver cannot reference a fake-IP DNS server"
                ));
            }
        }
    }
    if let Some(rules) = config["dns"]["rules"].as_array() {
        for (index, rule) in rules.iter().enumerate() {
            if let Some(server) = rule.get("server").and_then(Value::as_str)
                && !dns_tags.contains(server)
            {
                return Err(format!("dns.rules[{index}] references missing DNS server {server:?}"));
            }
        }
    }
    if let Some(rules) = route.get("rules").and_then(Value::as_array) {
        let known: std::collections::HashSet<&str> = route["rule_set"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|set| set.get("tag").and_then(Value::as_str))
            .collect();
        for (index, rule) in rules.iter().enumerate() {
            validate_nested_route_rule(rule, &tags, &known)
                .map_err(|error| format!("route.rules[{index}]: {error}"))?;
            if let Some(target) = rule.get("outbound").and_then(Value::as_str)
                && !tags.contains(target)
            {
                return Err(format!("route.rules[{index}] references missing outbound {target:?}"));
            }
            if let Some(sets) = rule.get("rule_set").and_then(Value::as_array) {
                let known: std::collections::HashSet<&str> = route["rule_set"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|set| set.get("tag").and_then(Value::as_str))
                    .collect();
                for set in sets.iter().filter_map(Value::as_str) {
                    if !known.contains(set) {
                        return Err(format!("route.rules[{index}] references missing rule_set {set:?}"));
                    }
                }
            }
        }
    }
    if let Some(sets) = route.get("rule_set").and_then(Value::as_array) {
        let mut seen = std::collections::HashSet::new();
        for (index, set) in sets.iter().enumerate() {
            let tag = set
                .get("tag")
                .and_then(Value::as_str)
                .ok_or_else(|| format!("route.rule_set[{index}] has no tag"))?;
            if !seen.insert(tag) {
                return Err(format!("duplicate route rule_set tag: {tag}"));
            }
        }
    }
    Ok(())
}

fn validate_nested_route_rule(
    rule: &Value,
    outbounds: &std::collections::HashSet<&str>,
    rule_sets: &std::collections::HashSet<&str>,
) -> Result<(), String> {
    if let Some(outbound) = rule.get("outbound").and_then(Value::as_str)
        && !outbounds.contains(outbound)
    {
        return Err(format!("references missing outbound {outbound:?}"));
    }
    if let Some(sets) = rule.get("rule_set") {
        let names: Vec<&str> = if let Some(name) = sets.as_str() {
            vec![name]
        } else if let Some(names) = sets.as_array() {
            names
                .iter()
                .map(|name| name.as_str().ok_or("rule_set references must be strings"))
                .collect::<Result<_, _>>()?
        } else {
            return Err("rule_set references must be a string or list".into());
        };
        for name in names {
            if !rule_sets.contains(name) {
                return Err(format!("references missing rule_set {name:?}"));
            }
        }
    }
    if let Some(children) = rule.get("rules") {
        for child in children.as_array().ok_or("logical rules must be a list")? {
            validate_nested_route_rule(child, outbounds, rule_sets)?;
        }
    }
    Ok(())
}

/// Validate structural contracts that are easy to check offline without
/// invoking a core binary. Unknown native fields remain untouched.
pub fn validate_native_config(config: &Value) -> Result<(), String> {
    validate_native_config_for_version(config, crate::singbox::capabilities::SING_BOX_1_14_2.version)
}

pub fn validate_native_config_for_version(config: &Value, version: &str) -> Result<(), String> {
    let matrix = crate::singbox::capabilities::require_for_version(version)?;
    if config.get("rule_set").is_some() {
        return Err(format!(
            "sing-box rule_set must be nested under route for version {}",
            matrix.version
        ));
    }
    if !matrix.native_json_preserved {
        return Err(format!(
            "native sing-box JSON preservation is unsupported by version {}",
            matrix.version
        ));
    }
    if !config["route"].is_object() || !config["outbounds"].is_array() {
        return Err("native sing-box profile requires route object and outbounds array".into());
    }
    if let Some((index, outbound_type)) = config["outbounds"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
        .find_map(|(index, outbound)| {
            let kind = outbound.get("type").and_then(Value::as_str)?;
            matches!(kind, "wireguard" | "dns" | "shadowsocksr").then_some((index, kind))
        })
    {
        return Err(format!(
            "native outbounds[{index}] uses unsupported sing-box 1.14.2 outbound type {outbound_type:?}"
        ));
    }
    if config["route"]["rule_set"].is_array() && !matrix.route_rule_sets_nested {
        return Err(format!(
            "sing-box {} does not support nested route rule sets",
            matrix.version
        ));
    }
    if config["route"]["rules"].is_array() && !matrix.route_rule_references {
        return Err(format!(
            "sing-box {} has no reviewed route-reference support",
            matrix.version
        ));
    }
    if let Some(inbounds) = config["inbounds"].as_array() {
        for inbound in inbounds
            .iter()
            .filter(|inbound| inbound.get("type").and_then(Value::as_str) == Some("tun"))
        {
            if let Some(stack) = inbound.get("stack").and_then(Value::as_str) {
                validate_tun_stack(stack)?;
                if stack == "gvisor" && !matrix.tun_gvisor {
                    return Err(format!("sing-box {} does not support gvisor TUN stack", matrix.version));
                }
            }
        }
    }
    if let Some(servers) = config["dns"]["servers"].as_array()
        && servers
            .iter()
            .any(|server| server.get("address").is_some() || server.get("address_resolver").is_some())
    {
        return Err("native DNS uses legacy address/address_resolver fields removed in sing-box 1.14; migrate to typed server/server_port fields".into());
    }
    if config["dns"].get("fakeip").is_some() {
        return Err(
            "native DNS uses the legacy fakeip section removed in sing-box 1.14; migrate to a typed fakeip server"
                .into(),
        );
    }
    validate_references(config)
}

/// The CLI-owned inbound list: a loopback mixed listener on the configured
/// port, plus a TUN inbound when TUN mode is on. Shared by generated configs
/// and the native-subscription passthrough so both bind identical listeners.
pub fn control_plane_inbounds(mixed_port: u16, enable_tun: bool, tun: &TunSettings) -> Vec<Value> {
    let mut inbounds = vec![json!({
        "type": "mixed",
        "tag": "mixed-in",
        "listen": "127.0.0.1",
        "listen_port": mixed_port,
    })];
    if enable_tun {
        inbounds.push(json!({
            "type": "tun",
            "tag": "tun-in",
            "interface_name": TUN_INTERFACE_NAME,
            "stack": tun.stack,
            "mtu": tun.mtu,
            "auto_route": true,
        }));
    }
    inbounds
}

/// Force the CLI's own control plane onto an existing sing-box config,
/// preserving the provider's `outbounds`, `route` and `dns`.
///
/// Used for native sing-box JSON subscriptions: the provider ships a complete
/// sing-box document, and only the pieces this app owns (inbounds, clash_api,
/// log) may be replaced — otherwise the core would not answer our controller or
/// would bind a port the user did not choose.
pub fn apply_control_plane(
    config: &mut Value,
    mixed_port: u16,
    enable_tun: bool,
    tun: &TunSettings,
    clash_api: &ClashApiSettings,
) {
    config["log"] = json!({ "level": "info", "timestamp": true });
    config["inbounds"] = Value::Array(control_plane_inbounds(mixed_port, enable_tun, tun));
    if !config.get("experimental").is_some_and(Value::is_object) {
        config["experimental"] = json!({});
    }
    config["experimental"]["clash_api"] = json!({
        "external_controller": clash_api.listen.to_string(),
        "secret": clash_api.secret,
    });
    if !config.get("route").is_some_and(Value::is_object) {
        config["route"] = json!({});
    }
    config["route"]["auto_detect_interface"] = json!(true);
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;

    fn sample_input() -> ConfigInput {
        ConfigInput {
            outbounds: vec![
                json!({ "type": "shadowsocks", "tag": "node-a", "server": "a.example", "server_port": 8388 }),
                json!({ "type": "shadowsocks", "tag": "node-b", "server": "b.example", "server_port": 8388 }),
            ],
            groups: vec![
                GroupSpec {
                    name: "auto".into(),
                    kind: GroupKind::UrlTest,
                    members: vec!["node-a".into(), "node-b".into()],
                },
                GroupSpec {
                    name: "PROXY".into(),
                    kind: GroupKind::Selector,
                    members: vec!["auto".into(), "node-a".into()],
                },
            ],
            mixed_port: 7897,
            enable_tun: false,
            tun: TunSettings {
                stack: "gvisor".into(),
                mtu: 9000,
            },
            clash_api: ClashApiSettings {
                listen: "127.0.0.1:9090".parse().expect("addr"),
                secret: "s3cret".into(),
            },
            rule_sets: Vec::new(),
            route_rules: Vec::new(),
            dns: None,
        }
    }

    #[test]
    fn generates_selector_urltest_and_route_final() {
        let config = generate_config(&sample_input()).expect("config");

        assert_eq!(config["route"]["final"], "PROXY");
        assert_eq!(
            config["experimental"]["clash_api"]["external_controller"],
            "127.0.0.1:9090"
        );
        assert_eq!(config["experimental"]["clash_api"]["secret"], "s3cret");

        let outbounds = config["outbounds"].as_array().expect("outbounds array");
        let tags: Vec<&str> = outbounds.iter().filter_map(|o| o["tag"].as_str()).collect();
        assert_eq!(tags, vec!["node-a", "node-b", "auto", "PROXY", "direct", "block"]);

        let proxy = outbounds.iter().find(|o| o["tag"] == "PROXY").expect("selector");
        // Selectors gain an explicit direct fallback entry.
        assert_eq!(
            proxy["outbounds"],
            json!(["auto", "node-a", "direct"]),
            "selector should append direct"
        );
        assert_eq!(proxy["default"], "auto");

        let auto = outbounds.iter().find(|o| o["tag"] == "auto").expect("urltest");
        assert_eq!(auto["url"], URLTEST_URL);
        assert_eq!(auto["outbounds"], json!(["node-a", "node-b"]));
    }

    #[test]
    fn tun_inbound_only_when_enabled_with_deterministic_interface() {
        let mut input = sample_input();

        input.enable_tun = true;
        let config = generate_config(&input).expect("config");
        let inbounds = config["inbounds"].as_array().expect("inbounds");
        assert_eq!(inbounds.len(), 2);
        let tun = &inbounds[1];
        assert_eq!(tun["type"], "tun");
        assert_eq!(tun["interface_name"], "sb-tun0");
        assert_eq!(tun["stack"], "gvisor");
        assert_eq!(tun["auto_route"], true);

        input.enable_tun = false;
        let config = generate_config(&input).expect("config");
        let inbounds = config["inbounds"].as_array().expect("inbounds");
        assert_eq!(inbounds.len(), 1);
        assert_eq!(inbounds[0]["type"], "mixed");
    }

    #[test]
    fn empty_nodes_fall_back_to_direct() {
        let mut input = sample_input();
        input.outbounds.clear();
        input.groups.clear();

        let config = generate_config(&input).expect("config");
        assert_eq!(config["route"]["final"], "direct");

        let outbounds = config["outbounds"].as_array().expect("outbounds array");
        assert_eq!(outbounds.len(), 2);
        assert_eq!(outbounds[0]["tag"], "direct");
        assert_eq!(outbounds[1]["tag"], "block");
    }

    #[test]
    fn empty_groups_are_skipped() {
        let mut input = sample_input();
        input.groups.push(GroupSpec {
            name: "empty-group".into(),
            kind: GroupKind::Selector,
            members: vec![],
        });

        let config = generate_config(&input).expect("config");
        let tags: Vec<&str> = config["outbounds"]
            .as_array()
            .expect("outbounds")
            .iter()
            .filter_map(|o| o["tag"].as_str())
            .collect();
        assert!(!tags.contains(&"empty-group"), "empty group must be skipped: {tags:?}");
    }

    #[test]
    fn duplicate_tags_are_rejected() {
        let mut input = sample_input();
        input.outbounds.push(input.outbounds[0].clone());
        let err = generate_config(&input).expect_err("duplicate node tags");
        assert!(err.contains("duplicate"), "{err}");

        let mut input = sample_input();
        input.groups.push(GroupSpec {
            name: "node-a".into(),
            kind: GroupKind::UrlTest,
            members: vec!["node-b".into()],
        });
        let err = generate_config(&input).expect_err("group colliding with node tag");
        assert!(err.contains("node-a"), "{err}");
    }

    #[test]
    fn rule_sets_are_nested_under_route_when_present() {
        let mut input = sample_input();
        input.rule_sets = vec![json!({
            "type": "remote",
            "tag": "geoip",
            "format": "binary",
            "url": "https://example.com/geoip.srs",
        })];

        let config = generate_config(&input).expect("config");
        assert_eq!(config["route"]["rule_set"].as_array().expect("rule_set array").len(), 1);
        assert_eq!(config["route"]["rule_set"][0]["tag"], "geoip");
        assert!(config.get("rule_set").is_none(), "root rule_set is invalid");

        input.rule_sets.clear();
        let config = generate_config(&input).expect("config");
        assert!(
            config["route"].get("rule_set").is_none(),
            "empty rule_sets must be omitted"
        );
    }

    #[test]
    fn dangling_group_and_rule_set_references_are_rejected() {
        let mut input = sample_input();
        input.groups[0].members = vec!["missing-node".into()];
        assert!(generate_config(&input).unwrap_err().contains("missing-node"));

        let mut input = sample_input();
        input.route_rules = vec![json!({ "rule_set": ["missing-set"], "outbound": "PROXY" })];
        assert!(generate_config(&input).unwrap_err().contains("missing-set"));

        let mut input = sample_input();
        input.route_rules = vec![json!({ "domain": ["example.org"], "outbound": "missing-outbound" })];
        assert!(generate_config(&input).unwrap_err().contains("missing-outbound"));
    }

    #[test]
    fn nested_logical_rules_and_selector_defaults_cannot_reference_missing_targets() {
        let mut input = sample_input();
        input.route_rules =
            vec![json!({"type":"logical","mode":"and","rules":[{"rule_set":"missing"}],"outbound":"direct"})];
        assert!(generate_config(&input).unwrap_err().contains("missing rule_set"));
        input.route_rules =
            vec![json!({"type":"logical","mode":"or","rules":[{"outbound":"missing"}],"outbound":"direct"})];
        assert!(generate_config(&input).unwrap_err().contains("missing outbound"));
        let native = json!({"route":{},"outbounds":[{"type":"selector","tag":"g","default":"missing","outbounds":[]}]});
        assert!(validate_native_config(&native).unwrap_err().contains("default"));
    }

    #[test]
    fn fakeip_dns_server_cannot_be_used_as_a_host_bootstrap_resolver() {
        let config = json!({
            "outbounds": [{ "type": "direct", "tag": "direct" }],
            "route": { "default_domain_resolver": "fake" },
            "dns": { "servers": [
                { "type": "fakeip", "tag": "fake" },
                { "type": "https", "tag": "remote", "server": "resolver.example", "domain_resolver": "fake" }
            ] }
        });
        let error = validate_native_config(&config).unwrap_err();
        assert!(error.contains("route.default_domain_resolver"));

        let mut config = config;
        config["route"]["default_domain_resolver"] = json!("remote");
        let error = validate_native_config(&config).unwrap_err();
        assert!(error.contains("dns.servers[1].domain_resolver"));
    }

    #[test]
    fn policy_targets_normalize_and_reserved_tags_fail_explicitly() {
        let mut input = sample_input();
        input.groups[1].members.push("REJECT".into());
        input.route_rules = vec![json!({ "domain": ["example.org"], "outbound": "REJECT" })];
        let config = generate_config(&input).expect("policy aliases normalized");
        assert_eq!(config["route"]["rules"][0]["outbound"], "block");
        let proxy = config["outbounds"]
            .as_array()
            .unwrap()
            .iter()
            .find(|o| o["tag"] == "PROXY")
            .unwrap();
        assert!(proxy["outbounds"].as_array().unwrap().contains(&json!("block")));

        input.groups[1].name = "DIRECT".into();
        assert!(generate_config(&input).unwrap_err().contains("reserved"));
    }

    #[test]
    fn invalid_tun_stack_and_legacy_native_dns_are_rejected_with_actionable_errors() {
        assert!(validate_tun_stack("mips").unwrap_err().contains("supported stacks"));
        assert!(validate_tun_stack("gvisor").is_ok());
        let native = json!({
            "outbounds": [{ "type": "direct", "tag": "direct" }],
            "route": {},
            "dns": { "servers": [{ "tag": "old", "address": "tls://1.1.1.1" }] }
        });
        assert!(
            validate_native_config(&native)
                .unwrap_err()
                .contains("migrate to typed")
        );
        let legacy_fakeip = json!({
            "outbounds": [{ "type": "direct", "tag": "direct" }],
            "route": {},
            "dns": { "fakeip": { "enabled": true } }
        });
        assert!(
            validate_native_config(&legacy_fakeip)
                .unwrap_err()
                .contains("typed fakeip server")
        );
    }
    #[test]
    fn route_rules_are_injected_into_route_section() {
        let mut input = sample_input();
        input.route_rules = vec![json!({ "domain_suffix": ["example.com"], "outbound": "PROXY" })];

        let config = generate_config(&input).expect("config");
        assert_eq!(
            config["route"]["rules"],
            json!([{ "domain_suffix": ["example.com"], "outbound": "PROXY" }])
        );
        // route.final coexists with injected rules.
        assert_eq!(config["route"]["final"], "PROXY");

        input.route_rules.clear();
        let config = generate_config(&input).expect("config");
        assert!(
            config["route"].get("rules").is_none(),
            "empty route_rules must be omitted"
        );
    }

    #[test]
    fn dns_section_is_emitted_when_present() {
        let mut input = sample_input();
        input.dns = Some(json!({ "servers": [{ "type": "local", "tag": "dns-local" }] }));

        let config = generate_config(&input).expect("config");
        assert_eq!(config["dns"]["servers"][0]["tag"], "dns-local");

        input.dns = None;
        input.dns = None;
        let config = generate_config(&input).expect("config");
        assert!(config.get("dns").is_none(), "absent dns must be omitted");
    }

    #[test]
    fn apply_control_plane_preserves_native_singbox_document() {
        let mut native = json!({
            "outbounds": [{ "type": "vless", "tag": "custom-node" }],
            "route": { "rules": [{ "outbound": "custom-node" }] },
            "dns": { "servers": [{ "tag": "remote", "type": "https" }] },
            "inbounds": [{ "type": "mixed", "listen_port": 1234 }]
        });
        let tun = TunSettings {
            stack: "gvisor".into(),
            mtu: 9000,
        };
        let clash_api = ClashApiSettings {
            listen: "127.0.0.1:9090".parse().expect("addr"),
            secret: "my-secret".into(),
        };
        apply_control_plane(&mut native, 7897, true, &tun, &clash_api);

        // Preserved original content
        assert_eq!(native["outbounds"][0]["tag"], "custom-node");
        assert_eq!(native["route"]["rules"][0]["outbound"], "custom-node");
        assert_eq!(native["dns"]["servers"][0]["tag"], "remote");
        assert_eq!(native["route"]["auto_detect_interface"], true);

        // Injected CLI control plane
        assert_eq!(native["inbounds"][0]["listen_port"], 7897);
        assert_eq!(native["inbounds"][1]["type"], "tun");
        assert_eq!(native["experimental"]["clash_api"]["secret"], "my-secret");
    }

    #[test]
    fn native_validation_rejects_root_rule_set_and_accepts_nested_unknown_fields() {
        let mut root = json!({ "outbounds": [{ "type": "direct", "tag": "direct" }], "route": {}, "rule_set": [] });
        assert!(
            validate_native_config(&root)
                .unwrap_err()
                .contains("nested under route")
        );
        root.as_object_mut().unwrap().remove("rule_set");
        root["route"]["rule_set"] = json!([]);
        root["provider_extension"] = json!({ "nested": [1, 2, 3] });
        validate_native_config(&root).expect("supported unknown fields preserved");
        assert_eq!(root["provider_extension"]["nested"], json!([1, 2, 3]));
        let removed = json!({
            "outbounds": [{ "type": "wireguard", "tag": "wg" }],
            "route": {}
        });
        assert!(validate_native_config(&removed).unwrap_err().contains("wireguard"));
    }
}
