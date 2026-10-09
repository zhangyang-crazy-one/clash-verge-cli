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
    /// clash field paths that had no sing-box equivalent and were dropped.
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
    apply_udp(&ptype, &mut outbound, map)?;
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

/// Transport layer for ws/grpc networks; other networks are noted by the
/// caller through the dropped-fields report (they never reach `outbound`).
fn apply_udp(ptype: &str, outbound: &mut Value, map: &serde_yaml_ng::Mapping) -> Result<(), String> {
    let Some(value) = map.get(Yaml::String("udp".into())) else {
        return Ok(());
    };
    let enabled = value
        .as_bool()
        .ok_or_else(|| "udp must be a boolean; refusing to guess transport behavior".to_string())?;
    if !matches!(ptype, "ss" | "hysteria2") {
        return Err(format!(
            "node type {ptype:?} has no verified sing-box mapping for explicit udp: {enabled}; refusing to lose transport semantics"
        ));
    }
    // Both supported sing-box outbounds use their native default for UDP;
    // the only explicit override needed to preserve Clash semantics is
    // disabling UDP, which maps to the TCP-only network.
    if !enabled {
        outbound["network"] = json!("tcp");
    }
    Ok(())
}

fn apply_transport(ptype: &str, outbound: &mut Value, map: &serde_yaml_ng::Mapping) -> Result<(), String> {
    let get = |key: &str| map.get(Yaml::String(key.into()));
    let Some(network) = get("network").and_then(Yaml::as_str) else {
        return Ok(());
    };
    if matches!(ptype, "ss" | "hysteria2") && matches!(network, "udp" | "tcp") {
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
    if matches!(ptype, "ss" | "hysteria2") && matches!(network, "ws" | "grpc") && get("udp").is_some() {
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
        let udp = parse("name: n\ntype: vmess\nserver: host\nport: 443\nuuid: id\nudp: false\n");
        assert!(convert_node(&udp).unwrap_err().contains("no verified sing-box mapping"));
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
                other => {
                    result.skipped.push(format!("{name}: group type '{other}' unsupported"));
                    continue;
                }
            };
            let members: Vec<String> = g
                .get(Yaml::String("proxies".into()))
                .and_then(Yaml::as_sequence)
                .map(|seq| seq.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
                .unwrap_or_default();
            if members.is_empty() {
                continue;
            }
            result.groups.push(crate::singbox::GroupSpec { name, kind, members });
        }
    }

    Ok(result)
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
  - name: fallback-g
    type: fallback
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
            "bad node + fallback group: {:?}",
            result.skipped
        );
        assert!(result.skipped[0].starts_with("bad-node:"), "{:?}", result.skipped);
    }

    #[test]
    fn invalid_yaml_is_an_error() {
        assert!(convert_profile("{unclosed flow").is_err());
    }
}
