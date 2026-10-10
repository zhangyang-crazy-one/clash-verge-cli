//! Clash YAML node → sing-box outbound conversion (task 5.1).
//!
//! Strategy per design D3: full protocol coverage with FIELD-LEVEL
//! degradation — every mapped protocol carries a whitelist of clash
//! fields that translate cleanly; anything else is dropped and reported.
//! An entirely unmappable node is an error, also reported upstream so
//! the UI can list skips.

use serde_json::{Value, json};
use serde_yaml_ng::Value as Yaml;

/// One successfully converted node plus its degradation report.
#[derive(Debug, Clone)]
pub struct ConvertedNode {
    pub outbound: Value,
    /// clash field paths that had no sing-box equivalent and were dropped,
    /// each with a human-readable reason.
    pub dropped: Vec<String>,
}

/// Convert a single clash proxy entry into a sing-box outbound object.
///
/// Errors mean the node type cannot be represented at all (unknown type,
/// missing mandatory fields) — callers skip the node and keep converting.
pub fn convert_node(proxy: &Yaml) -> Result<ConvertedNode, String> {
    let Yaml::Mapping(map) = proxy else {
        return Err("proxy entry is not a mapping".into());
    };
    let get = |key: &str| map.get(Yaml::String(key.into()));
    let as_str = |key: &str| get(key).and_then(Yaml::as_str).map(str::to_string);

    let name = as_str("name").ok_or_else(|| "node missing name".to_string())?;
    let ptype = as_str("type").ok_or_else(|| format!("node '{name}' missing type"))?;

    let mut dropped: Vec<String> = Vec::new();
    for key in map.keys() {
        if let Yaml::String(k) = key
            && !RESERVED_FIELDS.contains(&k.as_str())
            && !PROTOCOL_MAPS
                .iter()
                .find(|(kind, _)| **kind == ptype)
                .is_some_and(|(_, (_, fields))| fields.iter().any(|(field, _)| *field == k))
        {
            dropped.push(format!("{name}.{k}"));
        }
    }
    const CRITICAL_UNSUPPORTED_FIELDS: &[&str] =
        &["packet-addr", "ip-version", "smux", "obfs", "plugin", "plugin-opts"];
    if let Some(field) = CRITICAL_UNSUPPORTED_FIELDS.iter().find(|field| get(field).is_some()) {
        return Err(format!(
            "node {name:?} uses critical field {field:?} with no supported sing-box 1.14.2 mapping"
        ));
    }

    let server = as_str("server").ok_or_else(|| format!("node '{name}' missing server"))?;
    let port = get("port")
        .and_then(Yaml::as_u64)
        .ok_or_else(|| format!("node '{name}' missing port"))?;

    // Build via a per-type field map: (clash key -> sing-box key).
    let Some((sb_type, fields)) = PROTOCOL_MAPS
        .iter()
        .find(|(ct, _)| **ct == ptype)
        .map(|(_, spec)| (spec.0, spec.1))
    else {
        return Err(format!("unsupported proxy type '{ptype}' for node '{name}'"));
    };

    let mut outbound = json!({
        "type": sb_type,
        "tag": name,
        "server": server,
        "server_port": port,
    });
    for (ckey, skey) in fields.iter() {
        if let Some(value) = yaml_to_json(get(ckey)) {
            outbound[*skey] = value;
        }
    }
    apply_udp(&ptype, &name, &mut outbound, map, &mut dropped)?;
    apply_tls(&ptype, &mut outbound, map)?;
    if let Some(fingerprint) = as_str("client-fingerprint") {
        if !crate::singbox::capabilities::SING_BOX_1_14_2.client_fingerprint {
            return Err(format!(
                "node {name:?} uses client-fingerprint unsupported by sing-box 1.14.2"
            ));
        }
        outbound["tls"]["utls"] = json!({ "enabled": true, "fingerprint": fingerprint });
    }
    apply_transport(&ptype, &mut outbound, map)?;

    Ok(ConvertedNode { outbound, dropped })
}

/// Fields read by the converter itself (never reported as dropped).
const RESERVED_FIELDS: &[&str] = &[
    "name",
    "type",
    "server",
    "port",
    "tls",
    "servername",
    "sni",
    "skip-cert-verify",
    "network",
    "ws-opts",
    "grpc-opts",
    "reality-opts",
    "client-fingerprint",
    "udp",
];

