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
    /// Task 7.4: sing-box rule-set definitions emitted verbatim as the
    /// `route.rule_set` section; rules reference them by tag.
    pub rule_sets: Vec<Value>,
    /// Pre-converted sing-box route rule objects (task 7.5): profile rules
    /// via `routing::to_singbox_json` plus stored logical rules.
    pub route_rules: Vec<Value>,
    /// Pre-built DNS section (task 8.1, 1.12+ new format); omitted when None.
    pub dns: Option<Value>,
    /// The profile's catch-all target (`MATCH,<target>`), used as
    /// `route.final` when it resolves; otherwise the first selector.
    pub final_target: Option<String>,
    /// Mode the core starts in (`rule` / `global` / `direct`).
    pub default_mode: Option<String>,
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
    generate_config_reporting(input).map(|(config, _)| config)
}

/// [`generate_config`], plus what had to be dropped to give sing-box a
/// config it accepts (group members and rules pointing at outbounds that do
/// not exist, Clash-only built-ins). sing-box rejects a dangling reference
/// at start (`dependency[X] not found`), and `sing-box check` does not catch
/// it, so every reference is resolved here.
pub fn generate_config_reporting(input: &ConfigInput) -> Result<(Value, Vec<String>), String> {
    let mut notes = Vec::new();
    let mut tags = std::collections::HashSet::new();
    for outbound in &input.outbounds {
        let tag = outbound
            .get("tag")
            .and_then(Value::as_str)
            .ok_or_else(|| "node outbound missing tag".to_string())?;
        if !tags.insert(tag.to_string()) {
            return Err(format!("duplicate outbound tag: {tag}"));
        }
    }
    for group in &input.groups {
        if !tags.insert(group.name.clone()) {
            return Err(format!("duplicate outbound tag: {}", group.name));
        }
    }
    if tags.contains(DIRECT_TAG) {
        return Err(format!("outbound tag {DIRECT_TAG:?} is reserved"));
    }

    let groups = resolve_groups(&input.outbounds, &input.groups, &mut notes);
    let available: std::collections::HashSet<&str> = input
        .outbounds
        .iter()
        .filter_map(|outbound| outbound.get("tag").and_then(Value::as_str))
        .chain(groups.iter().map(|group| group.name.as_str()))
        .chain(std::iter::once(DIRECT_TAG))
        .collect();

    let first_selector = groups
        .iter()
        .find(|g| g.kind == GroupKind::Selector)
        .map(|g| g.name.clone())
        .unwrap_or_else(|| DIRECT_TAG.to_string());
    let final_outbound = match input.final_target.as_deref().and_then(builtin_member) {
        Some(Member::Tag(tag)) if available.contains(tag.as_str()) => tag,
        Some(Member::Tag(tag)) => {
            notes.push(format!("MATCH target {tag:?} does not exist; using {first_selector:?}"));
            first_selector.clone()
        }
        Some(Member::Reject) => {
            notes.push("MATCH,REJECT is not supported as the final route; using direct".into());
            DIRECT_TAG.to_string()
        }
        None => first_selector.clone(),
    };

    let inbounds = control_plane_inbounds(input.mixed_port, input.enable_tun, &input.tun);

    let mut outbounds: Vec<Value> = input.outbounds.clone();
    for group in &groups {
        match group.kind {
            GroupKind::Selector => {
                // Selectors always expose an explicit direct fallback.
                let mut members = group.members.clone();
                if !members.iter().any(|member| member == DIRECT_TAG) {
                    members.push(DIRECT_TAG.to_string());
                }
                outbounds.push(json!({
                    "type": "selector",
                    "tag": group.name,
                    "outbounds": members,
                    "default": group.members.first().cloned().unwrap_or_else(|| DIRECT_TAG.into()),
                }));
            }
            GroupKind::UrlTest => {
                outbounds.push(json!({
                    "type": "urltest",
                    "tag": group.name,
                    "outbounds": group.members,
                    "url": URLTEST_URL,
                    "interval": "3m",
                    "tolerance": 50,
                }));
            }
        }
    }
    outbounds.push(json!({ "type": "direct", "tag": DIRECT_TAG }));

    // Clash modes: sing-box's clash_api switches modes through rules that
    // match on `clash_mode`; without them it only knows "Rule".
    let mut route_rules = vec![
        json!({ "clash_mode": "Direct", "outbound": DIRECT_TAG }),
        json!({ "clash_mode": "Global", "outbound": final_outbound }),
    ];
    for rule in &input.route_rules {
        if let Some(rule) = resolve_rule(rule, &available, &mut notes) {
            route_rules.push(rule);
        }
    }
    // Last, so it only sees what no rule matched, like `final`: it adds
    // "Rule" to the modes the controller accepts.
    route_rules.push(json!({ "clash_mode": "Rule", "outbound": final_outbound }));

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
            "rules": route_rules,
        },
        "experimental": {
            "clash_api": clash_api_json(&input.clash_api, input.default_mode.as_deref()),
        }
    });
    // Rule-sets belong to the route section (sing-box rejects a top-level
    // `rule_set`).
    if !input.rule_sets.is_empty() {
        config["route"]["rule_set"] = Value::Array(input.rule_sets.clone());
    }
    if let Some(dns) = &input.dns {
        config["dns"] = dns.clone();
    }
    for note in &notes {
        tracing::warn!(target: "singbox", "{note}");
    }
    Ok((config, notes))
}

