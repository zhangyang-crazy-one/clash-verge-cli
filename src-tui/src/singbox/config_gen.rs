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
//! Intentionally minimal: DNS/rule-set/route rules are layered on by later
//! work; unknown fields survive because generation starts from scratch each
//! time the profile changes.

use serde_json::{json, Value};
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

    // Empty groups are rejected by sing-box at startup — skip them instead.
    let groups: Vec<&GroupSpec> = input.groups.iter().filter(|g| !g.members.is_empty()).collect();
    let final_outbound = groups
        .iter()
        .find(|g| g.kind == GroupKind::Selector)
        .map(|g| g.name.as_str())
        .unwrap_or(DIRECT_TAG);

    let mut inbounds = vec![json!({
        "type": "mixed",
        "tag": "mixed-in",
        "listen": "127.0.0.1",
        "listen_port": input.mixed_port,
    })];
    if input.enable_tun {
        inbounds.push(json!({
            "type": "tun",
            "tag": "tun-in",
            "interface_name": TUN_INTERFACE_NAME,
            "stack": input.tun.stack,
            "mtu": input.tun.mtu,
            "auto_route": true,
        }));
    }

    let mut outbounds: Vec<Value> = input.outbounds.clone();
    for group in &groups {
        match group.kind {
            GroupKind::Selector => {
                // Selectors always expose an explicit direct fallback.
                let mut members = group.members.clone();
                members.push(DIRECT_TAG.to_string());
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

    Ok(json!({
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
    }))
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
            tun: TunSettings { stack: "gvisor".into(), mtu: 9000 },
            clash_api: ClashApiSettings {
                listen: "127.0.0.1:9090".parse().expect("addr"),
                secret: "s3cret".into(),
            },
        }
    }

    #[test]
    fn generates_selector_urltest_and_route_final() {
        let config = generate_config(&sample_input()).expect("config");

        assert_eq!(config["route"]["final"], "PROXY");
        assert_eq!(config["experimental"]["clash_api"]["external_controller"], "127.0.0.1:9090");
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
}
