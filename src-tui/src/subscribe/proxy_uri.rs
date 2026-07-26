//! Parse raw proxy URI lists (trojan://, vless://, ss://, vmess://,
//! hysteria://, hysteria2://, tuic://) into Clash-compatible YAML proxy
//! objects.
//!
//! Many proxy providers return base64-encoded lines of proxy URIs instead
//! of full Clash YAML configurations. This module detects such lists and
//! converts each recognised URI into a `serde_yaml_ng::Mapping` suitable
//! for Mihomo.

use anyhow::{Context, bail};
use base64::Engine as _;
use percent_encoding::percent_decode_str;
use serde_yaml_ng::{Mapping, Value};
use url::Url;

// ---------------------------------------------------------------------------
// scheme set used by detection
// ---------------------------------------------------------------------------

const PROXY_SCHEMES: &[&str] = &[
    "trojan://",
    "vless://",
    "ss://",
    "vmess://",
    "hysteria://",
    "hysteria2://",
    "tuic://",
    "socks://",
    "http://",
];

/// Schemes for which we have an actual parser (the rest emit a warning and
/// are skipped).
const SUPPORTED_SCHEMES: &[&str] = &["trojan", "vless", "ss", "vmess", "hysteria", "hysteria2", "tuic"];

// ---------------------------------------------------------------------------
// detection
// ---------------------------------------------------------------------------

/// Heuristic: returns `true` when the majority of non-empty, non-comment
/// lines start with a recognised proxy scheme.
pub fn detect_proxy_uri_list(data: &str) -> bool {
    let (total, matching) = data
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .fold((0u32, 0u32), |(total, m), line| {
            let is_proxy = PROXY_SCHEMES.iter().any(|scheme| line.starts_with(scheme));
            (total + 1, if is_proxy { m + 1 } else { m })
        });

    total > 0 && matching * 2 > total
}

// ---------------------------------------------------------------------------
// public entry point
// ---------------------------------------------------------------------------

/// Parse a raw proxy URI list (one per line) into a Clash-compatible
/// `{ proxies: […], proxy-groups: […] }` mapping.
///
/// Unsupported schemes and unparseable lines are skipped with a
/// `tracing::warn!` log.
pub fn parse_proxy_uri_list(data: &str) -> anyhow::Result<Mapping> {
    let mut proxies: Vec<Mapping> = Vec::new();

    for line in data.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        let (scheme, _rest) = match line.split_once("://") {
            Some(pair) => pair,
            None => {
                tracing::warn!("skipping non-URI line in proxy list: {line}");
                continue;
            }
        };

        if !SUPPORTED_SCHEMES.contains(&scheme) {
            tracing::warn!("skipping unsupported scheme: {scheme}");
            continue;
        }

        let result = match scheme {
            "trojan" => parse_trojan_uri(line),
            "vless" => parse_vless_uri(line),
            "ss" => parse_ss_uri(line),
            "vmess" => parse_vmess_uri(line),
            "hysteria" => parse_hysteria_uri(line),
            "hysteria2" => parse_hysteria2_uri(line),
            "tuic" => parse_tuic_uri(line),
            _ => unreachable!("scheme was filtered above"),
        };

        match result {
            Ok(proxy) => proxies.push(proxy),
            Err(e) => {
                tracing::warn!("failed to parse {scheme} URI: {e:#}");
            }
        }
    }

    if proxies.is_empty() {
        bail!("no valid proxy URIs found in the list");
    }

    // Build the proxy-group list: one name per proxy.
    let group_proxies: Vec<Value> = proxies
        .iter()
        .filter_map(|p| {
            p.get("name")
                .and_then(|v| v.as_str())
                .map(|n| Value::String(n.to_owned()))
        })
        .collect();

    let mut group = Mapping::new();
    group.insert("name".into(), Value::String("Proxy".into()));
    group.insert("type".into(), Value::String("select".into()));
    group.insert("proxies".into(), Value::Sequence(group_proxies));

    let mut root = Mapping::new();
    let proxies_values: Vec<Value> = proxies.into_iter().map(Value::Mapping).collect();
    root.insert("proxies".into(), Value::Sequence(proxies_values));
    root.insert("proxy-groups".into(), Value::Sequence(vec![Value::Mapping(group)]));

    Ok(root)
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// Extract the proxy name from a URL fragment (`#MyNode`). Falls back to
/// `server:port` when the fragment is absent or empty.
fn extract_name(url: &Url, server: &str, port: u16) -> String {
    if let Some(fragment) = url.fragment() {
        let decoded = percent_decode_str(fragment).decode_utf8_lossy();
        let trimmed = decoded.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    format!("{server}:{port}")
}

fn qs_owned(url: &Url, key: &str) -> Option<String> {
    url.query_pairs()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.into_owned())
        .filter(|v| !v.is_empty())
}

fn qs_bool(url: &Url, key: &str, default: bool) -> bool {
    match qs_owned(url, key).as_deref() {
        Some("1" | "true" | "True" | "TRUE") => true,
        Some("0" | "false" | "False" | "FALSE") => false,
        Some(_) => default,
        None => default,
    }
}