/// A Clash group member or rule target as sing-box sees it.
enum Member {
    Tag(String),
    /// `REJECT` / `REJECT-DROP`: not an outbound in sing-box 1.11+.
    Reject,
}

/// Clash built-ins mapped to sing-box; `None` for ones with no meaning
/// here (`PASS`, `COMPATIBLE`).
fn builtin_member(name: &str) -> Option<Member> {
    match name {
        "DIRECT" => Some(Member::Tag(DIRECT_TAG.into())),
        "REJECT" | "REJECT-DROP" => Some(Member::Reject),
        "PASS" | "COMPATIBLE" => None,
        other => Some(Member::Tag(other.into())),
    }
}

/// Groups with every member resolved to an outbound that will exist.
/// Dropping an empty group can leave another group empty, so this repeats
/// until nothing changes.
fn resolve_groups(outbounds: &[Value], groups: &[GroupSpec], notes: &mut Vec<String>) -> Vec<GroupSpec> {
    let nodes: std::collections::HashSet<&str> = outbounds
        .iter()
        .filter_map(|outbound| outbound.get("tag").and_then(Value::as_str))
        .collect();
    let mut alive: Vec<GroupSpec> = groups.to_vec();
    loop {
        let names: std::collections::HashSet<String> = alive.iter().map(|group| group.name.clone()).collect();
        let mut next = Vec::with_capacity(alive.len());
        for group in &alive {
            let mut members: Vec<String> = Vec::new();
            for member in &group.members {
                match builtin_member(member) {
                    Some(Member::Tag(tag))
                        if tag == DIRECT_TAG || nodes.contains(tag.as_str()) || names.contains(&tag) =>
                    {
                        if !members.contains(&tag) {
                            members.push(tag);
                        }
                    }
                    Some(Member::Tag(tag)) => {
                        notes.push(format!(
                            "group {:?}: member {tag:?} does not exist; dropped",
                            group.name
                        ));
                    }
                    Some(Member::Reject) | None => {
                        notes.push(format!(
                            "group {:?}: member {member:?} has no sing-box form; dropped",
                            group.name
                        ));
                    }
                }
            }
            if members.is_empty() {
                notes.push(format!("group {:?} has no usable members; dropped", group.name));
            } else {
                next.push(GroupSpec {
                    name: group.name.clone(),
                    kind: group.kind,
                    members,
                });
            }
        }
        if next.len() == alive.len() {
            // Notes from earlier passes repeat; keep each once.
            let mut seen = std::collections::HashSet::new();
            notes.retain(|note| seen.insert(note.clone()));
            return next;
        }
        alive = next;
    }
}

