//! Clash YAML node → sing-box outbound conversion (task 5.1).
//!
//! Strategy per design D3: full protocol coverage with FIELD-LEVEL
//! degradation — every mapped protocol carries a whitelist of clash
//! fields that translate cleanly; anything else is dropped and reported.
//! An entirely unmappable node is an error, also reported upstream so
//! the UI can list skips.

use serde_json::{json, Value};
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
    let get = |key: &str| map.get(&Yaml::String(key.into()));
    let as_str = |key: &str| get(key).and_then(Yaml::as_str).map(str::to_string);

    let name = as_str("name").ok_or_else(|| "node missing name".to_string())?;
    let ptype = as_str("type").ok_or_else(|| format!("node '{name}' missing type"))?;

    let mut dropped: Vec<String> = Vec::new();
    for key in map.keys() {
        if let Yaml::String(k) = key {
            if !RESERVED_FIELDS.contains(&k.as_str()) {
                dropped.push(format!("{name}.{k}"));
            }
        }
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
    apply_tls(&ptype, &mut outbound, map);
    apply_transport(&mut outbound, map, &mut dropped);

    Ok(ConvertedNode { outbound, dropped })
}

/// Fields read by the converter itself (never reported as dropped).
const RESERVED_FIELDS: &[&str] = &[
    "name", "type", "server", "port", "tls", "servername", "sni", "skip-cert-verify",
    "network", "ws-opts", "grpc-opts", "reality-opts", "client-fingerprint",
];

/// (clash type, (sing-box type, [(clash field, sing-box field)]))
/// Only fields whose names/values translate directly are listed here.
const PROTOCOL_MAPS: &[(&str, (&str, &[(&str, &str)]))] = &[
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
        ("hysteria", &[("auth-str", "auth_str"), ("up", "up_mbps"), ("down", "down_mbps")]),
    ),
    (
        "hysteria2",
        ("hysteria2", &[("password", "password"), ("up", "up_mbps"), ("down", "down_mbps")]),
    ),
    (
        "tuic",
        (
            "tuic",
            &[("uuid", "uuid"), ("password", "password"), ("congestion-controller", "congestion_controller")],
        ),
    ),
    ("naive", ("naive", &[("username", "username"), ("password", "password")])),
    ("anytls", ("anytls", &[("password", "password")])),
    ("http", ("http", &[("username", "username"), ("password", "password")])),
    ("socks5", ("socks", &[("username", "username"), ("password", "password")])),
];

fn yaml_to_json(value: Option<&Yaml>) -> Option<Value> {
    match value? {
        Yaml::String(v) => Some(json!(v)),
        Yaml::Number(n) => Some(json!(n.as_f64().unwrap_or_default())),
        Yaml::Bool(b) => Some(json!(b)),
        _ => None,
    }
}

/// TLS handling: trojan/vLESS/Hysteria-family are TLS-native; the rest
/// need `tls: true` to emit the block. reality-opts maps onto uTLS/reality.
fn apply_tls(ptype: &str, outbound: &mut Value, map: &serde_yaml_ng::Mapping) {
    let get = |key: &str| map.get(&Yaml::String(key.into()));
    let native_tls = matches!(ptype, "trojan" | "vless" | "hysteria" | "hysteria2" | "tuic" | "naive");
    let explicit_tls = get("tls").and_then(Yaml::as_bool).unwrap_or(false);
    if !native_tls && !explicit_tls {
        return;
    }

    let mut tls = json!({ "enabled": true });
    if let Some(sni) = get("servername").or_else(|| get("sni")).and_then(Yaml::as_str) {
        tls["server_name"] = json!(sni);
    }
    if get("skip-cert-verify").and_then(Yaml::as_bool).unwrap_or(false) {
        tls["insecure"] = json!(true);
    }
    if let Some(Yaml::Mapping(reality)) = get("reality-opts") {
        let public_key = reality.get(&Yaml::String("public-key".into())).and_then(Yaml::as_str);
        let short_id = reality.get(&Yaml::String("short-id".into())).and_then(Yaml::as_str);
        if let Some(pk) = public_key {
            tls["utls"] = json!({ "enabled": true, "fingerprint": "chrome" });
            tls["reality"] = json!({ "enabled": true, "public_key": pk, "short_id": short_id.unwrap_or("") });
        }
    }
    outbound["tls"] = tls;
}

/// Transport layer for ws/grpc networks; other networks are noted by the
/// caller through the dropped-fields report (they never reach `outbound`).
fn apply_transport(outbound: &mut Value, map: &serde_yaml_ng::Mapping, dropped: &mut Vec<String>) {
    let get = |key: &str| map.get(&Yaml::String(key.into()));
    let Some(network) = get("network").and_then(Yaml::as_str) else {
        return;
    };
    match network {
        "ws" => {
            let mut transport = json!({ "type": "ws" });
            if let Some(Yaml::Mapping(opts)) = get("ws-opts") {
                if let Some(path) = opts.get(&Yaml::String("path".into())).and_then(Yaml::as_str) {
                    transport["path"] = json!(path);
                }
                if let Some(Yaml::Mapping(headers)) = opts.get(&Yaml::String("headers".into())) {
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
            if let Some(Yaml::Mapping(opts)) = get("grpc-opts") {
                if let Some(service) = opts.get(&Yaml::String("grpc-service-name".into())).and_then(Yaml::as_str)
                {
                    transport["service_name"] = json!(service);
                }
            }
            outbound["transport"] = transport;
        }
        other => dropped.push(format!("transport:{other}")),
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;

    fn parse(yaml: &str) -> Yaml {
        serde_yaml_ng::from_str(yaml).expect("yaml")
    }

    #[test]
    fn converts_ss_node_exactly() {
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
        let converted = convert_node(&node).expect("convert");
        assert_eq!(
            converted.outbound,
            json!({
                "type": "shadowsocks",
                "tag": "ss-node",
                "server": "1.2.3.4",
                "server_port": 8388,
                "method": "aes-256-gcm",
                "password": "pw",
            })
        );
        // `udp` has no equivalent — must be reported, not silently lost.
        assert!(converted.dropped.iter().any(|d| d.ends_with(".udp")), "{:?}", converted.dropped);
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