fn qs_i64(url: &Url, key: &str) -> Option<i64> {
    qs_owned(url, key).and_then(|v| v.parse().ok())
}

/// Insert a `serde_yaml_ng::Value::String` only when the value is non-empty.
fn insert_str(map: &mut Mapping, key: &str, value: &str) {
    if !value.is_empty() {
        map.insert(key.into(), Value::String(value.to_owned()));
    }
}

/// Insert `serde_yaml_ng::Value::Number` from i64.
fn insert_i64(map: &mut Mapping, key: &str, value: i64) {
    map.insert(key.into(), Value::Number(value.into()));
}

/// Build ws-opts / grpc-opts / h2-opts when network != tcp.
fn build_transport_opts(url: &Url, network: &str) -> Option<Mapping> {
    match network {
        "ws" => {
            let mut opts = Mapping::new();
            let path = qs_owned(url, "path").unwrap_or_else(|| "/".into());
            opts.insert("path".into(), Value::String(path));
            if let Some(host) = qs_owned(url, "host") {
                let mut headers = Mapping::new();
                headers.insert("Host".into(), Value::String(host));
                opts.insert("headers".into(), Value::Mapping(headers));
            }
            Some(opts)
        }
        "grpc" => {
            let mut opts = Mapping::new();
            if let Some(svc) = qs_owned(url, "serviceName").or_else(|| qs_owned(url, "path")) {
                opts.insert("grpc-service".into(), Value::String(svc));
            }
            Some(opts)
        }
        "h2" => {
            let mut opts = Mapping::new();
            if let Some(host) = qs_owned(url, "host") {
                opts.insert("host".into(), Value::String(host));
            }
            if let Some(path) = qs_owned(url, "path") {
                opts.insert("path".into(), Value::String(path));
            }
            if opts.is_empty() { None } else { Some(opts) }
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// trojan://password@host:port?query#name
// ---------------------------------------------------------------------------

fn parse_trojan_uri(uri: &str) -> anyhow::Result<Mapping> {
    let url = Url::parse(uri).context("invalid trojan URI")?;
    let server = url.host_str().context("missing host")?.to_owned();
    let port = url.port().unwrap_or(443);
    let password = url.username().to_owned();
    let name = extract_name(&url, &server, port);

    let network = qs_owned(&url, "type").unwrap_or_else(|| "tcp".into());
    let sni = qs_owned(&url, "sni")
        .or_else(|| qs_owned(&url, "peer"))
        .unwrap_or_else(|| server.clone());

    let mut proxy = Mapping::new();
    proxy.insert("name".into(), Value::String(name));
    proxy.insert("type".into(), Value::String("trojan".into()));
    proxy.insert("server".into(), Value::String(server));
    proxy.insert("port".into(), Value::Number(port.into()));
    proxy.insert("password".into(), Value::String(password));
    proxy.insert("sni".into(), Value::String(sni));
    proxy.insert("udp".into(), Value::Bool(true));

    if qs_bool(&url, "allowInsecure", false) {
        proxy.insert("skip-cert-verify".into(), Value::Bool(true));
    }

    if network != "tcp" {
        insert_str(&mut proxy, "network", &network);
        if let Some(opts) = build_transport_opts(&url, &network) {
            let key = match network.as_str() {
                "ws" => "ws-opts",
                "grpc" => "grpc-opts",
                _ => "ws-opts",
            };
            proxy.insert(key.into(), Value::Mapping(opts));
        }
    }

    Ok(proxy)
}

// ---------------------------------------------------------------------------
// vless://uuid@host:port?query#name
// ---------------------------------------------------------------------------

fn parse_vless_uri(uri: &str) -> anyhow::Result<Mapping> {
    let url = Url::parse(uri).context("invalid vless URI")?;
    let server = url.host_str().context("missing host")?.to_owned();
    let port = url.port().unwrap_or(443);
    let uuid = url.username().to_owned();
    let name = extract_name(&url, &server, port);

    let network = qs_owned(&url, "type").unwrap_or_else(|| "tcp".into());
    let sni = qs_owned(&url, "sni")
        .or_else(|| qs_owned(&url, "peer"))
        .unwrap_or_else(|| server.clone());

    let mut proxy = Mapping::new();
    proxy.insert("name".into(), Value::String(name));
    proxy.insert("type".into(), Value::String("vless".into()));
    proxy.insert("server".into(), Value::String(server));
    proxy.insert("port".into(), Value::Number(port.into()));
    proxy.insert("uuid".into(), Value::String(uuid));
    proxy.insert("sni".into(), Value::String(sni.clone()));
    proxy.insert("udp".into(), Value::Bool(true));

    insert_str(&mut proxy, "flow", qs_owned(&url, "flow").unwrap_or_default().as_str());
    insert_str(&mut proxy, "servername", &sni);

    if qs_bool(&url, "allowInsecure", false) {
        proxy.insert("skip-cert-verify".into(), Value::Bool(true));
    }

    if let Some(alpn) = qs_owned(&url, "alpn") {
        let alpn_list: Vec<Value> = alpn.split(',').map(|s| Value::String(s.to_owned())).collect();
        proxy.insert("alpn".into(), Value::Sequence(alpn_list));
    }

    insert_str(
        &mut proxy,
        "client-fingerprint",
        qs_owned(&url, "fp").unwrap_or_default().as_str(),
    );

    match network.as_str() {
        "reality" => {
            insert_str(&mut proxy, "network", "tcp");
            let mut reality = Mapping::new();
            insert_str(
                &mut reality,
                "public-key",
                qs_owned(&url, "pbk").unwrap_or_default().as_str(),
            );
            insert_str(
                &mut reality,
                "short-id",
                qs_owned(&url, "sid").unwrap_or_default().as_str(),
            );
            if !reality.is_empty() {
                proxy.insert("reality-opts".into(), Value::Mapping(reality));
            }
            proxy.insert("vl".into(), Value::Bool(true));
        }
        "tcp" => {
            // nothing extra
        }
        _ => {
            insert_str(&mut proxy, "network", &network);
            if let Some(opts) = build_transport_opts(&url, &network) {
                let key = match network.as_str() {
                    "ws" => "ws-opts",
                    "grpc" => "grpc-opts",
                    "h2" => "h2-opts",
                    _ => "ws-opts",
                };
                proxy.insert(key.into(), Value::Mapping(opts));
            }
        }
    }

    Ok(proxy)
}

// ---------------------------------------------------------------------------
// ss:// (SIP002 + legacy)
// ---------------------------------------------------------------------------

fn parse_ss_uri(uri: &str) -> anyhow::Result<Mapping> {
    let rest = uri.strip_prefix("ss://").context("not an ss URI")?;

    // Try SIP002 first: the URI has an '@' separating userinfo from host.
    if let Ok(url) = Url::parse(uri)
        && url.has_host()
        && !url.username().is_empty()
    {
        return parse_ss_sip002(&url);
    }

    // Legacy format: the entire string after "ss://" is a single base64
    // blob encoding `method:password@host:port`.
    parse_ss_legacy(rest)
}

fn parse_ss_sip002(url: &Url) -> anyhow::Result<Mapping> {
    let server = url.host_str().context("missing host")?.to_owned();
    let port = url.port().unwrap_or(443);
    let name = extract_name(url, &server, port);

    // userinfo is base64(method:password) — Url::username() percent-encodes
    // special chars like '=' so we must decode before base64.
    let userinfo = percent_decode_str(url.username()).decode_utf8_lossy();
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(userinfo.as_bytes())
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(userinfo.as_bytes()))
        .context("failed to base64-decode ss userinfo")?;
    let decoded_str = String::from_utf8(decoded).context("ss userinfo is not valid UTF-8")?;
    let (method, password) = decoded_str
        .split_once(':')
        .context("ss userinfo must be method:password")?;

    let mut proxy = Mapping::new();
    proxy.insert("name".into(), Value::String(name));
    proxy.insert("type".into(), Value::String("ss".into()));
    proxy.insert("server".into(), Value::String(server));
    proxy.insert("port".into(), Value::Number(port.into()));
    proxy.insert("cipher".into(), Value::String(method.to_owned()));
    proxy.insert("password".into(), Value::String(password.to_owned()));
    proxy.insert("udp".into(), Value::Bool(true));

    // Optional plugin
    if let Some(plugin) = qs_owned(url, "plugin") {
        insert_str(&mut proxy, "plugin", &plugin);
        if let Some(opts_str) = qs_owned(url, "plugin-opts") {
            let mut plugin_opts = Mapping::new();
            for pair in opts_str.split(';') {
                if let Some((k, v)) = pair.split_once('=') {
                    plugin_opts.insert(k.to_owned().into(), Value::String(v.to_owned()));
                }
            }
            if !plugin_opts.is_empty() {
                proxy.insert("plugin-opts".into(), Value::Mapping(plugin_opts));
            }
        }
    }

    Ok(proxy)
}

fn parse_ss_legacy(rest: &str) -> anyhow::Result<Mapping> {
    // Strip fragment first (we'll use it for the name).
    let (encoded, fragment) = match rest.find('#') {
        Some(pos) => (&rest[..pos], Some(&rest[pos + 1..])),
        None => (rest, None),
    };
    // Strip query params if present.
    let encoded = match encoded.find('?') {
        Some(pos) => &encoded[..pos],
        None => encoded,
    };

    let decoded_bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(encoded))
        .context("failed to base64-decode legacy ss URI")?;
    let decoded = String::from_utf8(decoded_bytes).context("legacy ss URI is not valid UTF-8")?;

    // Format: method:password@host:port
    let (userinfo, hostport) = decoded.split_once('@').context("legacy ss URI missing @ separator")?;
    let (method, password) = userinfo
        .split_once(':')
        .context("legacy ss userinfo must be method:password")?;
    let (host_str, port_str) = hostport
        .split_once(':')
        .map(|(h, p)| (h, Some(p)))
        .unwrap_or((hostport, None));
    let port: u16 = port_str.and_then(|p| p.parse().ok()).unwrap_or(443);

    let name = match fragment {
        Some(frag) => {
            let decoded = percent_decode_str(frag).decode_utf8_lossy();
            let trimmed = decoded.trim();
            if !trimmed.is_empty() {
                trimmed.to_string()
            } else {
                format!("{host_str}:{port}")
            }
        }
        None => format!("{host_str}:{port}"),
    };

    let mut proxy = Mapping::new();
    proxy.insert("name".into(), Value::String(name));
    proxy.insert("type".into(), Value::String("ss".into()));
    proxy.insert("server".into(), Value::String(host_str.to_owned()));
    proxy.insert("port".into(), Value::Number(port.into()));
    proxy.insert("cipher".into(), Value::String(method.to_owned()));
    proxy.insert("password".into(), Value::String(password.to_owned()));
    proxy.insert("udp".into(), Value::Bool(true));

    Ok(proxy)
}

// ---------------------------------------------------------------------------
// vmess://base64(json)
// ---------------------------------------------------------------------------

fn parse_vmess_uri(uri: &str) -> anyhow::Result<Mapping> {
    let encoded = uri.strip_prefix("vmess://").context("not a vmess URI")?;
    let json_bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(encoded))
        .context("failed to base64-decode vmess URI")?;

    let json: serde_json::Value = serde_json::from_slice(&json_bytes).context("vmess JSON is invalid")?;

    let server = json["add"].as_str().context("vmess: missing 'add' field")?.to_owned();
    let port: u16 = json["port"]
        .as_str()
        .and_then(|p| p.parse().ok())
        .or_else(|| json["port"].as_u64().map(|u| u as u16))
        .unwrap_or(443);
    let uuid = json["id"].as_str().unwrap_or("").to_owned();
    let aid: i64 = json["aid"]
        .as_str()
        .and_then(|v| v.parse().ok())
        .or_else(|| json["aid"].as_i64())
        .unwrap_or(0);
    let ps = json["ps"].as_str().unwrap_or("");
    let name = if ps.is_empty() {
        format!("{server}:{port}")
    } else {
        ps.to_owned()
    };
    let network = json["net"].as_str().unwrap_or("tcp").to_owned();
    let tls = json["tls"].as_str().unwrap_or("") == "tls";
    let host = json["host"].as_str().unwrap_or("").to_owned();
    let path = json["path"].as_str().unwrap_or("").to_owned();
    let sni = json["sni"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(|s| s.to_owned())
        .unwrap_or_else(|| host.clone());
    let fp = json["fp"].as_str().unwrap_or("").to_owned();

    let mut proxy = Mapping::new();
    proxy.insert("name".into(), Value::String(name));
    proxy.insert("type".into(), Value::String("vmess".into()));
    proxy.insert("server".into(), Value::String(server));
    proxy.insert("port".into(), Value::Number(port.into()));
    proxy.insert("uuid".into(), Value::String(uuid));
    proxy.insert("alterId".into(), Value::Number(aid.into()));
    proxy.insert("cipher".into(), Value::String("auto".into()));
    proxy.insert("udp".into(), Value::Bool(true));

    if tls {
        proxy.insert("tls".into(), Value::Bool(true));
        insert_str(&mut proxy, "sni", &sni);
        insert_str(&mut proxy, "servername", &sni);
    }

    if network != "tcp" {
        insert_str(&mut proxy, "network", &network);
    }

    match network.as_str() {
        "ws" => {
            let mut opts = Mapping::new();
            let ws_path = if path.is_empty() { "/" } else { &path };
            opts.insert("path".into(), Value::String(ws_path.to_owned()));
            if !host.is_empty() {
                let mut headers = Mapping::new();
                headers.insert("Host".into(), Value::String(host));
                opts.insert("headers".into(), Value::Mapping(headers));
            }
            proxy.insert("ws-opts".into(), Value::Mapping(opts));
        }
        "h2" => {
            let mut opts = Mapping::new();
            if !host.is_empty() {
                opts.insert("host".into(), Value::String(host));
            }
            if !path.is_empty() {
                opts.insert("path".into(), Value::String(path));
            }
            proxy.insert("h2-opts".into(), Value::Mapping(opts));
        }
        "grpc" => {
            let mut opts = Mapping::new();
            if !path.is_empty() {
                opts.insert("grpc-service".into(), Value::String(path));
            }
            proxy.insert("grpc-opts".into(), Value::Mapping(opts));
        }
        _ => {}
    }

    insert_str(&mut proxy, "client-fingerprint", &fp);

    if let Some(allow_insecure) = json["allowInsecure"].as_str()
        && (allow_insecure == "1" || allow_insecure == "true")
    {
        proxy.insert("skip-cert-verify".into(), Value::Bool(true));
    }

    Ok(proxy)
}

// ---------------------------------------------------------------------------
// hysteria://host:port?query#name
// ---------------------------------------------------------------------------

fn parse_hysteria_uri(uri: &str) -> anyhow::Result<Mapping> {
    let url = Url::parse(uri).context("invalid hysteria URI")?;
    let server = url.host_str().context("missing host")?.to_owned();
    let port = url.port().unwrap_or(443);
    let name = extract_name(&url, &server, port);

    let mut proxy = Mapping::new();
    proxy.insert("name".into(), Value::String(name));
    proxy.insert("type".into(), Value::String("hysteria".into()));
    proxy.insert("server".into(), Value::String(server.clone()));
    proxy.insert("port".into(), Value::Number(port.into()));

    // Password: username from URL or query "auth"
    let password = if url.username().is_empty() {
        qs_owned(&url, "auth").unwrap_or_default()
    } else {
        url.username().to_owned()
    };
    if !password.is_empty() {
        insert_str(&mut proxy, "auth", &password);
        insert_str(&mut proxy, "auth-str", &password);
    }

    // ports / mport
    if let Some(ports) = qs_owned(&url, "ports").or_else(|| qs_owned(&url, "mport")) {
        insert_str(&mut proxy, "ports", &ports);
    }

    // speed
    insert_str(&mut proxy, "up", qs_owned(&url, "up").unwrap_or_default().as_str());
    insert_str(&mut proxy, "down", qs_owned(&url, "down").unwrap_or_default().as_str());

    // sni
    let sni = qs_owned(&url, "peer")
        .or_else(|| qs_owned(&url, "sni"))
        .unwrap_or_else(|| server.clone());
    insert_str(&mut proxy, "sni", &sni);

    if qs_bool(&url, "insecure", false) {
        proxy.insert("skip-cert-verify".into(), Value::Bool(true));
    }

    // alpn
    if let Some(alpn) = qs_owned(&url, "alpn") {
        let alpn_list: Vec<Value> = alpn.split(',').map(|s| Value::String(s.to_owned())).collect();
        proxy.insert("alpn".into(), Value::Sequence(alpn_list));
    }

    // protocol
    insert_str(
        &mut proxy,
        "protocol",
        qs_owned(&url, "protocol").unwrap_or_default().as_str(),
    );

    // obfs
    insert_str(&mut proxy, "obfs", qs_owned(&url, "obfs").unwrap_or_default().as_str());
    let obfs_password = qs_owned(&url, "obfs-password").or_else(|| qs_owned(&url, "obfsParam"));
    insert_str(&mut proxy, "obfs-password", obfs_password.unwrap_or_default().as_str());

    // quic tunables
    if let Some(v) = qs_i64(&url, "recv_window_conn") {
        insert_i64(&mut proxy, "recv-window-conn", v);
    }
    if let Some(v) = qs_i64(&url, "recv_window") {
        insert_i64(&mut proxy, "recv-window", v);
    }
    if qs_bool(&url, "disable_mtu_discovery", false) {
        proxy.insert("disable-mtu-discovery".into(), Value::Bool(true));
    }
    if qs_bool(&url, "fast_open", false) {
        proxy.insert("fast-open".into(), Value::Bool(true));
    }

    // fingerprint (pinSHA256)
    insert_str(
        &mut proxy,
        "fingerprint",
        qs_owned(&url, "pinSHA256").unwrap_or_default().as_str(),
    );

    Ok(proxy)
}

// ---------------------------------------------------------------------------
// hysteria2://password@host:port?query#name
// ---------------------------------------------------------------------------

fn parse_hysteria2_uri(uri: &str) -> anyhow::Result<Mapping> {
    let url = Url::parse(uri).context("invalid hysteria2 URI")?;
    let server = url.host_str().context("missing host")?.to_owned();
    let port = url.port().unwrap_or(443);
    let name = extract_name(&url, &server, port);

    // Password: username from URL or query "auth"
    let password = if !url.username().is_empty() {
        url.username().to_owned()
    } else {
        qs_owned(&url, "auth").unwrap_or_default()
    };
    if password.is_empty() {
        bail!("hysteria2 URI missing password");
    }

    let mut proxy = Mapping::new();
    proxy.insert("name".into(), Value::String(name));
    proxy.insert("type".into(), Value::String("hysteria2".into()));
    proxy.insert("server".into(), Value::String(server));
    proxy.insert("port".into(), Value::Number(port.into()));
    proxy.insert("password".into(), Value::String(password));

    // sni
    if let Some(sni) = qs_owned(&url, "sni") {
        insert_str(&mut proxy, "sni", &sni);
    }

    if qs_bool(&url, "insecure", false) {
        proxy.insert("skip-cert-verify".into(), Value::Bool(true));
    }

    // speed
    insert_str(&mut proxy, "up", qs_owned(&url, "up").unwrap_or_default().as_str());
    insert_str(&mut proxy, "down", qs_owned(&url, "down").unwrap_or_default().as_str());

    // obfs
    insert_str(&mut proxy, "obfs", qs_owned(&url, "obfs").unwrap_or_default().as_str());
    insert_str(
        &mut proxy,
        "obfs-password",
        qs_owned(&url, "obfs-password").unwrap_or_default().as_str(),
    );

    // pinSHA256
    insert_str(
        &mut proxy,
        "pinSHA256",
        qs_owned(&url, "pinSHA256").unwrap_or_default().as_str(),
    );

    // ca / caStr
    insert_str(&mut proxy, "ca", qs_owned(&url, "ca").unwrap_or_default().as_str());
    insert_str(
        &mut proxy,
        "ca-str",
        qs_owned(&url, "caStr").unwrap_or_default().as_str(),
    );

    // quic tunables
    if let Some(v) = qs_i64(&url, "recv_window_conn") {
        insert_i64(&mut proxy, "recv-window-conn", v);
    }
    if let Some(v) = qs_i64(&url, "recv_window") {
        insert_i64(&mut proxy, "recv-window", v);
    }
    if qs_bool(&url, "disable_mtu_discovery", false) {
        proxy.insert("disable-mtu-discovery".into(), Value::Bool(true));
    }
    if qs_bool(&url, "fast_open", false) {
        proxy.insert("fast-open".into(), Value::Bool(true));
    }
    if let Some(v) = qs_i64(&url, "cwnd") {
        insert_i64(&mut proxy, "cwnd", v);
    }
    if !qs_bool(&url, "udp", true) {
        proxy.insert("udp".into(), Value::Bool(false));
    }

    Ok(proxy)
}

// ---------------------------------------------------------------------------
// tuic://uuid:password@host:port?query#name
// ---------------------------------------------------------------------------

fn parse_tuic_uri(uri: &str) -> anyhow::Result<Mapping> {
    let url = Url::parse(uri).context("invalid tuic URI")?;
    let server = url.host_str().context("missing host")?.to_owned();
    let port = url.port().unwrap_or(443);
    let name = extract_name(&url, &server, port);

    // Userinfo is uuid:password — use url.password() which properly
    // separates on ':' for all schemes.
    let uuid = url.username().to_owned();
    let password = url
        .password()
        .map(|p| p.to_owned())
        .or_else(|| qs_owned(&url, "password"))
        .unwrap_or_default();

    let sni = qs_owned(&url, "sni")
        .or_else(|| qs_owned(&url, "peer"))
        .unwrap_or_else(|| server.clone());

    let mut proxy = Mapping::new();
    proxy.insert("name".into(), Value::String(name));
    proxy.insert("type".into(), Value::String("tuic".into()));
    proxy.insert("server".into(), Value::String(server));
    proxy.insert("port".into(), Value::Number(port.into()));
    proxy.insert("uuid".into(), Value::String(uuid));
    proxy.insert("password".into(), Value::String(password));
    proxy.insert("sni".into(), Value::String(sni));
    proxy.insert("udp".into(), Value::Bool(true));

    if qs_bool(&url, "allowInsecure", false) {
        proxy.insert("skip-cert-verify".into(), Value::Bool(true));
    }

    if let Some(alpn) = qs_owned(&url, "alpn") {
        let alpn_list: Vec<Value> = alpn.split(',').map(|s| Value::String(s.to_owned())).collect();
        proxy.insert("alpn".into(), Value::Sequence(alpn_list));
    }

    insert_str(
        &mut proxy,
        "congestion-controller",
        qs_owned(&url, "congestion_control").unwrap_or_default().as_str(),
    );
    insert_str(
        &mut proxy,
        "udp-relay-mode",
        qs_owned(&url, "udp_relay_mode").unwrap_or_default().as_str(),
    );

    if let Some(v) = qs_owned(&url, "heartbeat") {
        insert_str(&mut proxy, "heartbeat", &v);
    }

    if qs_bool(&url, "disable_sni", false) {
        proxy.insert("disable-sni".into(), Value::Bool(true));
    }
    if qs_bool(&url, "reduce_rtt", false) {
        proxy.insert("reduce-rtt".into(), Value::Bool(true));
    }
    if qs_bool(&url, "fast_open", false) {
        proxy.insert("fast-open".into(), Value::Bool(true));
    }

    if let Some(v) = qs_i64(&url, "max_open_streams") {
        insert_i64(&mut proxy, "max-open-streams", v);
    }
    if let Some(v) = qs_i64(&url, "max_udp_relay_packet_size") {
        insert_i64(&mut proxy, "max-udp-relay-packet-size", v);
    }
    if let Some(v) = qs_i64(&url, "max_datagram_frame_size") {
        insert_i64(&mut proxy, "max-datagram-frame-size", v);
    }
    if let Some(v) = qs_i64(&url, "request_timeout") {
        insert_i64(&mut proxy, "request-timeout", v);
    }

    Ok(proxy)
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- detection --------------------------------------------------------

    #[test]
    fn detect_non_empty_uri_list() {
        let input = "trojan://pass@1.2.3.4:443#Node1\nvless://uuid@5.6.7.8:443#Node2\n";
        assert!(detect_proxy_uri_list(input));
    }

    #[test]
    fn detect_non_matching_text() {
        assert!(!detect_proxy_uri_list("proxies:\n  - name: foo\n"));
        assert!(!detect_proxy_uri_list("<html><body>error</body></html>"));
    }

    #[test]
    fn detect_empty_string() {
        assert!(!detect_proxy_uri_list(""));
    }

    #[test]
    fn detect_mostly_non_uri_lines() {
        // 1 URI + 2 blank lines + 3 junk lines = minority
        let input = "# comment1\n# comment2\n\nnot a uri\njunk text\ntrojan://p@h:1#ok\n";
        assert!(!detect_proxy_uri_list(input));
    }

    // -- trojan -----------------------------------------------------------

    #[test]
    fn parse_trojan_uri_full() {
        let uri = "trojan://password@example.com:443?type=ws&path=%2Fws&host=cdn.example.com&sni=cdn.example.com&allowInsecure=1#MyNode";
        let proxy = parse_trojan_uri(uri).unwrap();
        assert_eq!(proxy["name"].as_str(), Some("MyNode"));
        assert_eq!(proxy["type"].as_str(), Some("trojan"));
        assert_eq!(proxy["server"].as_str(), Some("example.com"));
        assert_eq!(proxy["port"].as_u64(), Some(443));
        assert_eq!(proxy["password"].as_str(), Some("password"));
        assert_eq!(proxy["sni"].as_str(), Some("cdn.example.com"));
        assert_eq!(proxy["network"].as_str(), Some("ws"));
        assert_eq!(proxy["skip-cert-verify"].as_bool(), Some(true));
        let ws = proxy["ws-opts"].as_mapping().unwrap();
        assert_eq!(ws.get("path").and_then(|v| v.as_str()), Some("/ws"));
    }

    #[test]
    fn parse_trojan_uri_minimal() {
        let uri = "trojan://pw@10.0.0.1:443";
        let proxy = parse_trojan_uri(uri).unwrap();
        assert_eq!(proxy["name"].as_str(), Some("10.0.0.1:443"));
        assert_eq!(proxy["type"].as_str(), Some("trojan"));
        assert_eq!(proxy["server"].as_str(), Some("10.0.0.1"));
        assert!(!proxy.contains_key("skip-cert-verify"));
    }

    // -- vless ------------------------------------------------------------

    #[test]
    fn test_parse_vless_uri() {
        let uri = "vless://my-uuid@example.com:8443?type=reality&pbk=pubkey&sid=abcd&sni=example.com&fp=chrome#VLNode";
        let proxy = parse_vless_uri(uri).unwrap();
        assert_eq!(proxy["name"].as_str(), Some("VLNode"));
        assert_eq!(proxy["type"].as_str(), Some("vless"));
        assert_eq!(proxy["server"].as_str(), Some("example.com"));
        assert_eq!(proxy["uuid"].as_str(), Some("my-uuid"));
        assert_eq!(proxy["network"].as_str(), Some("tcp"));
        assert_eq!(proxy["vl"].as_bool(), Some(true));
        let reality = proxy["reality-opts"].as_mapping().unwrap();
        assert_eq!(reality.get("public-key").and_then(|v| v.as_str()), Some("pubkey"));
        assert_eq!(reality.get("short-id").and_then(|v| v.as_str()), Some("abcd"));
    }

    // -- ss ---------------------------------------------------------------

    #[test]
    fn parse_ss_sip002() {
        let method_pass = "aes-256-gcm:my-password";
        let encoded = base64::engine::general_purpose::STANDARD.encode(method_pass);
        let uri = format!("ss://{encoded}@1.2.3.4:8388#SSNode");
        let proxy = parse_ss_uri(&uri).unwrap();
        assert_eq!(proxy["name"].as_str(), Some("SSNode"));
        assert_eq!(proxy["type"].as_str(), Some("ss"));
        assert_eq!(proxy["server"].as_str(), Some("1.2.3.4"));
        assert_eq!(proxy["port"].as_u64(), Some(8388));
        assert_eq!(proxy["cipher"].as_str(), Some("aes-256-gcm"));
        assert_eq!(proxy["password"].as_str(), Some("my-password"));
    }

    #[test]
    fn parse_ss_legacy() {
        let full = "chacha20-ietf-poly1305:pass123@10.0.0.1:8388";
        let encoded = base64::engine::general_purpose::STANDARD.encode(full);
        let uri = format!("ss://{encoded}#LegacyNode");
        let proxy = parse_ss_uri(&uri).unwrap();
        assert_eq!(proxy["name"].as_str(), Some("LegacyNode"));
        assert_eq!(proxy["server"].as_str(), Some("10.0.0.1"));
        assert_eq!(proxy["cipher"].as_str(), Some("chacha20-ietf-poly1305"));
        assert_eq!(proxy["password"].as_str(), Some("pass123"));
    }

    // -- vmess ------------------------------------------------------------

    #[test]
    fn test_parse_vmess_uri() {
        let json = serde_json::json!({
            "v": "2",
            "ps": "VMessNode",
            "add": "1.2.3.4",
            "port": "443",
            "id": "my-uuid",
            "aid": "0",
            "net": "ws",
            "type": "none",
            "host": "cdn.example.com",
            "path": "/ws",
            "tls": "tls",
            "sni": "cdn.example.com",
            "fp": "chrome"
        });
        let encoded = base64::engine::general_purpose::STANDARD.encode(json.to_string());
        let uri = format!("vmess://{encoded}");
        let proxy = parse_vmess_uri(&uri).unwrap();
        assert_eq!(proxy["name"].as_str(), Some("VMessNode"));
        assert_eq!(proxy["type"].as_str(), Some("vmess"));
        assert_eq!(proxy["server"].as_str(), Some("1.2.3.4"));
        assert_eq!(proxy["port"].as_u64(), Some(443));
        assert_eq!(proxy["uuid"].as_str(), Some("my-uuid"));
        assert!(proxy["tls"].as_bool().unwrap());
        let ws = proxy["ws-opts"].as_mapping().unwrap();
        assert_eq!(ws.get("path").and_then(|v| v.as_str()), Some("/ws"));
    }

    // -- hysteria2 --------------------------------------------------------

    #[test]
    fn test_parse_hysteria2_uri() {
        let uri = "hysteria2://letmein@example.com:8443?sni=example.com&insecure=1&up=100&down=200&obfs=salamander&obfs-password=obfspass#HY2";
        let proxy = parse_hysteria2_uri(uri).unwrap();
        assert_eq!(proxy["name"].as_str(), Some("HY2"));
        assert_eq!(proxy["type"].as_str(), Some("hysteria2"));
        assert_eq!(proxy["server"].as_str(), Some("example.com"));
        assert_eq!(proxy["password"].as_str(), Some("letmein"));
        assert_eq!(proxy["sni"].as_str(), Some("example.com"));
        assert!(proxy["skip-cert-verify"].as_bool().unwrap());
        assert_eq!(proxy["obfs"].as_str(), Some("salamander"));
        assert_eq!(proxy["obfs-password"].as_str(), Some("obfspass"));
    }

    // -- tuic -------------------------------------------------------------

    #[test]
    fn test_parse_tuic_uri() {
        let uri = "tuic://uuid-foo:pw-bar@example.com:443?sni=example.com&alpn=h3&congestion_control=bbr#TuicNode";
        let proxy = parse_tuic_uri(uri).unwrap();
        assert_eq!(proxy["name"].as_str(), Some("TuicNode"));
        assert_eq!(proxy["type"].as_str(), Some("tuic"));
        assert_eq!(proxy["server"].as_str(), Some("example.com"));
        assert_eq!(proxy["uuid"].as_str(), Some("uuid-foo"));
        assert_eq!(proxy["password"].as_str(), Some("pw-bar"));
        assert_eq!(proxy["congestion-controller"].as_str(), Some("bbr"));
    }

    // -- parse_proxy_uri_list integration ---------------------------------

    #[test]
    fn parse_mixed_lines_skips_unsupported_and_broken() {
        let input = concat!(
            "trojan://pw@1.2.3.4:443#T\n",
            "http://user@1.2.3.4:80#Skipped\n", // unsupported scheme
            "not-a-uri\n",                      // no ://
            "vless://uid@5.6.7.8:443#V\n",
        );
        let root = parse_proxy_uri_list(input).unwrap();
        let proxies = root["proxies"].as_sequence().unwrap();
        assert_eq!(proxies.len(), 2);

        let names: Vec<&str> = proxies.iter().filter_map(|p| p["name"].as_str()).collect();
        assert_eq!(names, vec!["T", "V"]);
    }

    #[test]
    fn parse_all_invalid_returns_error() {
        assert!(parse_proxy_uri_list("http://user@1.2.3.4:80#Nope\nsocks://1.2.3.4:1080\n").is_err());
    }

    #[test]
    fn generated_mapping_has_proxies_and_proxy_groups() {
        let input = "trojan://pw@1.2.3.4:443#T\nvless://uid@5.6.7.8:443#V\n";
        let root = parse_proxy_uri_list(input).unwrap();
        assert!(root.contains_key("proxies"));
        assert!(root.contains_key("proxy-groups"));

        let groups = root["proxy-groups"].as_sequence().unwrap();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0]["name"].as_str(), Some("Proxy"));
        assert_eq!(groups[0]["type"].as_str(), Some("select"));

        let group_proxies = groups[0]["proxies"].as_sequence().unwrap();
        let group_names: Vec<&str> = group_proxies.iter().filter_map(|v| v.as_str()).collect();
        assert_eq!(group_names, vec!["T", "V"]);
    }

    #[test]
    fn proxy_name_falls_back_to_server_port() {
        let uri = "trojan://pw@10.0.0.1:8080";
        let url = Url::parse(uri).unwrap();
        assert_eq!(extract_name(&url, "10.0.0.1", 8080), "10.0.0.1:8080");
    }

    #[test]
    fn qs_bool_values() {
        let url = Url::parse("trojan://pw@h:1?a=1&b=0&c=true&d=false&e=yes").unwrap();
        assert!(qs_bool(&url, "a", false));
        assert!(!qs_bool(&url, "b", true));
        assert!(qs_bool(&url, "c", false));
        assert!(!qs_bool(&url, "d", true));
        // "yes" is not a recognised bool → returns default
        assert!(qs_bool(&url, "e", true));
        // missing key returns default
        assert!(qs_bool(&url, "missing", true));
        assert!(!qs_bool(&url, "missing", false));
    }
}