/// A route rule pointing at an outbound that exists: `REJECT` becomes the
/// `reject` action, `DIRECT` the direct outbound; rules whose target is
/// missing are dropped (sing-box would refuse to start).
fn resolve_rule(rule: &Value, available: &std::collections::HashSet<&str>, notes: &mut Vec<String>) -> Option<Value> {
    let Some(outbound) = rule.get("outbound").and_then(Value::as_str) else {
        return Some(rule.clone());
    };
    let mut rule = rule.clone();
    match outbound {
        "block" | "REJECT" | "REJECT-DROP" => {
            if let Some(object) = rule.as_object_mut() {
                object.remove("outbound");
                object.insert("action".into(), json!("reject"));
            }
            Some(rule)
        }
        "DIRECT" => {
            rule["outbound"] = json!(DIRECT_TAG);
            Some(rule)
        }
        tag if available.contains(tag) => Some(rule),
        tag => {
            notes.push(format!("rule {rule} targets {tag:?}, which does not exist; dropped"));
            None
        }
    }
}

fn clash_api_json(clash_api: &ClashApiSettings, default_mode: Option<&str>) -> Value {
    let mut api = json!({
        "external_controller": clash_api.listen.to_string(),
        "secret": clash_api.secret,
        // Browsers must not drive the controller: allow only its own origin
        // (an empty list means "*" to sing-box).
        "access_control_allow_origin": [format!("http://{}", clash_api.listen)],
        "access_control_allow_private_network": false,
    });
    if let Some(mode) = default_mode.and_then(mode_name) {
        api["default_mode"] = json!(mode);
    }
    api
}