/// (clash type, (sing-box type, [(clash field, sing-box field)]))
/// Only fields whose names/values translate directly are listed here.
type ProtocolField = (&'static str, &'static str);
type ProtocolBody = (&'static str, &'static [ProtocolField]);
type ProtocolEntry = (&'static str, ProtocolBody);

const PROTOCOL_MAPS: &[ProtocolEntry] = &[
    ("ss", ("shadowsocks", &[("cipher", "method"), ("password", "password")])),
    (
        "vmess",
        (
            "vmess",
            &[("uuid", "uuid"), ("cipher", "security"), ("alterId", "alter_id")],
        ),
    ),
    ("vless", ("vless", &[("uuid", "uuid"), ("flow", "flow")])),
    ("trojan", ("trojan", &[("password", "password")])),
    (
        "hysteria",
        (
            "hysteria",
            &[("auth-str", "auth_str"), ("up", "up_mbps"), ("down", "down_mbps")],
        ),
    ),
    (
        "hysteria2",
        (
            "hysteria2",
            &[("password", "password"), ("up", "up_mbps"), ("down", "down_mbps")],
        ),
    ),
    (
        "tuic",
        (
            "tuic",
            &[
                ("uuid", "uuid"),
                ("password", "password"),
                ("congestion-controller", "congestion_controller"),
            ],
        ),
    ),
    (
        "naive",
        ("naive", &[("username", "username"), ("password", "password")]),
    ),
    ("anytls", ("anytls", &[("password", "password")])),
    ("http", ("http", &[("username", "username"), ("password", "password")])),
    (
        "socks5",
        ("socks", &[("username", "username"), ("password", "password")]),
    ),
];

fn yaml_to_json(value: Option<&Yaml>) -> Option<Value> {
    match value? {
        Yaml::String(v) => Some(json!(v)),
        Yaml::Number(n) => Some(json!(n.as_f64().unwrap_or_default())),
        Yaml::Bool(b) => Some(json!(b)),
        _ => None,
    }
}

/// TLS handling: trojan/Hysteria-family are TLS-native; VLESS and VMess
/// need `tls: true` to emit the block. reality-opts maps onto uTLS/reality.
fn apply_tls(ptype: &str, outbound: &mut Value, map: &serde_yaml_ng::Mapping) -> Result<(), String> {
    let get = |key: &str| map.get(Yaml::String(key.into()));
    let native_tls = matches!(ptype, "trojan" | "hysteria" | "hysteria2" | "tuic" | "naive" | "anytls");
    if get("tls").is_some() && get("tls").and_then(Yaml::as_bool).is_none() {
        return Err("tls must be a boolean; refusing to guess security behavior".into());
    }
    if get("skip-cert-verify").is_some() && get("skip-cert-verify").and_then(Yaml::as_bool).is_none() {
        return Err("skip-cert-verify must be a boolean; refusing to guess TLS verification behavior".into());
    }
    for field in ["servername", "sni", "client-fingerprint"] {
        if get(field).is_some() && get(field).and_then(Yaml::as_str).is_none() {
            return Err(format!("{field} must be a string; refusing to drop TLS settings"));
        }
    }
    let explicit_tls = get("tls").and_then(Yaml::as_bool).unwrap_or(false);
    if native_tls && get("tls").and_then(Yaml::as_bool) == Some(false) {
        return Err(format!(
            "{ptype} node requires TLS; explicit tls: false cannot be preserved"
        ));
    }
    if !native_tls && !explicit_tls {
        if get("reality-opts").is_some()
            || get("client-fingerprint").is_some()
            || get("servername").is_some()
            || get("sni").is_some()
            || get("skip-cert-verify").and_then(Yaml::as_bool) == Some(true)
        {
            return Err("TLS parameters require tls: true; refusing to drop security settings".into());
        }
        return Ok(());
    }

    let mut tls = json!({ "enabled": true });
    if let Some(sni) = get("servername").or_else(|| get("sni")).and_then(Yaml::as_str) {
        tls["server_name"] = json!(sni);
    }
    if get("skip-cert-verify").and_then(Yaml::as_bool).unwrap_or(false) {
        tls["insecure"] = json!(true);
    }
    if let Some(Yaml::Mapping(reality)) = get("reality-opts") {
        let public_key = reality.get(Yaml::String("public-key".into())).and_then(Yaml::as_str);
        let short_id = reality.get(Yaml::String("short-id".into())).and_then(Yaml::as_str);
        if let Some(pk) = public_key {
            tls["utls"] = json!({ "enabled": true, "fingerprint": "chrome" });
            tls["reality"] = json!({ "enabled": true, "public_key": pk, "short_id": short_id.unwrap_or("") });
        } else {
            return Err("reality-opts requires public-key; refusing to drop Reality security settings".into());
        }
    } else if get("reality-opts").is_some() {
        return Err("reality-opts must be a mapping".into());
    }
    outbound["tls"] = tls;
    Ok(())
}

/// Clash's `udp` flag versus sing-box's per-outbound UDP relay.
///
/// A supplementary flag never costs the user a node: `udp: true` is either
/// already the outbound's default (flag dropped with a note) or a capability
/// the outbound does not have (flag dropped, capability loss reported, node
/// kept). `udp: false` is the unsafe direction — silently relaying UDP the
/// user switched off would leak traffic onto a transport they rejected — so it
/// is expressed as `network: "tcp"` where sing-box supports that and refused
/// outright where it does not. See
/// [`crate::singbox::capabilities::udp_relay`] for the verified matrix.
fn apply_udp(
    ptype: &str,
    name: &str,
    outbound: &mut Value,
    map: &serde_yaml_ng::Mapping,
    dropped: &mut Vec<String>,
) -> Result<(), String> {
    use crate::singbox::capabilities::{UdpRelay, udp_relay};

    let Some(value) = map.get(Yaml::String("udp".into())) else {
        return Ok(());
    };
    let enabled = value
        .as_bool()
        .ok_or_else(|| "udp must be a boolean; refusing to guess transport behavior".to_string())?;

    match udp_relay(ptype) {
        UdpRelay::NetworkToggle => {
            if enabled {
                dropped.push(format!(
                    "{name}.udp: {ptype} relays UDP natively in sing-box 1.14.2; explicit udp: true dropped"
                ));
            } else {
                outbound["network"] = json!("tcp");
            }
        }
        UdpRelay::UdpOverTcp => {
            if enabled {
                dropped.push(format!(
                    "{name}.udp: {ptype} relays UDP over its TCP stream (UDP-over-TCP) in sing-box 1.14.2; explicit udp: true dropped"
                ));
            } else {
                return Err(format!(
                    "node {name:?} sets udp: false but the sing-box {ptype} outbound has no tcp-only mode; refusing to silently relay UDP"
                ));
            }
        }
        UdpRelay::NoRelay | UdpRelay::Unverified => {
            if enabled {
                let reason = if udp_relay(ptype) == UdpRelay::NoRelay {
                    format!("sing-box 1.14.2 {ptype} outbound has no UDP relay")
                } else {
                    format!("sing-box 1.14.2 {ptype} UDP relay is unverified on the pinned build")
                };
                dropped.push(format!(
                    "{name}.udp: {reason}; udp: true dropped, UDP traffic through this node will fail"
                ));
            } else {
                return Err(format!(
                    "node {name:?} sets udp: false but sing-box {ptype} cannot be restricted to TCP; refusing to silently relay UDP"
                ));
            }
        }
    }
    Ok(())
}

fn apply_transport(ptype: &str, outbound: &mut Value, map: &serde_yaml_ng::Mapping) -> Result<(), String> {
    use crate::singbox::capabilities::{UdpRelay, udp_relay};
    let get = |key: &str| map.get(Yaml::String(key.into()));
    let Some(network) = get("network").and_then(Yaml::as_str) else {
        return Ok(());
    };
    // Only outbounds that accept `network: ["tcp","udp"]` can carry Clash's
    // own network selector; the rest fall through to ws/grpc handling.
    let network_toggle = udp_relay(ptype) == UdpRelay::NetworkToggle;
    if network_toggle && matches!(network, "udp" | "tcp") {
        if let Some(udp) = get("udp").and_then(Yaml::as_bool)
            && (network == "udp") != udp
        {
            return Err(format!(
                "node {ptype:?} has conflicting network: {network} and udp: {udp}"
            ));
        }
        outbound["network"] = json!(network);
        return Ok(());
    }
    if network_toggle && matches!(network, "ws" | "grpc") && get("udp").is_some() {
        return Err(format!(
            "node type {ptype:?} cannot combine explicit udp selection with {network} transport"
        ));
    }
    match network {
        "ws" => {
            let mut transport = json!({ "type": "ws" });
            if let Some(Yaml::Mapping(opts)) = get("ws-opts") {
                if let Some(path) = opts.get(Yaml::String("path".into())).and_then(Yaml::as_str) {
                    transport["path"] = json!(path);
                }
                if let Some(Yaml::Mapping(headers)) = opts.get(Yaml::String("headers".into())) {
                    let mut map = serde_json::Map::new();
                    for (k, v) in headers {
                        if let (Yaml::String(k), Some(v)) = (k, yaml_to_json(Some(v))) {
                            map.insert(k.clone(), v);
                        }
                    }
                    transport["headers"] = Value::Object(map);
                }
            }
            outbound["transport"] = transport;
        }
        "grpc" => {
            let mut transport = json!({ "type": "grpc" });
            if let Some(Yaml::Mapping(opts)) = get("grpc-opts")
                && let Some(service) = opts
                    .get(Yaml::String("grpc-service-name".into()))
                    .and_then(Yaml::as_str)
            {
                transport["service_name"] = json!(service);
            }
            outbound["transport"] = transport;
        }
        other => {
            return Err(format!(
                "unsupported transport {other:?}: refusing to silently lose security/network semantics"
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;

    fn parse(yaml: &str) -> Yaml {
        serde_yaml_ng::from_str(yaml).expect("yaml")
    }

    #[test]
    fn maps_verified_udp_modes_for_shadowsocks_and_hysteria2() {
        let node = parse(
            r#"
name: "ss-node"
type: ss
server: 1.2.3.4
port: 8388
cipher: aes-256-gcm
password: "pw"
udp: true
"#,
        );
        let converted = convert_node(&node).expect("udp true is mapped");
        assert!(
            converted.outbound.get("network").is_none(),
            "udp:true keeps sing-box's native default"
        );

        let tcp = parse(
            "name: ss-tcp\ntype: ss\nserver: 1.2.3.4\nport: 8388\ncipher: aes-256-gcm\npassword: pw\nudp: false\n",
        );
        assert_eq!(convert_node(&tcp).unwrap().outbound["network"], "tcp");

        let h2 = parse("name: h2\ntype: hysteria2\nserver: h.example\nport: 443\npassword: pw\nudp: true\n");
        assert!(convert_node(&h2).unwrap().outbound.get("network").is_none());
    }

    #[test]
    fn anytls_with_udp_true_is_kept_and_the_flag_is_dropped_with_a_note() {
        // The shape of the user's real subscription nodes.
        let node = parse(
            r#"
name: "🇭🇰 01"
type: anytls
server: anytls.example.com
port: 443
password: hunter2
sni: anytls.example.com
skip-cert-verify: true
client-fingerprint: random
udp: true
tfo: false
"#,
        );
        let converted = convert_node(&node).expect("anytls must not be dropped for udp: true");
        assert_eq!(converted.outbound["type"], "anytls");
        assert_eq!(converted.outbound["tls"]["server_name"], "anytls.example.com");
        assert_eq!(converted.outbound["tls"]["insecure"], true);
        assert_eq!(converted.outbound["tls"]["utls"]["fingerprint"], "random");
        assert!(
            converted.outbound.get("network").is_none(),
            "anytls has no network field in sing-box 1.14.2"
        );
        let note = converted
            .dropped
            .iter()
            .find(|line| line.contains(".udp:"))
            .expect("udp degradation is reported");
        assert!(note.contains("UDP-over-TCP"), "{note}");
    }

    #[test]
    fn udp_true_keeps_nodes_whose_sing_box_outbound_has_no_udp_relay() {
        let http = parse("name: h\ntype: http\nserver: p.example\nport: 8080\nudp: true\n");
        let converted = convert_node(&http).expect("http is kept");
        assert_eq!(converted.outbound["type"], "http");
        let note = converted
            .dropped
            .iter()
            .find(|line| line.contains(".udp:"))
            .expect("capability loss is reported");
        assert!(note.contains("no UDP relay"), "{note}");
        assert!(note.contains("will fail"), "{note}");

        // Unverified types degrade the same way instead of vanishing.
        let naive = parse("name: nv\ntype: naive\nserver: n.example\nport: 443\nudp: true\n");
        let naive = convert_node(&naive).expect("naive is kept");
        assert!(
            naive.dropped.iter().any(|line| line.contains("unverified")),
            "{:?}",
            naive.dropped
        );
    }

    #[test]
    fn udp_false_maps_to_tcp_only_where_sing_box_offers_the_network_field() {
        for (yaml, sb_type) in [
            (
                "name: v\ntype: vless\nserver: h\nport: 443\nuuid: u\ntls: true\nudp: false\n",
                "vless",
            ),
            (
                "name: t\ntype: trojan\nserver: h\nport: 443\npassword: p\nudp: false\n",
                "trojan",
            ),
            (
                "name: s\ntype: socks5\nserver: 1.2.3.4\nport: 1080\nudp: false\n",
                "socks",
            ),
            (
                "name: q\ntype: tuic\nserver: h\nport: 443\nuuid: u\npassword: p\nudp: false\n",
                "tuic",
            ),
        ] {
            let converted = convert_node(&parse(yaml)).unwrap_or_else(|e| panic!("{yaml}: {e}"));
            assert_eq!(converted.outbound["type"], sb_type);
            assert_eq!(converted.outbound["network"], "tcp", "{yaml}");
        }
    }

    #[test]
    fn udp_false_stays_fail_closed_where_sing_box_cannot_disable_udp() {
        for yaml in [
            "name: at\ntype: anytls\nserver: h\nport: 443\npassword: p\nudp: false\n",
            "name: hp\ntype: http\nserver: h\nport: 8080\nudp: false\n",
            "name: nv\ntype: naive\nserver: h\nport: 443\nudp: false\n",
        ] {
            let err = convert_node(&parse(yaml)).expect_err("must not silently relay UDP");
            assert!(err.contains("refusing to silently relay UDP"), "{err}");
        }
    }

    #[test]
    fn preserves_supported_client_fingerprint_in_tls() {
        let node = parse(
            "name: vless\ntype: vless\nserver: a.example\nport: 443\nuuid: 00000000-0000-0000-0000-000000000000\ntls: true\nclient-fingerprint: firefox\n",
        );
        let converted = convert_node(&node).expect("converted");
        assert_eq!(converted.outbound["tls"]["utls"]["fingerprint"], "firefox");
    }

    #[test]
    fn converts_vmess_with_tls_and_ws() {
        let node = parse(
            r#"
name: vm-node
type: vmess
server: example.com
port: 443
uuid: 12345678-1234-1234-1234-123456789012
alterId: 0
cipher: auto
tls: true
servername: example.com
network: ws
ws-opts:
  path: /path
"#,
        );
        let converted = convert_node(&node).expect("convert");
        assert_eq!(converted.outbound["type"], "vmess");
        assert_eq!(converted.outbound["security"], "auto");
        assert_eq!(converted.outbound["tls"]["enabled"], true);
        assert_eq!(converted.outbound["tls"]["server_name"], "example.com");
        assert_eq!(converted.outbound["transport"]["type"], "ws");
        assert_eq!(converted.outbound["transport"]["path"], "/path");
    }

    #[test]
    fn trojan_is_tls_native_without_explicit_tls_flag() {
        let node = parse(
            r#"
name: tj
type: trojan
server: tj.example.com
port: 443
password: hunter2
"#,
        );
        let converted = convert_node(&node).expect("convert");
        assert_eq!(converted.outbound["tls"]["enabled"], true);
    }

    #[test]
    fn anytls_outbound_gets_required_tls_block() {
        let node = parse("name: at\ntype: anytls\nserver: at.example\nport: 443\npassword: pw\n");
        let converted = convert_node(&node).unwrap();
        assert_eq!(converted.outbound["tls"]["enabled"], true);
    }

    #[test]
    fn vless_plaintext_is_preserved_and_malformed_tls_identity_is_rejected() {
        let yaml = "name: v\ntype: vless\nserver: example.com\nport: 443\nuuid: id\ntls: false\n";
        assert!(convert_node(&parse(yaml)).unwrap().outbound.get("tls").is_none());
        let malformed = format!("{yaml}client-fingerprint: 7\n");
        assert!(
            convert_node(&parse(&malformed))
                .unwrap_err()
                .contains("must be a string")
        );
    }

    #[test]
    fn vless_reality_maps_to_utls() {
        let node = parse(
            r#"
name: vr
type: vless
server: r.example.com
port: 443
uuid: 12345678-1234-1234-1234-123456789012
tls: true
servername: r.example.com
reality-opts:
  public-key: pbk
  short-id: sid
"#,
        );
        let converted = convert_node(&node).expect("convert");
        assert_eq!(converted.outbound["tls"]["reality"]["enabled"], true);
        assert_eq!(converted.outbound["tls"]["reality"]["public_key"], "pbk");
        assert_eq!(converted.outbound["tls"]["utls"]["fingerprint"], "chrome");
    }

    #[test]
    fn unknown_type_is_an_error_not_a_panic() {
        let node = parse(
            r#"
name: weird
type: mieru
server: x
port: 1
"#,
        );
        let err = convert_node(&node).expect_err("must error");
        assert!(err.contains("mieru"), "{err}");
    }

    #[test]
    fn critical_fields_and_unknown_transports_are_rejected_not_dropped() {
        let udp = parse("name: n\ntype: anytls\nserver: host\nport: 443\npassword: p\nudp: false\n");
        assert!(
            convert_node(&udp)
                .unwrap_err()
                .contains("refusing to silently relay UDP")
        );
        let ws = parse("name: n\ntype: vless\nserver: host\nport: 443\nuuid: id\ntls: true\nnetwork: h2\n");
        assert!(convert_node(&ws).unwrap_err().contains("unsupported transport \"h2\""));
        let reality = parse(
            "name: n\ntype: vless\nserver: host\nport: 443\nuuid: id\ntls: true\nreality-opts: { short-id: abc }\n",
        );
        assert!(convert_node(&reality).unwrap_err().contains("requires public-key"));
    }

    #[test]
    fn socks5_maps_to_socks() {
        let node = parse(
            r#"
name: s5
type: socks5
server: 127.0.0.1
port: 1080
username: u
password: p
"#,
        );
        let converted = convert_node(&node).expect("convert");
        assert_eq!(converted.outbound["type"], "socks");
        assert_eq!(converted.outbound["username"], "u");
    }
}

/// Profile-level conversion result (task 5.2).
#[derive(Debug, Clone, Default)]
pub struct ProfileConversion {
    pub outbounds: Vec<Value>,
    pub groups: Vec<crate::singbox::GroupSpec>,
    /// Node names that could not be converted at all.
    pub skipped: Vec<String>,
    /// Per-node degradation lines ("node.field").
    pub degraded: Vec<String>,
    /// Informational notes about conversions that lost nothing:
    /// approximated group semantics, pruned members of a group whose
    /// target no longer exists, dropped modifiers (#52). These do NOT
    /// block a core switch — they are reported by `status`/the profile
    /// commands so CLI and TUI behave the same way.
    pub notes: Vec<String>,
}

impl ProfileConversion {
    /// Every tag a route rule may target: converted nodes, group tags and
    /// the built-in policy outbounds. Used to drop rules pointing at a
    /// group the conversion could not produce instead of generating a
    /// dangling `outbound` reference (#52).
    pub fn outbound_tags(&self) -> std::collections::HashSet<String> {
        let mut tags: std::collections::HashSet<String> = self
            .outbounds
            .iter()
            .filter_map(|outbound| outbound.get("tag").and_then(Value::as_str))
            .map(str::to_owned)
            .collect();
        tags.extend(self.groups.iter().map(|group| group.name.clone()));
        for builtin in ["direct", "block", "DIRECT", "REJECT"] {
            tags.insert(builtin.to_string());
        }
        tags
    }
}

/// Convert an entire clash config document (proxies + proxy-groups).
///
/// Nodes that fail conversion are skipped and reported; groups map
/// select→Selector / url-test→UrlTest, other group types are skipped.
pub fn convert_profile(config_yaml: &str) -> Result<ProfileConversion, String> {
    let doc: Yaml = serde_yaml_ng::from_str(config_yaml).map_err(|e| format!("invalid YAML: {e}"))?;
    let Yaml::Mapping(map) = &doc else {
        return Err("config root is not a mapping".into());
    };

    let mut result = ProfileConversion::default();

    if let Some(Yaml::Sequence(proxies)) = map.get(Yaml::String("proxies".into())) {
        for proxy in proxies {
            match convert_node(proxy) {
                Ok(converted) => {
                    result.outbounds.push(converted.outbound);
                    for d in converted.dropped {
                        result.degraded.push(d);
                    }
                }
                Err(reason) => {
                    let name = proxy
                        .get(Yaml::String("name".into()))
                        .and_then(Yaml::as_str)
                        .unwrap_or("<unnamed>");
                    result.skipped.push(format!("{name}: {reason}"));
                }
            }
        }
    }

    if let Some(Yaml::Sequence(groups)) = map.get(Yaml::String("proxy-groups".into())) {
        let node_tags: std::collections::HashSet<String> = result
            .outbounds
            .iter()
            .filter_map(|outbound| outbound.get("tag").and_then(Value::as_str))
            .map(str::to_owned)
            .collect();

        // Every group NAME declared in the document, converted or not: a
        // member pointing at one that produced no outbound is reported as
        // such instead of the vaguer "not convertible".
        let declared: std::collections::HashSet<String> = groups
            .iter()
            .filter_map(|group| group.get(Yaml::String("name".into())).and_then(Yaml::as_str))
            .map(str::to_owned)
            .collect();

        // Pass 1: classify the declared groups. A member referencing a
        // group that is itself skipped (relay, or emptied by pruning) must
        // not survive as a dangling outbound reference, so nothing is
        // pruned against DECLARED names — only against the tags this pass
        // will really emit (#P1-3).
        let mut pending: Vec<PendingGroup> = Vec::new();
        for group in groups {
            let Yaml::Mapping(g) = group else { continue };
            let name = g
                .get(Yaml::String("name".into()))
                .and_then(Yaml::as_str)
                .unwrap_or_default()
                .to_string();
            let gtype = g
                .get(Yaml::String("type".into()))
                .and_then(Yaml::as_str)
                .unwrap_or_default();
            let kind = match gtype {
                "select" => crate::singbox::GroupKind::Selector,
                "url-test" => crate::singbox::GroupKind::UrlTest,
                // #52: `fallback` and `load-balance` are health-check
                // groups; sing-box's `urltest` is the closest equivalent
                // (it keeps picking the fastest reachable member). The
                // difference — fallback never leaves a working node,
                // load-balance spreads traffic — is recorded as a note,
                // not a reason to drop the group (the CLI used to drop it
                // silently, the TUI refused to switch at all).
                "fallback" | "load-balance" => {
                    result.notes.push(format!(
                        "{name}: group type '{gtype}' converted to urltest (health-check semantics approximated)"
                    ));
                    crate::singbox::GroupKind::UrlTest
                }
                // `relay` chains nodes through each other; sing-box
                // expresses that with per-outbound `detour`, which cannot
                // be derived from a proxy-group member list. Refused
                // loudly rather than silently replaced.
                other => {
                    result.skipped.push(format!("{name}: group type '{other}' unsupported"));
                    continue;
                }
            };
            pending.push(PendingGroup {
                name,
                kind,
                raw_members: g
                    .get(Yaml::String("proxies".into()))
                    .and_then(Yaml::as_sequence)
                    .map(|seq| seq.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
                    .unwrap_or_default(),
            });
        }

        // Pass 2: decide which groups survive, then prune every group's
        // members against the EMITTED tags only. The two steps are
        // mutually dependent (a group survives only if it keeps a member, a
        // member survives only if its target is emitted), so the emitted
        // set is iterated to a fixed point — at most one round per group,
        // and it terminates because each round can only drop groups.
        let allowed = |emitted: &std::collections::HashSet<String>| -> std::collections::HashSet<String> {
            let mut allowed = node_tags.clone();
            allowed.extend(emitted.iter().cloned());
            allowed
        };
        let mut emitted: std::collections::HashSet<String> = pending
            .iter()
            .filter(|group| !prune_members(group, &allowed(&Default::default())).is_empty())
            .map(|group| group.name.clone())
            .collect();
        loop {
            let next: std::collections::HashSet<String> = pending
                .iter()
                .filter(|group| !prune_members(group, &allowed(&emitted)).is_empty())
                .map(|group| group.name.clone())
                .collect();
            if next == emitted {
                break;
            }
            emitted = next;
        }

        // Pass 3: emit the survivors and report every prune exactly once.
        let allowed = allowed(&emitted);
        for group in &pending {
            let name = &group.name;
            let members = prune_members(group, &allowed);
            if members.is_empty() {
                result
                    .skipped
                    .push(format!("{name}: group has no member left after conversion"));
                continue;
            }
            report_member_prunes(group, &members, &mut result, &declared);
            result.groups.push(crate::singbox::GroupSpec {
                name: name.clone(),
                kind: group.kind,
                members,
            });
        }
    }

    Ok(result)
}

/// A declared clash group before its members are pruned.
struct PendingGroup {
    name: String,
    kind: crate::singbox::GroupKind,
    raw_members: Vec<String>,
}

/// Policy outbound names clash allows as group members; they always exist
/// in the generated sing-box config.
fn is_builtin_member(member: &str) -> bool {
    matches!(member.to_ascii_uppercase().as_str(), "DIRECT" | "REJECT" | "BLOCK")
}

/// The members a group keeps once every non-emitted target is pruned.
///
/// Emptiness is decided on the RAW list: `{select, proxies: [DIRECT]}`
/// used to lose its `DIRECT` before the check and the group was dropped,
/// taking every rule that targeted it with it (#P1-4). `DIRECT` is only
/// de-duplicated when at least one non-`DIRECT` member survives, because
/// the generator appends `direct` to every selector anyway.
fn prune_members(group: &PendingGroup, allowed: &std::collections::HashSet<String>) -> Vec<String> {
    let selector = group.kind == crate::singbox::GroupKind::Selector;
    let mut kept: Vec<String> = Vec::with_capacity(group.raw_members.len());
    for member in &group.raw_members {
        if !allowed.contains(member) && !is_builtin_member(member) {
            continue;
        }
        if kept.contains(member) {
            continue;
        }
        kept.push(member.clone());
    }
    if selector && kept.iter().any(|member| !member.eq_ignore_ascii_case("direct")) {
        kept.retain(|member| !member.eq_ignore_ascii_case("direct"));
    }
    kept
}

/// One degradation line per pruned member: an unknown node, a duplicate,
/// a redundant `DIRECT`, or a reference to a group this pass dropped.
fn report_member_prunes(
    group: &PendingGroup,
    members: &[String],
    result: &mut ProfileConversion,
    declared: &std::collections::HashSet<String>,
) {
    let deduped_direct = group.kind == crate::singbox::GroupKind::Selector
        && members.iter().any(|member| !member.eq_ignore_ascii_case("direct"));
    let mut seen: std::collections::HashSet<&String> = Default::default();
    for member in &group.raw_members {
        if !seen.insert(member) {
            result
                .notes
                .push(format!("{}: dropped duplicate member '{member}'", group.name));
            continue;
        }
        if deduped_direct && member.eq_ignore_ascii_case("direct") {
            result.notes.push(format!(
                "{}: dropped redundant DIRECT member (selectors always expose direct)",
                group.name
            ));
            continue;
        }
        if members.contains(member) {
            continue;
        }
        let reason = if declared.contains(member.as_str()) {
            format!("group '{member}' produced no outbound")
        } else {
            "not convertible to sing-box".to_string()
        };
        result
            .notes
            .push(format!("{}: dropped member '{member}' ({reason})", group.name));
    }
}

// ---------- rule-sets derived from a clash profile (#52) ----------

/// Base URLs of the official sing-box geo rule-set repositories.
const GEOIP_RULE_SET_BASE: &str = "https://raw.githubusercontent.com/SagerNet/sing-geoip/rule-set";
const GEOSITE_RULE_SET_BASE: &str = "https://raw.githubusercontent.com/SagerNet/sing-geosite/rule-set";

/// Geo rule-set names published in `SagerNet/sing-geoip@rule-set`
/// (regenerate from the GitHub tree API: `.srs` file names minus the
/// `geoip-` prefix). Synthesizing a tag that is not in this list produces a
/// 404 at rule-set initialization, which FATALs the whole core start —
/// so a clash value outside the list never becomes a tag (#P0-2).
///
/// Stored as one identifier per array element, NEVER as a whitespace-wrapped
/// blob: a wrapped blob silently splits an identifier into two bogus tokens
/// (`apple-music@cn` -> `apple-` + `music@cn`), which drops a legal
/// `GEOSITE,apple-music@cn` from the conversion (#P2). `rustfmt` only ever
/// breaks between elements, so no identifier can be damaged by wrapping, and
/// `published_geo_name_lists_are_sorted_unique_and_match_the_official_set`
/// pins the list against the upstream file names.
const PUBLISHED_GEOIP_NAMES: &[&str] = &[
    "ad", "ae", "af", "ag", "ai", "al", "am", "ao", "aq", "ar", "as", "at", "au", "aw", "ax", "az", "ba", "bb", "bd",
    "be", "bf", "bg", "bh", "bi", "bj", "bl", "bm", "bn", "bo", "bq", "br", "bs", "bt", "bw", "by", "bz", "ca", "cd",
    "cf", "cg", "ch", "ci", "ck", "cl", "cm", "cn", "co", "cr", "cu", "cv", "cw", "cy", "cz", "de", "dj", "dk", "dm",
    "do", "dz", "ec", "ee", "eg", "er", "es", "et", "fi", "fj", "fk", "fm", "fo", "fr", "ga", "gb", "gd", "ge", "gf",
    "gg", "gh", "gi", "gl", "gm", "gn", "gp", "gq", "gr", "gt", "gu", "gw", "gy", "hk", "hn", "hr", "ht", "hu", "id",
    "ie", "il", "im", "in", "io", "iq", "ir", "is", "it", "je", "jm", "jo", "jp", "ke", "kg", "kh", "ki", "km", "kn",
    "kp", "kr", "kw", "ky", "kz", "la", "lb", "lc", "li", "lk", "lr", "ls", "lt", "lu", "lv", "ly", "ma", "mc", "md",
    "me", "mf", "mg", "mh", "mk", "ml", "mm", "mn", "mo", "mp", "mq", "mr", "ms", "mt", "mu", "mv", "mw", "mx", "my",
    "mz", "na", "nc", "ne", "nf", "ng", "ni", "nl", "no", "np", "nr", "nu", "nz", "om", "pa", "pe", "pf", "pg", "ph",
    "pk", "pl", "pm", "pr", "ps", "pt", "pw", "py", "qa", "re", "ro", "rs", "ru", "rw", "sa", "sb", "sc", "sd", "se",
    "sg", "si", "sk", "sl", "sm", "sn", "so", "sr", "ss", "st", "sv", "sx", "sy", "sz", "tc", "td", "tg", "th", "tj",
    "tk", "tl", "tm", "tn", "to", "tr", "tt", "tv", "tw", "tz", "ua", "ug", "us", "uy", "uz", "va", "vc", "ve", "vg",
    "vi", "vn", "vu", "wf", "ws", "ye", "yt", "za", "zm", "zw",
];

/// Geo rule-set names published in `SagerNet/sing-geosite@rule-set`
/// (regenerate the same way: `.srs` file names minus `geosite-`).
///
/// One identifier per array element so that no line wrap can damage a name;
/// see `PUBLISHED_GEOIP_NAMES`.
const PUBLISHED_GEOSITE_NAMES: &[&str] = &[
    "0x0",
    "115",
    "1337x",
    "17zuoye",
    "18comic",
    "2ch",
    "2gis",
    "2kgames",
    "36kr",
    "36kr@ads",
    "4399",
    "4chan",
    "4pda",
    "4plebs",
    "51job",
    "54647",
    "58tongcheng",
    "5ch",
    "6park",
    "7tv",
    "800best",
    "8btc",
    "928plus",
    "9news",
    "9to5",
    "aamgame",
    "abc",
    "abema",
    "accuweather",
    "acer",
    "acer@cn",
    "acfun",
    "acfun@ads",
    "actalis",
    "activision",
    "activision-blizzard",
    "adblock",
    "adblockplus",
    "addthis",
    "addtoany",
    "adguard",
    "adidas",
    "adidas@cn",
    "adjust",
    "adjust@ads",
    "adobe",
    "adobe-activation",
    "adobe@ads",
    "adobe@cn",
    "aerogard",
    "aerogard@cn",
    "afdian",
    "afp",
    "agilebits",
    "agora",
    "aiqicha",
    "airbnb",
    "airchina",
    "airchina@!cn",
    "airwick",
    "airwick@cn",
    "aisiku",
    "aixcoder",
    "akamai",
    "akamai@cn",
    "akiko",
    "alibaba",
    "alibaba@!cn",
    "alibaba@ads",
    "alibaba@cn",
    "alibabacloud",
    "alibabacloud@!cn",
    "alibabacloud@cn",
    "aligames",
    "aliyun",
    "aliyun-drive",
    "aliyun@!cn",
    "aliyun@ads",
    "aljazeera",
    "alphabet",
    "alphabet@!cn",
    "alphabet@ads",
    "alphabet@cn",
    "amap",
    "amap@ads",
    "amazon",
    "amazon@ads",
    "amazon@cn",
    "amazontrust",
    "amc",
    "amd",
    "amd@cn",
    "amp",
    "amp@cn",
    "amplitude",
    "amplitude@ads",
    "anaconda",
    "anandtech",
    "android",
    "anexia",
    "anime",
    "anker",
    "anker@!cn",
    "annas-archive",
    "anon-v",
    "anthropic",
    "ap",
    "apa",
    "aparat",
    "apifox",
    "apipost",
    "apkcombo",
    "apkmirror",
    "apkpure",
    "apple",
    "apple-dev",
    "apple-dev@cn",
    "apple-intelligence",
    "apple-music",
    "apple-music@cn",
    "apple-pki",
    "apple-pki@cn",
    "apple-podcasts",
    "apple-podcasts@cn",
    "apple-tvplus",
    "apple-update",
    "apple@ads",
    "apple@cn",
    "appledaily",
    "applysquare",
    "aptoide",
    "archive",
    "archiveofourown",
    "archivetoday",
    "archlinux",
    "arphic",
    "artstation",
    "asahi",
    "askdiandian",
    "asobo",
    "asproex",
    "asus",
    "asus@cn",
    "atlassian",
    "att",
    "att@cn",
    "attwatchtv",
    "autodesk",
    "autoru",
    "auxgroup",
    "auxgroup@!cn",
    "avaxhome",
    "aviasales",
    "avito",
    "avito@ads",
    "avmoo",
    "awempire",
    "aws",
    "aws-cn",
    "aws@cn",
    "azure",
    "azure@cn",
    "b3log",
    "bahamut",
    "baidu",
    "baidu@ads",
    "baishancloud",
    "baltamatica",
    "bamtech",
    "bandcamp",
    "bandwagonhost",
    "bangumi",
    "barrons",
    "bbc",
    "bdsmhub",
    "beats",
    "beats@cn",
    "beget",
    "beisen",
    "bestbuy",
    "bestbuy@cn",
    "bestchange",
    "bestore",
    "bestv",
    "betboom",
    "bethesda",
    "betterexplained",
    "bilibili",
    "bilibili-cdn",
    "bilibili-cdn@!cn",
    "bilibili-game",
    "bilibili2",
    "bilibili@!cn",
    "binance",
    "bing",
    "bing@ads",
    "bing@cn",
    "bitauto",
    "bitflyer",
    "bitly",
    "bitsquare",
    "bitwarden",
    "bjyouth",
    "blender",
    "blizzard",
    "blogspot",
    "bloomberg",
    "bluearchive",
    "bluearchive@cn",
    "bluepoch",
    "bluepoch-games",
    "bluesky",
    "blurams",
    "bmw",
    "bmw@cn",
    "boboporn",
    "boc",
    "boc@!cn",
    "bohemia",
    "bongacams",
    "booking",
    "booking@cn",
    "books",
    "boomerang",
    "bootcdn",
    "bootstrap",
    "borneoschematics",
    "boslife",
    "boxun",
    "boylove",
    "braveux",
    "brazzers",
    "bridgestone",
    "bridgestone@cn",
    "brightcove",
    "brilliant",
    "broadcom",
    "broadcom@cn",
    "btdig",
    "bttzyw",
    "bushiroad",
    "buymeacoffee",
    "buypass",
    "bybit",
    "bytedance",
    "bytedance-ai-!cn",
    "bytedance@!cn",
    "bytedance@ads",
    "c-span",
    "cabletv",
    "cainiao",
    "cainiao@ads",
    "calgoncarbon",
    "calgoncarbon@cn",
    "cambridge",
    "camwhores",
    "canon",
    "canon@cn",
    "canonical",
    "canva",
    "capitalonline",
    "carrotquest",
    "carrotquest@ads",
    "cas",
    "casimages",
    "catchplay",
    "category-acg",
    "category-ads",
    "category-ads-all",
    "category-ads-all@ads",
    "category-ads-ir",
    "category-ai-!cn",
    "category-ai-!cn@ads",
    "category-ai-!cn@telemetry",
    "category-ai-chat-!cn",
    "category-ai-chat-!cn@ads",
    "category-ai-chat-!cn@telemetry",
    "category-ai-cn",
    "category-ai-ru",
    "category-android-app-download",
    "category-anticensorship",
    "category-antivirus",
    "category-antivirus@cn",
    "category-automobile-cn",
    "category-bank-cn",
    "category-bank-ir",
    "category-bank-jp",
    "category-bank-mm",
    "category-bank-ru",
    "category-bank-ru@ads",
    "category-betting-ru",
    "category-blog-cn",
    "category-bourse-ir",
    "category-browser-!cn",
    "category-cas",
    "category-cas@cn",
    "category-cdn-!cn",
    "category-cdn-cn",
    "category-collaborate-cn",
    "category-communication",
    "category-communication@ads",
    "category-companies",
    "category-companies@!cn",
    "category-companies@ads",
    "category-companies@cn",
    "category-companies@telemetry",
    "category-consent-management",
    "category-container",
    "category-cryptocurrency",
    "category-cryptocurrency@cn",
    "category-ddns",
    "category-dev",
    "category-dev-cn",
    "category-dev-cn@ads",
    "category-dev@ads",
    "category-dev@cn",
    "category-dev@telemetry",
    "category-documents-cn",
    "category-doh",
    "category-ecommerce",
    "category-ecommerce-ru",
    "category-ecommerce-ru@ads",
    "category-ecommerce@ads",
    "category-ecommerce@cn",
    "category-education-cn",
    "category-education-cn@ads",
    "category-education-ir",
    "category-education-ru",
    "category-electronic-cn",
    "category-emby",
    "category-enhance-gaming",
    "category-enhance-gaming@cn",
    "category-enterprise-query-platform-cn",
    "category-entertainment",
    "category-entertainment-cn",
    "category-entertainment-cn@ads",
    "category-entertainment-ru",
    "category-entertainment@!cn",
    "category-entertainment@ads",
    "category-entertainment@cn",
    "category-finance",
    "category-finance@ads",
    "category-finance@cn",
    "category-food-cn",
    "category-food-cn@ads",
    "category-forums",
    "category-forums-ir",
    "category-forums-ru",
    "category-forums@ads",
    "category-game-accelerator-cn",
    "category-game-platforms-download",
    "category-game-platforms-download@cn",
    "category-games",
    "category-games-!cn",
    "category-games-!cn@ads",
    "category-games-cn",
    "category-games-cn@ads",
    "category-games@ads",
    "category-games@cn",
    "category-gov-ir",
    "category-gov-ru",
    "category-hospital-cn",
    "category-httpdns-cn",
    "category-httpdns-cn@ads",
    "category-insurance-ir",
    "category-ip-geo-detect",
    "category-ip-geo-detect@!cn",
    "category-ip-geo-detect@cn",
    "category-ipfs",
    "category-ir",
    "category-logistics-cn",
    "category-logistics-cn@ads",
    "category-media",
    "category-media-cn",
    "category-media-cn@ads",
    "category-media-ir",
    "category-media-ru",
    "category-media-ru-blocked",
    "category-media@cn",
    "category-medicine-ru",
    "category-mobile-repair",
    "category-mooc-cn",
    "category-netdisk-!cn",
    "category-netdisk-!cn@ads",
    "category-netdisk-cn",
    "category-network-security-cn",
    "category-news-ir",
    "category-novel",
    "category-ntp",
    "category-ntp-cn",
    "category-ntp-jp",
    "category-ntp@cn",
    "category-number-verification-cn",
    "category-olympiad-in-informatics",
    "category-orgs",
    "category-outsource-cn",
    "category-password-management",
    "category-payment-ir",
    "category-porn",
    "category-porn@ads",
    "category-proxy-tunnels",
    "category-pt",
    "category-pt@!cn",
    "category-public-tracker",
    "category-remote-control",
    "category-remote-control@cn",
    "category-retail-ru",
    "category-retail-ru@ads",
    "category-ru",
    "category-ru@ads",
    "category-ru@cn",
    "category-scholar-!cn",
    "category-scholar-cn",
    "category-scholar-hk",
    "category-scholar-ir",
    "category-scholar-uk",
    "category-securities-cn",
    "category-shopping-ir",
    "category-social-media-!cn",
    "category-social-media-!cn@ads",
    "category-social-media-cn",
    "category-social-media-cn@ads",
    "category-social-media-ir",
    "category-speedtest",
    "category-speedtest@!cn",
    "category-speedtest@ads",
    "category-speedtest@cn",
    "category-stun",
    "category-tech-ir",
    "category-tech-media",
    "category-tech-media-ru",
    "category-tech-media@cn",
    "category-tm",
    "category-travel-ir",
    "category-travel-ru",
    "category-urlshortner",
    "category-voip",
    "category-vpnservices",
    "category-web-archive",
    "category-wiki-cn",
    "cavporn",
    "cbs",
    "ccb",
    "ccb@!cn",
    "cctv",
    "cctv@ads",
    "cdek",
    "cdn77",
    "ceno",
    "cerebras",
    "cern",
    "certinomis",
    "certum",
    "changyou",
    "chaoxing",
    "chatango",
    "chatwhores",
    "cheetahmobile",
    "chegg",
    "chesscom",
    "chinabroadnet",
    "chinamobile",
    "chinamobile@!cn",
    "chinanews",
    "chinapost",
    "chinapower",
    "chinaso",
    "chinatelecom",
    "chinatelecom@!cn",
    "chinatower",
    "chinaunicom",
    "chinaunicom@!cn",
    "chinaz",
    "cian",
    "cisco",
    "cisco@cn",
    "citic",
    "citic@!cn",
    "citizenlab",
    "cityu-hk",
    "ciweimao",
    "ck101",
    "clarivate",
    "clearasil",
    "clearasil@cn",
    "clearbit",
    "clearbit@ads",
    "clips4sale",
    "cloudcone",
    "cloudconvert",
    "cloudflare",
    "cloudflare-cn",
    "cloudflare-ipfs",
    "cloudflare@cn",
    "cloudinary",
    "cloudns",
    "clubhouse",
    "cmb",
    "cmb@!cn",
    "cn",
    "cn@ads",
    "cnb",
    "cnbc",
    "cnbeta",
    "cnblogs",
    "cnet",
    "cnki",
    "cnn",
    "code",
    "codeberg",
    "codecademy",
    "codeforces",
    "coding",
    "coinone",
    "collabora",
    "colorfulclouds",
    "comfy",
    "comfy-ui-launcher",
    "comodo",
    "comssone",
    "connectivity-check",
    "connectivity-check@cn",
    "contentful",
    "coolapk",
    "coomer",
    "copymanga",
    "corel",
    "costco",
    "coupang",
    "coursera",
    "cowlevel",
    "cowtransfer",
    "craigslist",
    "creativecommons",
    "csdn",
    "csis",
    "ctexcel",
    "ctexcel@!cn",
    "ctrip",
    "ctrip@!cn",
    "ctyun",
    "cuhk",
    "cuinc",
    "curseforge",
    "cursor",
    "cuttly",
    "cybertrust",
    "cygames",
    "cylink",
    "dailymail",
    "dailymotion",
    "dandanplay",
    "dandanzan",
    "dangdang",
    "dart",
    "dazn",
    "dcard",
    "ddmaicai",
    "debian",
    "decryptipastore",
    "dedao",
    "deepin",
    "deepin@!cn",
    "deepseek",
    "deezer",
    "dell",
    "dell@cn",
    "demonoid",
    "deppon",
    "deribit",
    "dettol",
    "dettol@cn",
    "deviantart",
    "dewu",
    "dewu@!cn",
    "didi",
    "didi@!cn",
    "digicert",
    "digicert@cn",
    "digitalocean",
    "digitalplayground",
    "dingdatech",
    "dingtalk",
    "discord",
    "discourse",
    "discoveryplus",
    "discuz",
    "disney",
    "disney@ads",
    "disney@cn",
    "disqus",
    "divar",
    "dji",
    "dlercloud",
    "dlsite",
    "dmit",
    "dmm",
    "dmm-porn",
    "dmm@ads",
    "dnspod",
    "docker",
    "doi",
    "dola",
    "dola@!cn",
    "dongchedi",
    "dongjiao",
    "douban",
    "doubao",
    "douyin",
    "douyu",
    "dowjones",
    "dribbble",
    "dropbox",
    "drweb",
    "dslreports",
    "duckduckgo",
    "duitang",
    "duolingo",
    "duolingo@ads",
    "duolingo@cn",
    "duowan",
    "durex",
    "durex@cn",
    "duyaoss",
    "dw",
    "dwion",
    "dyna",
    "dynu",
    "dzen",
    "dzen@ads",
    "ea",
    "eastmoney",
    "eastmoney@!cn",
    "easylist",
    "ebay",
    "ebay@cn",
    "ebuyer",
    "economist",
    "eduhk",
    "edx",
    "egghead",
    "ehentai",
    "electron",
    "eleme",
    "eleme@ads",
    "elevenlabs",
    "elsevier",
    "embark",
    "embedly",
    "embl",
    "emojipedia",
    "eneba",
    "enfa",
    "entermediadb",
    "entrust",
    "entrust@cn",
    "envato",
    "envybox",
    "epicbrowser",
    "epicgames",
    "epicgames@cn",
    "epochmediagroup",
    "erolabs",
    "escapefromtarkov",
    "eset",
    "eset@cn",
    "espn",
    "espressif",
    "espressif@!cn",
    "esri",
    "ethereum",
    "everbright",
    "evernote",
    "f-droid",
    "facebook",
    "facebook-dev",
    "facebook@ads",
    "faceit",
    "falungong",
    "familymart",
    "familymart@cn",
    "fandom",
    "fans66",
    "fansta",
    "farfetch",
    "farfetch@cn",
    "faronics",
    "fastlane",
    "fastly",
    "faststone",
    "fcbox",
    "fedora",
    "feedly",
    "feishu",
    "fengxing",
    "fflogs",
    "fflogs@cn",
    "fibank",
    "ficbook",
    "figma",
    "filimo",
    "finish",
    "finish@cn",
    "firebase",
    "firebase@cn",
    "firefox",
    "flatpak",
    "flibusta",
    "flickr",
    "flowus",
    "flowwow",
    "flutter",
    "flyio",
    "focuschina",
    "fonbet",
    "fontawesome",
    "fontexplorer",
    "fonts",
    "fontshop",
    "fontsinuse",
    "forbes",
    "formula1",
    "forza",
    "fox",
    "fqnovel",
    "fqnovel@ads",
    "framer",
    "freebuff",
    "freecodecamp",
    "freenode",
    "ft",
    "ftv",
    "funpay",
    "futu",
    "fzdm",
    "gaijin",
    "gamersky",
    "gamersky@ads",
    "gamesplanet",
    "gandi",
    "ganji",
    "gannett",
    "garena",
    "gateio",
    "geetest",
    "gemfury",
    "genotek-ru",
    "geolocation-!cn",
    "geolocation-!cn@ads",
    "geolocation-!cn@telemetry",
    "geolocation-cn",
    "geolocation-cn@ads",
    "gettyimages",
    "getui",
    "gfycat",
    "ggsel",
    "giffgaff",
    "gigabyte",
    "gigabyte@cn",
    "gimy",
    "gismeteo",
    "gitbook",
    "gitee",
    "github",
    "github-copilot",
    "github-copilot@telemetry",
    "github1s",
    "github@telemetry",
    "gitlab",
    "gitv",
    "globalsign",
    "globalsign@cn",
    "globalvoices",
    "globo",
    "glyphs",
    "gmo-internet",
    "gmo-internet@cn",
    "godaddy",
    "gofundme",
    "gog",
    "gog@ads",
    "gog@cn",
    "golang",
    "goodreads",
    "google",
    "google-deepmind",
    "google-gemini",
    "google-play",
    "google-play@cn",
    "google-registry",
    "google-registry-tld",
    "google-scholar",
    "google-trust-services",
    "google-trust-services@cn",
    "google@!cn",
    "google@ads",
    "google@cn",
    "googlefcm",
    "googlefcm@!cn",
    "goproxy",
    "gracg",
    "grapheneos",
    "gravatar",
    "greatfire",
    "gree",
    "groq",
    "group-ib",
    "growingio",
    "gucci",
    "gucci@cn",
    "guo",
    "guokr",
    "habr",
    "haier",
    "hainanairlines",
    "haitang",
    "hamivideo",
    "hanyi",
    "harpercollins",
    "hashicorp",
    "haskell",
    "haveibeenpwned",
    "hbo",
    "hcaptcha",
    "hdrezka",
    "headhunter",
    "hentaichen",
    "hentaivn",
    "herogame",
    "heroku",
    "hetzner",
    "hetzner@ads",
    "heyzo",
    "hikvision",
    "hinet",
    "hinet-eca",
    "hisense",
    "hitun",
    "hkbn",
    "hkbu",
    "hkedcity",
    "hketgroup",
    "hketgroup@cn",
    "hkgolden",
    "hkt",
    "hku",
    "hkust",
    "hm",
    "hm@cn",
    "homebrew",
    "homedepot",
    "hongkongpost",
    "honor",
    "hooligapps",
    "hotstar",
    "hoyoverse",
    "hoyoverse@ads",
    "hp",
    "hp@cn",
    "hpe",
    "hsbc",
    "hsbc-cn",
    "huanghuagang",
    "huawei",
    "huawei-dev",
    "huawei@!cn",
    "huawei@ads",
    "huaweicloud",
    "huaweicloud@!cn",
    "hubblephone",
    "huffpost",
    "hugecore",
    "huggingface",
    "hujiang",
    "hulu",
    "humblebundle",
    "hunantv",
    "hunantv@ads",
    "huobi",
    "hupu",
    "hupun",
    "hurricaneelectric",
    "huya",
    "ibkr",
    "ibm",
    "icable",
    "icbc",
    "icbc@!cn",
    "icloud",
    "icloud@cn",
    "icloudprivaterelay",
    "ideco-ru",
    "identrust",
    "idg",
    "ieee",
    "ifanr",
    "ifast",
    "ifast@cn",
    "iflytek",
    "ihuman",
    "iina",
    "ikea",
    "ikea@cn",
    "illgames",
    "illusion",
    "illusion-nonofficial",
    "imagebam",
    "imagecurl",
    "imageshack",
    "imagetwist",
    "imdb",
    "imgbb",
    "imgix",
    "imgur",
    "imperialcollege",
    "infowars",
    "infrapedia",
    "inoreader",
    "inshot",
    "insider",
    "instagram",
    "instagram@ads",
    "intel",
    "intel-dev",
    "intel@cn",
    "intercom",
    "internet-archive",
    "intsig",
    "intuit",
    "ipip",
    "ipip@!cn",
    "iqiyi",
    "iqiyi@!cn",
    "iqiyi@ads",
    "isgd",
    "ishumei",
    "itchio",
    "itiger",
    "itunes",
    "itunes@cn",
    "ixbt",
    "ixsystems",
    "iyf",
    "jable",
    "japonx",
    "java",
    "javbus",
    "javcc",
    "javdb",
    "javwide",
    "jd",
    "jd@!cn",
    "jd@ads",
    "jetbrains",
    "jetbrains-ai",
    "jetbrains@cn",
    "jfrog",
    "jianshu",
    "jibencaozuo",
    "jiemian",
    "jiguang",
    "jinshuju",
    "jiyukobo",
    "jkf",
    "jlc",
    "johren",
    "jquery",
    "jsdelivr",
    "jtexpress",
    "juejin",
    "jushuitan",
    "justav",
    "justmysocks",
    "jutongbao",
    "jwplayer",
    "kaggle",
    "kakao",
    "kanzhongguo",
    "kaspersky",
    "kaspersky@cn",
    "kechuang",
    "keep",
    "kemono",
    "kernel",
    "keybase",
    "khanacademy",
    "kick",
    "kindle",
    "kindle4rss",
    "kindle@cn",
    "kingkonglive",
    "kingsoft",
    "kinopoisk",
    "kinopub",
    "kkbox",
    "kktv",
    "kodi",
    "kodik",
    "konachan",
    "konami",
    "kontur",
    "kontur@ads",
    "koolearn",
    "kraken",
    "ku6",
    "kuaidi100",
    "kuaikan",
    "kuaishou",
    "kuaishou@ads",
    "kuaiyikeji",
    "kubakuba",
    "kubernetes",
    "kucoin",
    "kugou",
    "kugou@ads",
    "kurogames",
    "kurogames@!cn",
    "kurogames@ads",
    "kuwo",
    "kyodonews",
    "lagou",
    "landian",
    "lantern",
    "lanzou",
    "laracasts",
    "lark",
    "lark-global",
    "lastfm",
    "lastpass",
    "launchpad",
    "lavteam",
    "le",
    "le@ads",
    "lenovo",
    "lethalhardcore",
    "letsencrypt",
    "lg",
    "lianjia",
    "liberapay",
    "libgen",
    "liepin",
    "lifewire",
    "ligastavok",
    "lighter",
    "lihkg",
    "likee",
    "limelight",
    "linakesi",
    "line",
    "linguee",
    "linkedin",
    "linkedin@cn",
    "linotype",
    "linux",
    "linuxdo",
    "lisiku",
    "litv",
    "livejournal",
    "liveperson",
    "lizhi",
    "lkcoffee",
    "localbitcoins",
    "localizejs",
    "logitech",
    "londonreal",
    "longbridge",
    "louisvuitton",
    "louisvuitton@cn",
    "lowiro",
    "ltn",
    "lumion",
    "lysol",
    "lysol@cn",
    "madshi",
    "mafengwo",
    "magnit",
    "mailcom",
    "mailru",
    "mailru-group",
    "mailru-group@ads",
    "mailru@ads",
    "mainichi",
    "manhuagui",
    "manhuaren",
    "manorama",
    "manoto",
    "manus",
    "maocloud",
    "mapbox",
    "mapbox@cn",
    "marvel",
    "mastercard",
    "mastercard@cn",
    "masterclass",
    "matrix",
    "matters",
    "mcdonalds",
    "mcdonalds@cn",
    "mdn",
    "meadjohnson",
    "meadjohnson@cn",
    "mediachinesegroup",
    "medium",
    "meduza",
    "mega",
    "megafon",
    "meipian",
    "meitu",
    "meituan",
    "meizu",
    "messenger",
    "meta",
    "meta@ads",
    "metabrainz",
    "metacritic",
    "metart",
    "miaomiaozhe",
    "microsoft",
    "microsoft-dev",
    "microsoft-dev@cn",
    "microsoft-pki",
    "microsoft@ads",
    "microsoft@cn",
    "microsoft@telemetry",
    "midea",
    "mihoyo",
    "mihoyo-cn",
    "mihoyo-cn@ads",
    "mihoyo@ads",
    "mihoyo@cn",
    "mikrotik",
    "mindbox",
    "mindbox@ads",
    "mindgeek",
    "mindgeek-porn",
    "mini",
    "miniso",
    "miniso@cn",
    "miraheze",
    "missav",
    "misskey",
    "misskey-universe",
    "mit",
    "mixi",
    "mobile01",
    "mocha",
    "modrinth",
    "mogujie",
    "mojang",
    "moji",
    "moji@ads",
    "momo",
    "mongodb",
    "monotype",
    "moonvy",
    "morisawa",
    "mortein",
    "mortein@cn",
    "mosmetro",
    "motorola",
    "movefree",
    "movefree@cn",
    "moxing",
    "mozilla",
    "mozilla@ads",
    "mozilla@telemetry",
    "msi",
    "msn",
    "msn@ads",
    "msn@cn",
    "mts-ru",
    "mts-ru@ads",
    "mubi",
    "mucinex",
    "mudvod",
    "muji",
    "muji@cn",
    "musixmatch",
    "mvideo",
    "mxroute",
    "mydirtyhobby",
    "myfonts",
    "myoffice-ru",
    "myradio",
    "mytvsuper",
    "mzed",
    "n26",
    "n3ro",
    "narwal",
    "nationalgeographic",
    "naver",
    "nbcuniversal",
    "neowin",
    "netcraze",
    "netcup",
    "netease",
    "netease@!cn",
    "netease@ads",
    "netflav",
    "netflix",
    "netlify",
    "neuralink",
    "newegg",
    "newgrounds",
    "newscorp",
    "newsmax",
    "nexitally",
    "nexo",
    "nexon",
    "nexusmods",
    "nga",
    "nginx",
    "ngrok",
    "nhk",
    "nic-ru",
    "nicegram",
    "nicegram@ads",
    "niconico",
    "nike",
    "nike@cn",
    "nikkan-gendai",
    "nikke",
    "nikkei",
    "nintendo",
    "nintendo@cn",
    "nist",
    "nixos",
    "nodejs",
    "nodeseek",
    "noip",
    "nordstrom",
    "nordvpn",
    "notion",
    "now",
    "nowcoder",
    "npmjs",
    "nudevista",
    "nurofen",
    "nurofen@cn",
    "nutaku",
    "nvidia",
    "nvidia@cn",
    "nyaa",
    "nypost",
    "nytimes",
    "oan",
    "oculus",
    "ogury",
    "ogury@ads",
    "ok",
    "okaapps",
    "okaapps@cn",
    "okjike",
    "okko",
    "okx",
    "okx@cn",
    "olevod",
    "onedrive",
    "onekey",
    "oneplus",
    "oneplus@!cn",
    "ookla-speedtest",
    "ookla-speedtest@ads",
    "op",
    "openai",
    "openai@ads",
    "opencollective",
    "openjsfoundation",
    "openjsfoundation@cn",
    "openrec",
    "opensourceinsights",
    "openspeedtest",
    "openstreetmap",
    "openweather",
    "openwrt",
    "openx",
    "openx@ads",
    "oppo",
    "oppo@!cn",
    "oracle",
    "oreilly",
    "oreilly@cn",
    "organicmaps",
    "origin",
    "oschina",
    "oskelly",
    "osu",
    "otpbank",
    "oup",
    "overclockers-ru",
    "ozon",
    "ozon@ads",
    "pagecdn",
    "panasonic",
    "panasonic@cn",
    "pandanet",
    "paofuyun",
    "paskoocheh",
    "pastebin",
    "patreon",
    "pawchive",
    "paypal",
    "paypal@cn",
    "pbs",
    "pccw",
    "pchome",
    "pearson",
    "pearson@cn",
    "peppy",
    "perl",
    "perplexity",
    "petrochina",
    "pgyer",
    "phoenix",
    "picacg",
    "picacg@ads",
    "picsee",
    "pikpak",
    "pikpak@ads",
    "pinduoduo",
    "pingan",
    "pingan@!cn",
    "pingcap",
    "pinggy",
    "pingpe",
    "pingsx",
    "pinkcore",
    "pinterest",
    "piratebay",
    "pixhost",
    "pixiv",
    "pixiv@ads",
    "pixnet",
    "playboy",
    "playcover",
    "playstation",
    "plex",
    "plutotv",
    "pocketcasts",
    "poe",
    "polocloud",
    "polymer",
    "polyu",
    "polyv",
    "pornhub",
    "pornpros",
    "positive-technologies",
    "postimages",
    "pptv",
    "primevideo",
    "primevideo@cn",
    "private",
    "progress",
    "projectpoi",
    "projectsekai",
    "proquest",
    "protonmail",
    "pstorage",
    "ptt",
    "pubg",
    "pubmatic",
    "pugpig",
    "purikonejp",
    "python",
    "qcc",
    "qcloud",
    "qcloud@!cn",
    "qianxin",
    "qihoo360",
    "qihoo360@ads",
    "qimao",
    "qingcloud",
    "qingtingfm",
    "qiniu",
    "qixin",
    "qnap",
    "qnap@cn",
    "qt",
    "qualcomm",
    "qualcomm@cn",
    "quantil",
    "quip",
    "quora",
    "qwant",
    "qweather",
    "radiko",
    "raiffeisenbank",
    "rakuten",
    "rarbg",
    "razer",
    "razer@cn",
    "rb",
    "rb@cn",
    "reabble",
    "reabble@cn",
    "readthedocs",
    "reagroup",
    "realclear",
    "realitykings",
    "realtype",
    "rebrandly",
    "reddit",
    "redhat",
    "redis",
    "redotpay",
    "redtube",
    "regru",
    "remnawave",
    "renren",
    "reurl",
    "reuters",
    "rferl",
    "riot",
    "riot@cn",
    "roblox",
    "rockstar",
    "roku",
    "rossiyasegodnya",
    "rostelecom",
    "rostelecom@ads",
    "rsshub",
    "rsshub-3rd",
    "rt",
    "rthk",
    "ruanmei",
    "ruby",
    "rubychina",
    "ruleoflaw",
    "rumble",
    "rust",
    "ruten",
    "rutracker",
    "rutube",
    "safepal",
    "sakurafrp",
    "salesforce",
    "samsung",
    "samsung@ads",
    "samsung@cn",
    "sankei",
    "sb",
    "sber",
    "sber@ads",
    "scala",
    "scaleflex",
    "scenesource",
    "schoopia",
    "schwab",
    "sci",
    "sci-hub",
    "sciencedirect",
    "scmp",
    "scp",
    "seasun",
    "secom",
    "sectigo",
    "sectigo@cn",
    "segment",
    "segment@ads",
    "segmentfault",
    "sehuatang",
    "selectel",
    "sentry",
    "servicepipe",
    "setapp",
    "setn",
    "sf-express",
    "shadowsockscom",
    "shanbay",
    "sharethis",
    "shireyishunjian",
    "shopee",
    "shopee@cn",
    "shopify",
    "shorturl",
    "showtimeanytime",
    "shuqi",
    "signal",
    "sina",
    "sina@!cn",
    "sina@ads",
    "singtaonewscorp",
    "sinopec",
    "sitepoint",
    "skillshare",
    "sky",
    "sky@cn",
    "skyeng",
    "skyperfect",
    "slack",
    "slideshare",
    "sling",
    "smartone",
    "smena",
    "smtiaojiaoshi",
    "smzdm",
    "snap",
    "snap@ads",
    "snapcraft",
    "snapp",
    "snk",
    "snodehome",
    "softbank",
    "softether",
    "sogou",
    "sogou@ads",
    "sohu",
    "sohu@ads",
    "sokolov",
    "sonemic",
    "sony",
    "sonypictures",
    "soundcloud",
    "soundofhope",
    "sourceforge",
    "sourcehut",
    "soyjakparty",
    "spacemail",
    "spaceship",
    "spacex",
    "spankbang",
    "speedtest",
    "speedtest@ads",
    "spiceworks",
    "spotify",
    "spotify@ads",
    "springer",
    "squareup",
    "squirrelvpn",
    "sslcom",
    "sslcom@cn",
    "ssrcloud",
    "st",
    "st@cn",
    "stackexchange",
    "stackpath",
    "stage1st",
    "standardchartered",
    "starbucks",
    "starbucks@cn",
    "starfieldtech",
    "starplus",
    "startpage",
    "starworld",
    "staticfile",
    "steam",
    "steam@cn",
    "steaminventoryhelper",
    "steamunlocked",
    "steemit",
    "sto-express",
    "straitsx",
    "streamable",
    "strepsils",
    "strepsils@cn",
    "strikingly",
    "stripe",
    "subscene",
    "suishouji",
    "sumkoo",
    "suning",
    "supercell",
    "supersonic",
    "supersonic@ads",
    "surflite",
    "suruga-ya",
    "svp",
    "swag",
    "swift",
    "swift@cn",
    "swisssign",
    "sxl",
    "symantec",
    "symantec-pki",
    "synology",
    "synology@cn",
    "t2-ru",
    "taboola",
    "taihe",
    "taikang",
    "tailscale",
    "take-two",
    "talkatone",
    "taomee",
    "taptap",
    "taptap@!cn",
    "target",
    "taylorfrancis",
    "tbank-ru",
    "tcl",
    "tcl@ads",
    "teambition",
    "teamspeak",
    "teamviewer",
    "teamviewer@cn",
    "technogym",
    "techpowerup",
    "techtimes",
    "ted",
    "telegram",
    "telekom",
    "temp-mail",
    "tencent",
    "tencent-dev",
    "tencent-dev@ads",
    "tencent-games",
    "tencent-tme",
    "tencent-tme@ads",
    "tencent@!cn",
    "tencent@ads",
    "tendcloud",
    "tendcloud@ads",
    "terabox",
    "termux",
    "tesla",
    "tesla@cn",
    "test",
    "test-ipv6",
    "test-ipv6@cn",
    "tex",
    "tgbus",
    "theboringcompany",
    "theguardian",
    "theinitium",
    "thelinuxfoundation",
    "thelinuxfoundation@cn",
    "theporndude",
    "thescoregroup",
    "thesun",
    "thetimes",
    "thetype",
    "thetype@cn",
    "thomsonreuters",
    "threads",
    "tiancity",
    "tianyancha",
    "tidal",
    "tidelift",
    "tiktok",
    "tiktok@!cn",
    "tiktok@ads",
    "tilda",
    "timeweb",
    "tinyurl",
    "tld-!cn",
    "tld-cn",
    "tld-opennic",
    "tld-ru",
    "tmdb",
    "tmtpost",
    "tokyo-sports",
    "tokyo-toshokan",
    "tonec",
    "tongcheng",
    "tongfang",
    "tor",
    "torproject",
    "trackernetwork",
    "trae",
    "translatewiki",
    "trello",
    "trustasia",
    "trustwallet",
    "trustwave",
    "truyen-hentai",
    "tsquare",
    "tube8",
    "tubi",
    "tumblr",
    "tutanota",
    "tvb",
    "tvb@cn",
    "tvdb",
    "tver",
    "twca",
    "twilio",
    "twitch",
    "twitter",
    "twitter@ads",
    "typekit",
    "typenetwork",
    "typography",
    "uber",
    "ubiquiti",
    "ubiquiti@cn",
    "ubisoft",
    "ubuntu",
    "ubuntukylin",
    "uc",
    "uc@ads",
    "ucloud",
    "ucoz",
    "udacity",
    "udemy",
    "udn",
    "umeng",
    "umeng@ads",
    "unext",
    "unionpay",
    "unity",
    "unity@ads",
    "unitychina",
    "unitychina@ads",
    "uoliv",
    "upai",
    "usersdrive",
    "uu-chat",
    "v2ray",
    "v8",
    "vancl",
    "vanish",
    "vanish@cn",
    "vaptcha",
    "veet",
    "veet@cn",
    "vercel",
    "verisign",
    "verisign-pki",
    "verizon",
    "vgtime",
    "viber",
    "vilavpn",
    "vimeo",
    "visa",
    "visa@cn",
    "visualarts",
    "viu",
    "vivo",
    "vivo@!cn",
    "vixengroup",
    "vk",
    "vk@ads",
    "vmware",
    "vmware@cn",
    "voanews",
    "vodafone",
    "vokino",
    "volcengine",
    "volmoe",
    "volvo",
    "volvo@cn",
    "voxmedia",
    "vpngate",
    "vrcdn",
    "vrchat",
    "vrzwk",
    "vultr",
    "w3schools",
    "wallhaven",
    "walmart",
    "walmart@cn",
    "wanfang",
    "wangsu",
    "wanmei",
    "wantmedia",
    "wargaming",
    "wasu",
    "watchout",
    "wbgames",
    "weathercn",
    "webex",
    "webex@cn",
    "webnovel",
    "webnovel@!cn",
    "webtype",
    "weiphone",
    "wenshushu",
    "westerndigital",
    "westerndigital@cn",
    "whatsapp",
    "wholefoodsmarket",
    "whoosh",
    "wikidot",
    "wikihow",
    "wikimedia",
    "wildberries",
    "wildberries@ads",
    "windsurf",
    "windy",
    "wink",
    "winline",
    "wise",
    "wisekey",
    "wish",
    "wistia",
    "wiwide",
    "wix",
    "wjx",
    "wolai",
    "woolite",
    "woolite@cn",
    "wordpress",
    "wps",
    "wsj",
    "wwe",
    "wynd",
    "x",
    "x5",
    "x5@ads",
    "x@ads",
    "xai",
    "xbox",
    "xbox@cn",
    "xd",
    "xd@!cn",
    "xda",
    "xdty",
    "xedge",
    "xhamster",
    "xhamster@ads",
    "xiaoheihe",
    "xiaohongshu",
    "xiaohongshu@!cn",
    "xiaomi",
    "xiaomi-ai",
    "xiaomi-iot",
    "xiaomi@!cn",
    "xiaomi@ads",
    "xiaoyuzhou",
    "ximalaya",
    "ximalaya@ads",
    "xingkongwuxianmedia",
    "xingrz",
    "xnxx",
    "xtom",
    "xueersi",
    "xueqiu",
    "xunlei",
    "xvideos",
    "yahoo",
    "yahoo@ads",
    "yahoo@cn",
    "yandex",
    "yandex@ads",
    "ycombinator",
    "ymtc",
    "ynet",
    "ynoproject",
    "yokaverse",
    "yomiuri",
    "yostar",
    "yostar@cn",
    "youjizz",
    "youku",
    "youku@!cn",
    "youku@ads",
    "youmind",
    "youporn",
    "youquan",
    "youtube",
    "youtube@ads",
    "youtube@cn",
    "youzan",
    "yto-express",
    "yuanbei",
    "yuanfudao",
    "yuewen",
    "yuewen@!cn",
    "yuketang",
    "yundaex",
    "yunfanjiasu",
    "yunlaopo",
    "yy",
    "z-library",
    "z3x-team",
    "zaobao",
    "zb",
    "zdns",
    "zee",
    "zeetv",
    "zendesk",
    "zeplin",
    "zhangtao",
    "zhihu",
    "zhihu@ads",
    "zhimeishe",
    "zhubajie",
    "ziroom",
    "zoho",
    "zoom",
    "zotero",
    "zscaler",
    "zte",
    "zto-express",
    "zuoyebang",
    "zuoyebang@ads",
    "zynga",
];

/// The published names of one geo kind.
fn published_geo_names(kind: &str) -> &'static [&'static str] {
    match kind {
        "geoip" => PUBLISHED_GEOIP_NAMES,
        "geosite" => PUBLISHED_GEOSITE_NAMES,
        _ => &[],
    }
}

/// `true` when `name` is published by the official SagerNet repository for
/// `kind` (`geoip`/`geosite`).
pub fn is_published_geo_name(kind: &str, name: &str) -> bool {
    if name.is_empty() || name.contains('/') {
        return false;
    }
    published_geo_names(kind).contains(&name)
}

/// Clash geo values that name no published rule-set but have an exact
/// sing-box equivalent, mapped EXPLICITLY.
///
/// `GEOIP,LAN` / `GEOIP,private` are pseudo-databases in clash (mihomo's
/// GeoIP database resolves them to the private address ranges), not
/// downloads. The SagerNet repositories publish no `geoip-lan` /
/// `geoip-private` (verified 404), and mapping them onto `geosite-private`
/// would swap IP ranges for private *domains* — a silent weakening. They
/// are expressed as literal `ip_cidr` matches instead, which needs no
/// download and keeps `GEOIP,LAN,DIRECT` meaning "private traffic goes
/// direct".
const PRIVATE_IP_GEO_VALUES: &[&str] = &["lan", "private", "local"];

/// Clash pseudo-geosites with an explicitly named replacement set:
/// `(clash value, published sing-box rule-set tag)`.
const GEOSITE_PSEUDO_VALUES: &[(&str, &str)] = &[("private", "geosite-private"), ("lan", "geosite-private")];

/// How a clash `GEOIP,<value>` / `GEOSITE,<value>` reference is expressed
/// for sing-box.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GeoValueMatch {
    /// A reference to an official rule-set with this tag.
    RuleSet(String),
    /// Literal private address ranges (clash's `GEOIP,LAN`), which have no
    /// published rule-set.
    PrivateNetworks,
}

/// Resolve one clash geo value into its sing-box form, or `None` when it
/// cannot be expressed.
///
/// A value outside the published list is NEVER turned into a guessed tag:
/// the caller keeps the rule verbatim for clash and drops it (with a note)
/// for sing-box, which is strictly better than a 404 rule-set download.
pub fn classify_geo_value(kind: &str, value: &str) -> Option<GeoValueMatch> {
    let value = value.trim();
    if value.is_empty() || value.starts_with('!') {
        return None;
    }
    let normalized = value.to_ascii_lowercase();
    match kind {
        "geoip" if PRIVATE_IP_GEO_VALUES.contains(&normalized.as_str()) => Some(GeoValueMatch::PrivateNetworks),
        "geosite" => match GEOSITE_PSEUDO_VALUES.iter().find(|(value, _)| *value == normalized) {
            Some((_, tag)) => Some(GeoValueMatch::RuleSet((*tag).to_string())),
            None if is_published_geo_name("geosite", &normalized) => {
                Some(GeoValueMatch::RuleSet(format!("geosite-{normalized}")))
            }
            None => None,
        },
        "geoip" if is_published_geo_name("geoip", &normalized) => {
            Some(GeoValueMatch::RuleSet(format!("geoip-{normalized}")))
        }
        _ => None,
    }
}

/// The remote `.srs` rule-set definition for a `geoip-*` / `geosite-*` tag
/// that is actually published by the SagerNet repositories, or `None` for
/// any other tag.
///
/// Gating matters as much as the URL: `geoip-lan` looks like a plausible
/// tag, 404s on download and aborts `sing-box start` with
/// `initialize rule-set: geoip-lan: 404`.
pub fn geo_rule_set(tag: &str) -> Option<Value> {
    let (kind, name) = tag
        .strip_prefix("geoip-")
        .map(|name| ("geoip", name))
        .or_else(|| tag.strip_prefix("geosite-").map(|name| ("geosite", name)))?;
    if !is_published_geo_name(kind, name) {
        return None;
    }
    let base = if kind == "geoip" {
        GEOIP_RULE_SET_BASE
    } else {
        GEOSITE_RULE_SET_BASE
    };
    Some(json!({
        "type": "remote",
        "tag": tag,
        "format": "binary",
        "url": format!("{base}/{tag}.srs"),
        // Fetching the geo lists must never recurse through the proxy
        // chain they are about to shape.
        "download_detour": "direct",
        "update_interval": "168h",
    }))
}

/// Outcome of converting a clash `rule-providers` block.
#[derive(Debug, Clone, Default)]
pub struct RuleProviderConversion {
    /// sing-box `route.rule_set` entries.
    pub rule_sets: Vec<Value>,
    /// Providers that cannot be represented, with the reason.
    pub skipped: Vec<String>,
}

/// Convert `rule-providers` into sing-box `route.rule_set` entries so a
/// profile's `RULE-SET,<name>,<policy>` rules stay resolvable (#52).
///
/// Only providers that already publish a sing-box `.srs` payload can be
/// mapped: clash's `classical`/`yaml` payloads have no sing-box equivalent
/// and would need to be converted by downloading and re-serialising them,
/// which this pass never does. Those are reported, and the rules that
/// reference them are dropped by the caller instead of failing the whole
/// config with a missing-rule-set error.
pub fn convert_rule_providers(config_yaml: &str) -> Result<RuleProviderConversion, String> {
    let doc: Yaml = serde_yaml_ng::from_str(config_yaml).map_err(|error| format!("invalid YAML: {error}"))?;
    let mut result = RuleProviderConversion::default();
    let Yaml::Mapping(map) = &doc else {
        return Ok(result);
    };
    let Some(providers) = map.get(Yaml::String("rule-providers".into())) else {
        return Ok(result);
    };
    // clash spells `rule-providers` as a mapping of name → definition; some
    // generators emit a list with an explicit `name` field instead.
    let entries: Vec<(String, &Yaml)> = match providers {
        Yaml::Mapping(providers) => providers
            .iter()
            .filter_map(|(name, entry)| Some((name.as_str()?.to_string(), entry)))
            .collect(),
        Yaml::Sequence(providers) => providers
            .iter()
            .filter_map(|provider| {
                Some((
                    provider.get(Yaml::String("name".into()))?.as_str()?.to_string(),
                    provider,
                ))
            })
            .collect(),
        _ => Vec::new(),
    };
    for (name, provider) in entries {
        let Yaml::Mapping(entry) = provider else { continue };
        let url = entry.get(Yaml::String("url".into())).and_then(Yaml::as_str);
        match url {
            Some(url) if publishable_rule_set_url(url) => result.rule_sets.push(json!({
                "type": "remote",
                "tag": name,
                "format": "binary",
                "url": url,
                "download_detour": "direct",
                "update_interval": "168h",
            })),
            Some(url) => result.skipped.push(format!(
                "{name}: rule-provider payload {url:?} is not a sing-box .srs rule-set ({}); \
RULE-SET rules referencing it are skipped",
                entry
                    .get(Yaml::String("behavior".into()))
                    .and_then(Yaml::as_str)
                    .unwrap_or("unknown behavior")
            )),
            None => result.skipped.push(format!(
                "{name}: rule-provider has no url; RULE-SET rules referencing it are skipped"
            )),
        }
    }
    Ok(result)
}

/// `true` when a provider URL points at a sing-box binary (`.srs`) rule-set.
///
/// - the suffix check ignores query strings and fragments, because
///   subscription providers commonly append cache-busting parameters
///   (`ads.srs?version=3`) which used to make an otherwise convertible
///   provider unconvertible;
/// - `.mrs` (the mihomo-flavored binary rule-set) is deliberately NOT
///   accepted: verified against sing-box 1.14.2, an `.mrs` payload fails
///   rule-set initialization with `invalid sing-box rule-set file` — its
///   magic is a bare zstd frame, while 1.14.2 parses the `SRS` container
///   (`SRS\x01` for the files SagerNet publishes, `SRS\x02` for the ones it
///   compiles itself). Accepting it would turn a clean "provider skipped"
///   note into a FATAL at core start.
fn publishable_rule_set_url(url: &str) -> bool {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    path.to_ascii_lowercase().ends_with(".srs")
}

#[cfg(test)]
mod profile_tests {
    use super::*;

    #[test]
    fn converts_nodes_and_groups_with_skip_report() {
        let yaml = r#"
proxies:
  - name: ok-node
    type: ss
    server: 1.1.1.1
    port: 8388
    cipher: aes-256-gcm
    password: p
  - name: bad-node
    type: mieru
    server: x
    port: 1
proxy-groups:
  - name: PROXY
    type: select
    proxies: [ok-node]
  - name: relay-g
    type: relay
    proxies: [ok-node]
"#;
        let result = convert_profile(yaml).expect("convert profile");
        assert_eq!(result.outbounds.len(), 1);
        assert_eq!(result.outbounds[0]["tag"], "ok-node");
        assert_eq!(result.groups.len(), 1);
        assert_eq!(result.groups[0].name, "PROXY");
        assert_eq!(result.groups[0].kind, crate::singbox::GroupKind::Selector);
        assert_eq!(
            result.skipped.len(),
            2,
            "bad node + refused relay group: {:?}",
            result.skipped
        );
        assert!(result.skipped[0].starts_with("bad-node:"), "{:?}", result.skipped);
        assert!(
            result.skipped.iter().any(|line| line.contains("relay-g")),
            "{:?}",
            result.skipped
        );
    }

    #[test]
    fn fallback_and_load_balance_groups_become_urltests_with_a_note() {
        // #52: these groups used to land in `skipped`, so the CLI dropped
        // them silently and the TUI refused to switch cores.
        let yaml = r#"
proxies:
  - {name: n1, type: http, server: a.example, port: 443}
  - {name: n2, type: http, server: b.example, port: 443}
proxy-groups:
  - {name: Auto, type: fallback, proxies: [n1, n2]}
  - {name: Spread, type: load-balance, proxies: [n1, n2]}
"#;
        let result = convert_profile(yaml).expect("convert");
        assert!(result.skipped.is_empty(), "{:?}", result.skipped);
        assert_eq!(result.groups.len(), 2);
        for group in &result.groups {
            assert_eq!(group.kind, crate::singbox::GroupKind::UrlTest, "{group:?}");
        }
        assert!(
            result
                .notes
                .iter()
                .any(|note| note.contains("Auto") && note.contains("urltest")),
            "{:?}",
            result.notes
        );
        assert!(
            result.notes.iter().any(|note| note.contains("Spread")),
            "{:?}",
            result.notes
        );
    }

    #[test]
    fn dangling_group_members_are_pruned_so_references_stay_valid() {
        // A group naming a node the converter skipped (and one naming a
        // group that no longer exists) must not survive as a reference —
        // validate_references rejects those, which used to abort `start`.
        let yaml = r#"
proxies:
  - {name: ok, type: http, server: a.example, port: 443}
  - {name: gone, type: mieru, server: b.example, port: 443}
proxy-groups:
  - {name: Missing, type: select, proxies: [ok]}
  - {name: PROXY, type: select, proxies: [ok, gone, Missing, nope, DIRECT, ok]}
"#;
        let result = convert_profile(yaml).expect("convert");
        let proxy = result.groups.iter().find(|group| group.name == "PROXY").expect("PROXY");
        // `Missing` is a group that converted fine, so it stays a member.
        assert_eq!(
            proxy.members,
            vec!["ok".to_string(), "Missing".to_string()],
            "{:?}",
            proxy.members
        );
        assert!(
            result.notes.iter().any(|note| note.contains("dropped member 'gone'")),
            "{:?}",
            result.notes
        );
        assert!(
            result.notes.iter().any(|note| note.contains("dropped member 'nope'")),
            "{:?}",
            result.notes
        );
        // Every surviving member resolves to a tag the config will have.
        let tags = result.outbound_tags();
        assert!(proxy.members.iter().all(|member| tags.contains(member)));
    }

    #[test]
    fn a_group_left_without_members_is_reported_not_emitted() {
        let yaml = r#"
proxies:
  - {name: gone, type: mieru, server: b.example, port: 443}
proxy-groups:
  - {name: PROXY, type: select, proxies: [gone]}
"#;
        let result = convert_profile(yaml).expect("convert");
        assert!(result.groups.is_empty(), "{:?}", result.groups);
        assert!(
            result.skipped.iter().any(|line| line.contains("no member left")),
            "{:?}",
            result.skipped
        );
    }

    #[test]
    fn geo_rule_sets_point_at_the_official_srs_repositories() {
        let ip = geo_rule_set("geoip-cn").expect("geoip");
        assert_eq!(ip["url"], format!("{GEOIP_RULE_SET_BASE}/geoip-cn.srs"));
        assert_eq!(ip["download_detour"], "direct");
        let site = geo_rule_set("geosite-geolocation-!cn").expect("geosite");
        assert_eq!(
            site["url"],
            format!("{GEOSITE_RULE_SET_BASE}/geosite-geolocation-!cn.srs")
        );
        assert!(geo_rule_set("some-local-set").is_none());
        assert!(geo_rule_set("geoip-").is_none());
    }

    #[test]
    fn unpublished_geo_tags_are_never_synthesized() {
        // #P0-2: every one of these used to produce a plausible-looking
        // tag whose download 404s, and `sing-box run` then dies with
        // `initialize rule-set: geoip-lan: 404` after `check` passed.
        for tag in [
            "geoip-lan",
            "geoip-private",
            "geoip-local",
            "geoip-telegram",
            "geoip-ZZ",
            "geosite-category-scholar",
            "geosite-not-published",
            "geosite-private/../evil",
        ] {
            assert!(geo_rule_set(tag).is_none(), "{tag} must not be synthesized");
        }
        for tag in ["geoip-cn", "geoip-us", "geosite-category-ads-all", "geosite-private"] {
            assert!(geo_rule_set(tag).is_some(), "{tag} is published");
        }
    }

    #[test]
    fn published_geo_name_gate_matches_the_official_repositories() {
        // The gate is only as good as the embedded lists; spot-check both
        // ends of them so a truncated regeneration fails loudly.
        for name in ["cn", "us", "de", "hk", "tw", "mo"] {
            assert!(is_published_geo_name("geoip", name), "geoip {name}");
        }
        for name in [
            "private",
            "category-ads-all",
            "geolocation-!cn",
            "geolocation-cn",
            "google",
        ] {
            assert!(is_published_geo_name("geosite", name), "geosite {name}");
        }
        assert!(!is_published_geo_name("geoip", "lan"));
        assert!(!is_published_geo_name("geosite", "lan"));
        assert!(!is_published_geo_name("geosite", ""));
        assert!(!is_published_geo_name("geosite", "a/b"));
        assert!(PUBLISHED_GEOIP_NAMES.len() > 200);
        assert!(PUBLISHED_GEOSITE_NAMES.len() > 1500);
    }

    /// Regression: the previous whitespace-wrapped blobs were damaged by the
    /// line wrap — `apple-music@cn` was stored as `apple-` + `music@cn` and
    /// `aws-cn` as `aws-` + `cn`, so those two legal `GEOSITE,` values were
    /// classified as unpublished and silently dropped from every profile.
    #[test]
    fn line_wrapped_geo_name_fragments_are_not_mistaken_for_published_names() {
        for name in [
            "apple-music@cn",
            "aws-cn",
            "google-trust-services@cn",
            "misskey-universe",
            "nikkan-gendai",
            "xiaomi-iot",
            "zto-express",
            "category-android-app-download",
            "category-pt",
            "category-vpnservices",
            "category-password-management",
            "category-stun",
            "category-communication",
        ] {
            assert!(is_published_geo_name("geosite", name), "geosite {name} is published");
            assert!(
                geo_rule_set(&format!("geosite-{name}")).is_some(),
                "geosite-{name} must synthesize a rule-set"
            );
        }
        // The wrap fragments must NOT be published names on their own.
        for fragment in [
            "apple-",
            "music@cn",
            "aws-",
            "cn-",
            "google-trust-",
            "services@cn",
            "xiaomi-",
            "iot",
            "misskey-",
            "universe",
            "nikkan-",
            "gendai",
            "zto-",
            "express",
            "category-",
            "pt",
            "stun",
            "games",
            "communication",
            "management",
        ] {
            assert!(
                !is_published_geo_name("geosite", fragment),
                "fragment {fragment} must stay unpublished"
            );
        }
        assert_eq!(
            classify_geo_value("geosite", "apple-music@cn"),
            Some(GeoValueMatch::RuleSet("geosite-apple-music@cn".into()))
        );
        assert_eq!(
            classify_geo_value("geosite", "aws-cn"),
            Some(GeoValueMatch::RuleSet("geosite-aws-cn".into()))
        );
    }

    /// Structural guard: every embedded entry is one intact, sorted, unique
    /// upstream file name. `sha256` fingerprints of the sorted name lists
    /// regenerated from the GitHub tree API of
    /// `SagerNet/sing-geosite@rule-set` / `SagerNet/sing-geoip@rule-set`
    /// (`geosite-<name>.srs` / `geoip-<name>.srs`, 1887 / 238 files).
    ///
    /// On an upstream rename/addition this test fails loudly: regenerate the
    /// arrays from the tree API, drop the two fingerprints, and add the new
    /// names back to this test's spot-checks.
    #[test]
    fn published_geo_name_lists_are_sorted_unique_and_match_the_official_set() {
        use sha2::Digest as _;
        for (kind, names, count, fingerprint) in [
            (
                "geoip",
                PUBLISHED_GEOIP_NAMES,
                238,
                "49d432c42e558a4c0ac337a69a7ddc69dd73c69de0d80178f8fca5abcf1bfdbd",
            ),
            (
                "geosite",
                PUBLISHED_GEOSITE_NAMES,
                1887,
                "54d700ac1b5996e886887bbb4a4e2184f7fd22ed22ade97a13beeb23a9774029",
            ),
        ] {
            assert_eq!(names.len(), count, "{kind}: upstream file-name count changed");
            let mut sorted: Vec<_> = names.to_vec();
            sorted.sort_unstable();
            sorted.dedup();
            assert_eq!(sorted.len(), names.len(), "{kind}: embedded names must be unique");
            assert_eq!(sorted, names, "{kind}: embedded names must be sorted");
            for name in names {
                assert!(!name.is_empty(), "{kind}: empty name");
                assert!(
                    name.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_@!.".contains(&b)),
                    "{kind}: {name} is not a bare rule-set file name"
                );
                assert!(is_published_geo_name(kind, name), "{kind}: {name}");
                assert!(geo_rule_set(&format!("{kind}-{name}")).is_some());
            }
            let payload = sorted.join("\n") + "\n";
            let digest = sha2::Sha256::digest(payload.as_bytes());
            assert_eq!(
                format!("{digest:x}"),
                fingerprint,
                "{kind}: embedded list no longer matches the official rule-set branch"
            );
        }
    }

    #[test]
    fn clash_geo_values_are_classified_against_the_published_names() {
        assert_eq!(
            classify_geo_value("geoip", "CN"),
            Some(GeoValueMatch::RuleSet("geoip-cn".into()))
        );
        assert_eq!(
            classify_geo_value("geosite", "geolocation-!cn"),
            Some(GeoValueMatch::RuleSet("geosite-geolocation-!cn".into()))
        );
        assert_eq!(
            classify_geo_value("geosite", "private"),
            Some(GeoValueMatch::RuleSet("geosite-private".into()))
        );
        assert_eq!(classify_geo_value("geoip", "LAN"), Some(GeoValueMatch::PrivateNetworks));
        // Negated sets have no positive rule-set reference; unknown names
        // are never guessed.
        assert_eq!(classify_geo_value("geoip", "!cn"), None);
        assert_eq!(classify_geo_value("geosite", "!cn"), None);
        assert_eq!(classify_geo_value("geoip", ""), None);
        assert_eq!(classify_geo_value("geoip", "not-published"), None);
        assert_eq!(classify_geo_value("geosite", "not-published"), None);
    }

    #[test]
    fn members_referencing_groups_that_produce_no_outbound_are_pruned() {
        // #P1-3: pruning used the DECLARED group names, so a member
        // pointing at a skipped relay group (or a group emptied by
        // pruning) survived into `GroupSpec.members` and made
        // `config_gen::validate_references` abort generation.
        let yaml = r#"
proxies:
  - {name: ok, type: http, server: a.example, port: 443}
  - {name: gone, type: mieru, server: b.example, port: 443}
proxy-groups:
  - {name: Relay, type: relay, proxies: [ok]}
  - {name: Emptied, type: select, proxies: [gone]}
  - {name: PROXY, type: select, proxies: [ok, Relay, Emptied]}
"#;
        let result = convert_profile(yaml).expect("convert");
        let proxy = result.groups.iter().find(|group| group.name == "PROXY").expect("PROXY");
        assert_eq!(proxy.members, vec!["ok".to_string()], "{:?}", proxy.members);
        assert!(
            result
                .notes
                .iter()
                .any(|note| note.contains("dropped member 'Relay'") && note.contains("produced no outbound")),
            "{:?}",
            result.notes
        );
        assert!(
            result
                .notes
                .iter()
                .any(|note| note.contains("dropped member 'Emptied'")),
            "{:?}",
            result.notes
        );
        let tags = result.outbound_tags();
        assert!(proxy.members.iter().all(|member| tags.contains(member)));
    }

    #[test]
    fn members_referencing_a_group_converted_later_still_survive() {
        // The fixed-point pruning must not depend on declaration order.
        let yaml = r#"
proxies:
  - {name: ok, type: http, server: a.example, port: 443}
proxy-groups:
  - {name: PROXY, type: select, proxies: [ok, Later]}
  - {name: Later, type: select, proxies: [ok]}
"#;
        let result = convert_profile(yaml).expect("convert");
        let proxy = result.groups.iter().find(|group| group.name == "PROXY").expect("PROXY");
        assert_eq!(
            proxy.members,
            vec!["ok".to_string(), "Later".to_string()],
            "{:?}",
            proxy.members
        );
        assert!(result.notes.is_empty(), "{:?}", result.notes);
    }

    #[test]
    fn a_select_group_with_only_direct_survives() {
        // #P1-4: `DIRECT` used to be stripped before the emptiness check,
        // so `{select, proxies: [DIRECT]}` was dropped and every rule
        // targeting it lost its outbound.
        let yaml = r#"
proxies:
  - {name: ok, type: http, server: a.example, port: 443}
proxy-groups:
  - {name: Manual, type: select, proxies: [DIRECT]}
  - {name: Mixed, type: select, proxies: [DIRECT, ok, ok]}
  - {name: DirectOnly, type: url-test, proxies: [DIRECT]}
"#;
        let result = convert_profile(yaml).expect("convert");
        let group = |name: &str| {
            result
                .groups
                .iter()
                .find(|group| group.name == name)
                .unwrap_or_else(|| panic!("{name} must survive: {:?} / {:?}", result.groups, result.skipped))
                .clone()
        };
        assert_eq!(group("Manual").members, vec!["DIRECT".to_string()]);
        assert_eq!(group("DirectOnly").members, vec!["DIRECT".to_string()]);
        // With another member present, `DIRECT` is still redundant: the
        // generator appends it to every selector.
        assert_eq!(group("Mixed").members, vec!["ok".to_string()]);
        assert!(
            result.notes.iter().any(|note| note.contains("redundant DIRECT")),
            "{:?}",
            result.notes
        );
        // A DIRECT-only selector is a valid outbound for sing-box.
        let tags = result.outbound_tags();
        for name in ["Manual", "Mixed", "DirectOnly"] {
            assert!(group(name).members.iter().all(|member| tags.contains(member)));
        }
    }

    #[test]
    fn rule_providers_tolerate_query_strings_and_reject_unsupported_payloads() {
        // #P2: `ads.srs?version=3` is the same binary rule-set, and the
        // suffix check must ignore the query. `.mrs` is NOT accepted:
        // verified against sing-box 1.14.2, an mihomo `.mrs` payload
        // fails with `invalid sing-box rule-set file`.
        let yaml = r#"
rule-providers:
  cached:
    type: http
    behavior: domain
    url: https://example.com/ads.srs?version=3&token=abc
  upper:
    type: http
    behavior: domain
    url: https://example.com/ads.SRS
  mihomo:
    type: http
    behavior: domain
    url: https://example.com/cn.mrs
  broken:
    type: http
    behavior: classical
    url: https://example.com/cn.yaml
  urlless:
    type: file
    behavior: classical
"#;
        let result = convert_rule_providers(yaml).expect("convert");
        let tags: Vec<&str> = result.rule_sets.iter().filter_map(|set| set["tag"].as_str()).collect();
        assert_eq!(tags, vec!["cached", "upper"], "{:?}", result.skipped);
        assert_eq!(
            result.rule_sets[0]["url"],
            "https://example.com/ads.srs?version=3&token=abc"
        );
        assert_eq!(result.rule_sets[0]["format"], "binary");
        assert_eq!(result.skipped.len(), 3, "{:?}", result.skipped);
        assert!(
            result.skipped.iter().any(|line| line.contains("cn.mrs")),
            "{:?}",
            result.skipped
        );
        assert!(
            result.skipped.iter().any(|line| line.contains("cn.yaml")),
            "{:?}",
            result.skipped
        );
        assert!(
            result.skipped.iter().any(|line| line.contains("has no url")),
            "{:?}",
            result.skipped
        );
    }

    #[test]
    fn rule_providers_convert_only_when_they_publish_srs() {
        let yaml = r#"
rule-providers:
  ads:
    type: http
    behavior: domain
    url: https://example.com/ads.srs
  cn:
    type: http
    behavior: classical
    url: https://example.com/cn.list
"#;
        let result = convert_rule_providers(yaml).expect("convert");
        assert_eq!(result.rule_sets.len(), 1, "{:?}", result.rule_sets);
        assert_eq!(result.rule_sets[0]["tag"], "ads");
        assert_eq!(result.rule_sets[0]["url"], "https://example.com/ads.srs");
        assert_eq!(result.skipped.len(), 1, "{:?}", result.skipped);
        assert!(result.skipped[0].contains("classical"), "{:?}", result.skipped);
    }

    #[test]
    fn invalid_yaml_is_an_error() {
        assert!(convert_profile("{unclosed flow").is_err());
    }
}