/// sing-box's spelling of a Clash mode.
fn mode_name(mode: &str) -> Option<&'static str> {
    match mode.to_ascii_lowercase().as_str() {
        "rule" => Some("Rule"),
        "global" => Some("Global"),
        "direct" => Some("Direct"),
        _ => None,
    }
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
    config["experimental"]["clash_api"] = clash_api_json(clash_api, None);
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
            final_target: None,
            default_mode: None,
        }
    }

    #[test]
    fn clash_builtins_and_dangling_references_are_resolved() {
        let mut input = sample_input();
        input.groups = vec![
            GroupSpec {
                name: "PROXY".into(),
                kind: GroupKind::Selector,
                members: vec![
                    "node-a".into(),
                    "DIRECT".into(),
                    "REJECT".into(),
                    "gone-node".into(),
                    "only-missing".into(),
                ],
            },
            // Every member is missing: the group goes, and so does its
            // mention in PROXY (resolved on the next pass).
            GroupSpec {
                name: "only-missing".into(),
                kind: GroupKind::UrlTest,
                members: vec!["gone-1".into(), "PASS".into()],
            },
        ];
        input.route_rules = vec![
            json!({ "domain": ["ads.example"], "outbound": "block" }),
            json!({ "domain": ["lan.example"], "outbound": "DIRECT" }),
            json!({ "domain": ["x.example"], "outbound": "only-missing" }),
            json!({ "domain": ["y.example"], "outbound": "PROXY" }),
        ];
        input.final_target = Some("PROXY".into());
        input.default_mode = Some("global".into());

        let (config, notes) = generate_config_reporting(&input).expect("config");
        let outbounds = config["outbounds"].as_array().unwrap();
        let tags: Vec<&str> = outbounds.iter().filter_map(|o| o["tag"].as_str()).collect();
        assert_eq!(tags, ["node-a", "node-b", "PROXY", "direct"]);
        let proxy = outbounds.iter().find(|o| o["tag"] == "PROXY").unwrap();
        assert_eq!(
            proxy["outbounds"],
            json!(["node-a", "direct"]),
            "DIRECT once, lowercase"
        );

        let rules = config["route"]["rules"].as_array().unwrap();
        assert_eq!(rules[0], json!({ "clash_mode": "Direct", "outbound": "direct" }));
        assert_eq!(rules[1], json!({ "clash_mode": "Global", "outbound": "PROXY" }));
        assert_eq!(rules[2], json!({ "domain": ["ads.example"], "action": "reject" }));
        assert_eq!(rules[3]["outbound"], "direct");
        assert_eq!(rules[4]["outbound"], "PROXY", "the rule to a dropped group is gone");
        assert_eq!(rules[5], json!({ "clash_mode": "Rule", "outbound": "PROXY" }));
        assert_eq!(rules.len(), 6);
        assert_eq!(config["route"]["final"], "PROXY");
        assert_eq!(config["experimental"]["clash_api"]["default_mode"], "Global");
        assert_eq!(
            config["experimental"]["clash_api"]["access_control_allow_origin"],
            json!(["http://127.0.0.1:9090"])
        );

        for expected in ["gone-node", "only-missing", "REJECT", "x.example"] {
            assert!(
                notes.iter().any(|note| note.contains(expected)),
                "{expected}: {notes:?}"
            );
        }
    }

    #[test]
    fn an_unknown_match_target_falls_back_to_the_first_selector() {
        let mut input = sample_input();
        input.final_target = Some("Nope".into());
        let (config, notes) = generate_config_reporting(&input).expect("config");
        assert_eq!(config["route"]["final"], "PROXY");
        assert!(notes.iter().any(|note| note.contains("Nope")));
        input.final_target = Some("DIRECT".into());
        assert_eq!(generate_config(&input).unwrap()["route"]["final"], "direct");
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
        assert_eq!(tags, vec!["node-a", "node-b", "auto", "PROXY", "direct"]);

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
        assert_eq!(outbounds.len(), 1);
        assert_eq!(outbounds[0]["tag"], "direct");
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
    fn rule_sets_are_emitted_when_present() {
        let mut input = sample_input();
        input.rule_sets = vec![json!({
            "type": "remote",
            "tag": "geoip",
            "format": "binary",
            "url": "https://example.com/geoip.srs",
        })];

        let config = generate_config(&input).expect("config");
        assert!(
            config.get("rule_set").is_none(),
            "sing-box rejects a top-level rule_set"
        );
        assert_eq!(config["route"]["rule_set"].as_array().expect("rule_set array").len(), 1);
        assert_eq!(config["route"]["rule_set"][0]["tag"], "geoip");

        input.rule_sets.clear();
        let config = generate_config(&input).expect("config");
        assert!(
            config["route"].get("rule_set").is_none(),
            "empty rule_sets must be omitted"
        );
    }
    #[test]
    fn route_rules_are_injected_into_route_section() {
        let mut input = sample_input();
        input.route_rules = vec![json!({ "domain_suffix": ["example.com"], "outbound": "PROXY" })];

        let config = generate_config(&input).expect("config");
        // After the two clash-mode rules that make mode switching work.
        assert_eq!(
            config["route"]["rules"],
            json!([
                { "clash_mode": "Direct", "outbound": "direct" },
                { "clash_mode": "Global", "outbound": "PROXY" },
                { "domain_suffix": ["example.com"], "outbound": "PROXY" },
                { "clash_mode": "Rule", "outbound": "PROXY" },
            ])
        );
        // route.final coexists with injected rules.
        assert_eq!(config["route"]["final"], "PROXY");

        // Without profile rules only the clash-mode rules remain.
        input.route_rules.clear();
        let config = generate_config(&input).expect("config");
        assert_eq!(config["route"]["rules"].as_array().map(Vec::len), Some(3));
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
}
