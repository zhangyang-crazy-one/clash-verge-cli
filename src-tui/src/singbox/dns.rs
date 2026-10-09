//! Structured sing-box DNS configuration (task 8.1, design D8).
//!
//! **1.12+ new format only** — the legacy `address`/`address_resolver`
//! syntax is never generated (F4: deprecated in 1.12, removed in 1.14;
//! target core is pinned to stable 1.14.2). Fields outside the structured form go
//! through the raw JSON editor (design D7), not here.
//!
//! Covered surface: multiple servers (`udp`/`tls`/`https`/`quic`/`local`/
//! `fakeip`), per-server `detour`, domain/IP split rules, fakeip ranges and
//! A/AAAA routing rules, and the `domain_resolver`
//! bootstrap attached to remote servers whose address is a domain.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use serde_yaml_ng::Value as Yaml;
use std::hash::{Hash as _, Hasher as _};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DnsServerKind {
    Udp,
    Tls,
    Https,
    Quic,
    Local,
    Fakeip,
}

impl DnsServerKind {
    pub fn parse(kind: &str) -> Option<Self> {
        Some(match kind {
            "udp" => Self::Udp,
            "tls" => Self::Tls,
            "https" => Self::Https,
            "quic" => Self::Quic,
            "local" => Self::Local,
            "fakeip" => Self::Fakeip,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Udp => "udp",
            Self::Tls => "tls",
            Self::Https => "https",
            Self::Quic => "quic",
            Self::Local => "local",
            Self::Fakeip => "fakeip",
        }
    }

    /// Remote server kinds carry an upstream address; local/fakeip do not.
    pub fn is_remote(self) -> bool {
        matches!(self, Self::Udp | Self::Tls | Self::Https | Self::Quic)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DnsServerSpec {
    pub tag: String,
    pub kind: DnsServerKind,
    /// Upstream address (host or IP). Required for remote kinds, rejected
    /// for local/fakeip.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_port: Option<u16>,
    /// HTTPS DNS endpoint path (sing-box defaults to `/dns-query`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Outbound tag the DNS queries leave through.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detour: Option<String>,
    /// fakeip pool ranges (fakeip kind only; sing-box defaults kept explicit).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inet4_range: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inet6_range: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DnsRuleSpec {
    /// Server tag the matched queries are routed to.
    pub server: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub query_type: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub domain_suffix: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub domain_keyword: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ip_cidr: Vec<String>,
}

impl DnsRuleSpec {
    fn is_empty(&self) -> bool {
        self.query_type.is_empty()
            && self.domain_suffix.is_empty()
            && self.domain_keyword.is_empty()
            && self.ip_cidr.is_empty()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DnsConfigSpec {
    pub servers: Vec<DnsServerSpec>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rules: Vec<DnsRuleSpec>,
    /// Bootstrap server tag written onto remote servers addressed by domain
    /// and as `route.default_domain_resolver` for outbound name resolution.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domain_resolver: Option<String>,
}

const DEFAULT_FAKEIP_V4: &str = "198.18.0.0/15";
const DEFAULT_FAKEIP_V6: &str = "fc00::/18";

impl DnsConfigSpec {
    /// An empty spec generates no DNS section at all — the default install
    /// keeps its skeleton behavior until the user configures something.
    pub fn is_empty(&self) -> bool {
        self.servers.is_empty() && self.rules.is_empty()
    }

    pub fn validate(&self) -> Result<(), String> {
        let mut tags = std::collections::HashSet::new();
        for server in &self.servers {
            if server.tag.is_empty() || !tags.insert(server.tag.clone()) {
                return Err(format!("invalid or duplicate dns server tag: {:?}", server.tag));
            }
            match server.kind {
                DnsServerKind::Local | DnsServerKind::Fakeip => {
                    // No upstream address; ignore a stray one instead of failing.
                }
                remote => {
                    if server.server.as_deref().unwrap_or("").is_empty() {
                        return Err(format!(
                            "dns server {:?} ({}) requires an address",
                            server.tag,
                            remote.as_str()
                        ));
                    }
                }
            }
            if server.path.is_some() && server.kind != DnsServerKind::Https {
                return Err(format!("dns server {:?}: path is supported only for HTTPS", server.tag));
            }
        }
        if let Some(resolver) = &self.domain_resolver {
            let server = self
                .servers
                .iter()
                .find(|server| &server.tag == resolver)
                .ok_or_else(|| format!("unknown DNS bootstrap resolver {resolver:?}"))?;
            if server.kind == DnsServerKind::Fakeip {
                return Err("a fake-IP server cannot be a DNS bootstrap resolver".into());
            }
        }
        for rule in &self.rules {
            if rule.is_empty() {
                return Err(format!("dns rule for {:?} has no match conditions", rule.server));
            }
            if !tags.contains(&rule.server) {
                return Err(format!("dns rule targets unknown server {:?}", rule.server));
            }
        }
        Ok(())
    }
}

/// Convert the profile's explicitly configured Clash DNS subset into the
/// typed sing-box 1.14 DNS model. `None` means the profile has no `dns`
/// block, so callers retain the user's persisted sidecar settings.
/// Unsupported policy fields fail with their key path instead of silently
/// changing resolver choice or fake-IP behavior.
pub fn dns_spec_from_clash_yaml(yaml: &str) -> Result<Option<DnsConfigSpec>, String> {
    let root: Yaml = serde_yaml_ng::from_str(yaml).map_err(|error| format!("invalid profile YAML: {error}"))?;
    let Some(root) = root.as_mapping() else {
        return Err("profile YAML root is not a mapping".into());
    };
    let Some(dns_value) = root.get(Yaml::String("dns".into())) else {
        return Ok(None);
    };
    let Some(dns) = dns_value.as_mapping() else {
        return Err("dns must be a mapping".into());
    };
    let get = |key: &str| dns.get(Yaml::String(key.into()));
    if get("enable").and_then(Yaml::as_bool) == Some(false) {
        return Err("dns.enable: disabling DNS has no safe sing-box sidecar representation".into());
    }
    if get("enable").is_some() && get("enable").and_then(Yaml::as_bool).is_none() {
        return Err("dns.enable must be a boolean".into());
    }
    if get("ipv6").and_then(Yaml::as_bool) == Some(false) {
        return Err("dns.ipv6: disabling IPv6 DNS has no reviewed sing-box mapping".into());
    }
    if get("ipv6").is_some() && get("ipv6").and_then(Yaml::as_bool).is_none() {
        return Err("dns.ipv6 must be a boolean".into());
    }
    if get("enhanced-mode").is_some() && get("enhanced-mode").and_then(Yaml::as_str).is_none() {
        return Err("dns.enhanced-mode must be a string".into());
    }
    for key in ["fake-ip-range", "fake-ip-range6"] {
        if get(key).is_some() && get(key).and_then(Yaml::as_str).is_none() {
            return Err(format!("dns.{key} must be a CIDR string"));
        }
    }
    for key in [
        "fallback",
        "fallback-filter",
        "default-nameserver",
        "proxy-server-nameserver",
        "listen",
        "respect-rules",
        "fake-ip-filter-mode",
    ] {
        if get(key).is_some_and(|value| !value.is_null() && !value.is_sequence() && !value.is_mapping())
            || get(key).is_some_and(|value| value.as_sequence().is_some_and(|values| !values.is_empty()))
            || get(key).is_some_and(|value| value.as_mapping().is_some_and(|values| !values.is_empty()))
        {
            return Err(format!(
                "dns.{key}: this DNS policy is not represented by the typed sing-box settings"
            ));
        }
    }
    if get("fake-ip-filter").is_some_and(|value| value.as_sequence().is_none_or(|values| !values.is_empty())) {
        return Err(
            "dns.fake-ip-filter: filtering fake-IP domains is not represented; refusing to change address semantics"
                .into(),
        );
    }
    for key in dns.keys().filter_map(Yaml::as_str) {
        if !matches!(
            key,
            "enable"
                | "ipv6"
                | "enhanced-mode"
                | "fake-ip-range"
                | "fake-ip-range6"
                | "fake-ip-filter"
                | "fake-ip-filter-mode"
                | "nameserver"
                | "nameserver-policy"
                | "fallback"
                | "fallback-filter"
                | "default-nameserver"
                | "proxy-server-nameserver"
                | "listen"
                | "respect-rules"
        ) {
            return Err(format!(
                "dns.{key}: unsupported profile DNS field; refusing to silently discard it"
            ));
        }
    }

    let mut spec = DnsConfigSpec::default();
    let mut endpoint_tags = std::collections::HashMap::<String, String>::new();
    if let Some(nameservers) = get("nameserver") {
        let endpoints = if let Some(sequence) = nameservers.as_sequence() {
            sequence.iter().collect::<Vec<_>>()
        } else {
            vec![nameservers]
        };
        for endpoint in endpoints {
            let endpoint = endpoint
                .as_str()
                .ok_or_else(|| "dns.nameserver entries must be strings".to_string())?;
            let tag = add_clash_dns_endpoint(endpoint, &mut spec, &mut endpoint_tags)?;
            if spec.domain_resolver.is_none()
                && spec
                    .servers
                    .iter()
                    .find(|server| server.tag == tag)
                    .and_then(|server| server.server.as_deref())
                    .is_some_and(looks_like_ip)
            {
                spec.domain_resolver = Some(tag);
            }
        }
    }
    if let Some(policy) = get("nameserver-policy") {
        let Some(policy) = policy.as_mapping() else {
            return Err("dns.nameserver-policy must be a mapping".into());
        };
        for (pattern, endpoints) in policy {
            let pattern = pattern
                .as_str()
                .ok_or_else(|| "dns.nameserver-policy keys must be strings".to_string())?;
            let values = if let Some(list) = endpoints.as_sequence() {
                if list.len() != 1 {
                    return Err(format!(
                        "dns.nameserver-policy.{pattern}: exactly one server is supported"
                    ));
                }
                list[0]
                    .as_str()
                    .ok_or_else(|| format!("dns.nameserver-policy.{pattern} must contain a server string"))?
            } else {
                endpoints.as_str().ok_or_else(|| {
                    format!("dns.nameserver-policy.{pattern} must be a server string or one-item list")
                })?
            };
            let (field, value) = parse_clash_dns_policy_pattern(pattern)?;
            let server = add_clash_dns_endpoint(values, &mut spec, &mut endpoint_tags)?;
            let mut rule = DnsRuleSpec {
                server,
                query_type: Vec::new(),
                domain_suffix: Vec::new(),
                domain_keyword: Vec::new(),
                ip_cidr: Vec::new(),
            };
            match field {
                "domain_suffix" => rule.domain_suffix.push(value),
                "domain_keyword" => rule.domain_keyword.push(value),
                "ip_cidr" => rule.ip_cidr.push(value),
                _ => unreachable!(),
            }
            spec.rules.push(rule);
        }
    }
    match get("enhanced-mode").and_then(Yaml::as_str).unwrap_or("redir-host") {
        "redir-host" => {
            if get("fake-ip-range").is_some() || get("fake-ip-range6").is_some() {
                return Err("dns.fake-ip-range: only valid with enhanced-mode: fake-ip".into());
            }
        }
        "fake-ip" => {
            let inet4_range = get("fake-ip-range").and_then(Yaml::as_str).map(str::to_string);
            let inet6_range = get("fake-ip-range6").and_then(Yaml::as_str).map(str::to_string);
            for (field, range) in [
                ("fake-ip-range", inet4_range.as_deref()),
                ("fake-ip-range6", inet6_range.as_deref()),
            ] {
                if let Some(range) = range {
                    validate_cidr(range).map_err(|_| format!("dns.{field}: invalid CIDR {range:?}"))?;
                }
            }
            spec.servers.push(DnsServerSpec {
                tag: "profile-fakeip".into(),
                kind: DnsServerKind::Fakeip,
                server: None,
                server_port: None,
                path: None,
                detour: None,
                inet4_range,
                inet6_range,
            });
            spec.rules.insert(
                0,
                DnsRuleSpec {
                    server: "profile-fakeip".into(),
                    query_type: vec!["A".into(), "AAAA".into()],
                    domain_suffix: Vec::new(),
                    domain_keyword: Vec::new(),
                    ip_cidr: Vec::new(),
                },
            );
        }
        mode => return Err(format!("dns.enhanced-mode: unsupported value {mode:?}")),
    }
    if spec
        .servers
        .iter()
        .any(|server| server.kind.is_remote() && server.server.as_deref().is_some_and(|server| !looks_like_ip(server)))
        && spec.domain_resolver.is_none()
    {
        return Err(
            "dns.nameserver: hostname endpoints require an IP-literal nameserver as a bootstrap resolver".into(),
        );
    }
    spec.validate()?;
    Ok(Some(spec))
}

fn add_clash_dns_endpoint(
    endpoint: &str,
    spec: &mut DnsConfigSpec,
    tags: &mut std::collections::HashMap<String, String>,
) -> Result<String, String> {
    if endpoint == "system://" {
        let tag = "profile-system-dns".to_string();
        if !spec.servers.iter().any(|server| server.tag == tag) {
            spec.servers.push(DnsServerSpec {
                tag: tag.clone(),
                kind: DnsServerKind::Local,
                server: None,
                server_port: None,
                path: None,
                detour: None,
                inet4_range: None,
                inet6_range: None,
            });
        }
        tags.insert("local|system".into(), tag.clone());
        return Ok(tag);
    }
    let uri = if endpoint.contains("://") {
        endpoint.to_string()
    } else {
        format!("udp://{endpoint}")
    };
    let url = reqwest::Url::parse(&uri).map_err(|error| format!("unsupported dns.nameserver endpoint: {error}"))?;
    let kind = match url.scheme() {
        "udp" => DnsServerKind::Udp,
        "tls" => DnsServerKind::Tls,
        "https" => DnsServerKind::Https,
        "quic" => DnsServerKind::Quic,
        scheme => {
            return Err(format!("dns.nameserver endpoint uses unsupported scheme {scheme:?}"));
        }
    };
    if !url.username().is_empty() || url.password().is_some() || url.query().is_some() || url.fragment().is_some() {
        return Err("dns.nameserver endpoint contains unsupported credentials, query, or fragment".to_string());
    }
    let path = if kind == DnsServerKind::Https && url.path() == "/dns-query" {
        Some(url.path().to_string())
    } else if url.path().is_empty() || url.path() == "/" {
        None
    } else {
        return Err("dns.nameserver endpoint has an unsupported path (only HTTPS /dns-query is supported)".to_string());
    };
    let server = url
        .host_str()
        .ok_or_else(|| "dns.nameserver endpoint has no host".to_string())?
        .to_string();
    let server_port = url.port();
    let key = format!(
        "{}|{}|{}|{}",
        kind.as_str(),
        server,
        server_port.unwrap_or_default(),
        path.as_deref().unwrap_or("")
    );
    if let Some(tag) = tags.get(&key) {
        return Ok(tag.clone());
    }
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    key.hash(&mut hasher);
    let tag = format!("profile-dns-{:016x}", hasher.finish());
    spec.servers.push(DnsServerSpec {
        tag: tag.clone(),
        kind,
        server: Some(server),
        server_port,
        path,
        detour: None,
        inet4_range: None,
        inet6_range: None,
    });
    tags.insert(key, tag.clone());
    Ok(tag)
}

fn parse_clash_dns_policy_pattern(pattern: &str) -> Result<(&'static str, String), String> {
    if let Some(value) = pattern.strip_prefix("+.") {
        return nonempty_policy_value(pattern, "domain_suffix", value);
    }
    for (prefix, field) in [
        ("domain:", "domain_suffix"),
        ("keyword:", "domain_keyword"),
        ("ipcidr:", "ip_cidr"),
    ] {
        if let Some(value) = pattern.strip_prefix(prefix) {
            if field == "domain_suffix" {
                return Err(format!(
                    "dns.nameserver-policy.{pattern}: exact domain rules are unsupported"
                ));
            }
            if field == "ip_cidr" {
                validate_cidr(value).map_err(|_| format!("dns.nameserver-policy.{pattern}: invalid CIDR"))?;
            }
            return nonempty_policy_value(pattern, field, value);
        }
    }
    Err(format!(
        "dns.nameserver-policy.{pattern}: unsupported matcher; use +.domain, keyword:, or ipcidr:"
    ))
}

fn nonempty_policy_value(pattern: &str, field: &'static str, value: &str) -> Result<(&'static str, String), String> {
    if value.trim().is_empty() {
        Err(format!("dns.nameserver-policy.{pattern}: matcher value is empty"))
    } else {
        Ok((field, value.trim().to_string()))
    }
}

fn validate_cidr(value: &str) -> Result<(), ()> {
    let (address, prefix) = value.split_once('/').ok_or(())?;
    let address: std::net::IpAddr = address.parse().map_err(|_| ())?;
    let prefix: u8 = prefix.parse().map_err(|_| ())?;
    let max = if address.is_ipv4() { 32 } else { 128 };
    (prefix <= max).then_some(()).ok_or(())
}

fn looks_like_ip(server: &str) -> bool {
    server.parse::<std::net::IpAddr>().is_ok()
}

/// Build the sing-box `dns` section (1.12+ new format). Fails on specs the
/// core would reject (duplicate tags, rules pointing nowhere, remote servers
/// without an address) so callers can surface the message before a restart.
pub fn build_dns_section(spec: &DnsConfigSpec) -> Result<Option<Value>, String> {
    if spec.is_empty() {
        return Ok(None);
    }
    spec.validate()?;

    let mut servers = Vec::with_capacity(spec.servers.len());
    for s in &spec.servers {
        let mut entry = json!({ "type": s.kind.as_str(), "tag": s.tag });
        match s.kind {
            DnsServerKind::Udp | DnsServerKind::Tls | DnsServerKind::Https | DnsServerKind::Quic => {
                entry["server"] = json!(s.server.clone().unwrap_or_default());
                if let Some(port) = s.server_port {
                    entry["server_port"] = json!(port);
                }
                if let Some(path) = &s.path {
                    entry["path"] = json!(path);
                }
                if let (Some(detour), false) = (&s.detour, s.detour.as_deref().unwrap_or("").is_empty()) {
                    entry["detour"] = json!(detour);
                }
                // Bootstrap: a remote server reached by hostname must be told
                // which other server resolves that hostname (1.12+ behavior).
                let by_domain = s.server.as_deref().is_some_and(|addr| !looks_like_ip(addr));
                if by_domain
                    && let Some(resolver) = &spec.domain_resolver
                    && resolver != &s.tag
                {
                    entry["domain_resolver"] = json!(resolver);
                }
            }
            DnsServerKind::Local => {}
            DnsServerKind::Fakeip => {
                entry["inet4_range"] = json!(s.inet4_range.clone().unwrap_or_else(|| DEFAULT_FAKEIP_V4.into()));
                entry["inet6_range"] = json!(s.inet6_range.clone().unwrap_or_else(|| DEFAULT_FAKEIP_V6.into()));
            }
        }
        servers.push(entry);
    }

    let mut section = json!({ "servers": servers });
    let rules: Vec<Value> = spec
        .rules
        .iter()
        .map(|r| {
            let mut rule = json!({ "server": r.server });
            if !r.query_type.is_empty() {
                rule["query_type"] = json!(r.query_type);
            }
            if !r.domain_suffix.is_empty() {
                rule["domain_suffix"] = json!(r.domain_suffix);
            }
            if !r.domain_keyword.is_empty() {
                rule["domain_keyword"] = json!(r.domain_keyword);
            }
            if !r.ip_cidr.is_empty() {
                rule["ip_cidr"] = json!(r.ip_cidr);
            }
            rule
        })
        .collect();
    if !rules.is_empty() {
        section["rules"] = Value::Array(rules);
    }
    Ok(Some(section))
}

/// The `route.default_domain_resolver` companion value, or None.
pub fn default_domain_resolver(spec: &DnsConfigSpec) -> Option<String> {
    spec.domain_resolver.clone().filter(|tag| {
        spec.servers
            .iter()
            .any(|server| &server.tag == tag && server.kind != DnsServerKind::Fakeip)
    })
}

// ---------- Task 8.1: compact one-line form specs ----------

/// Parse a `kind|tag|server|port|detour` line from the DNS editor form.
///
/// Remote kinds (udp/tls/https/quic) require an address; local/fakeip take
/// none (fakeip pool ranges keep their defaults and stay editable through
/// the raw JSON path, design D7). Empty segments are skipped, so
/// `local|dns-local|||` is valid.
pub fn parse_server_spec(spec: &str) -> Result<DnsServerSpec, String> {
    let parts: Vec<&str> = spec.split('|').map(str::trim).collect();
    if parts.len() < 2 {
        return Err("expected kind|tag|server|port|detour".into());
    }
    let kind = DnsServerKind::parse(parts[0]).ok_or_else(|| {
        format!(
            "unknown dns server kind '{}' (use udp/tls/https/quic/local/fakeip)",
            parts[0]
        )
    })?;
    let tag = parts[1];
    if tag.is_empty() {
        return Err("empty server tag".into());
    }
    let mut server = DnsServerSpec {
        tag: tag.to_string(),
        kind,
        server: None,
        server_port: None,
        path: None,
        detour: None,
        inet4_range: None,
        inet6_range: None,
    };
    if kind.is_remote() {
        let addr = parts
            .get(2)
            .filter(|v| !v.is_empty())
            .ok_or_else(|| format!("{} server requires an address", kind.as_str()))?;
        server.server = Some(addr.to_string());
        if let Some(port) = parts.get(3).filter(|v| !v.is_empty()) {
            server.server_port = Some(port.parse().map_err(|_| format!("invalid port: {port}"))?);
        }
        if let Some(detour) = parts.get(4).filter(|v| !v.is_empty()) {
            server.detour = Some(detour.to_string());
        }
    }
    Ok(server)
}

/// Parse a `tag|suffix=a.com,b|keyword=x|cidr=10.0.0.0/8` split-rule line.
/// At least one condition segment is required.
pub fn parse_rule_spec(spec: &str) -> Result<DnsRuleSpec, String> {
    let parts: Vec<&str> = spec.split('|').map(str::trim).collect();
    if parts[0].is_empty() {
        return Err("expected tag|suffix=a,b|keyword=x|cidr=c".into());
    }
    let mut rule = DnsRuleSpec {
        server: parts[0].to_string(),
        query_type: Vec::new(),
        domain_suffix: Vec::new(),
        domain_keyword: Vec::new(),
        ip_cidr: Vec::new(),
    };
    for part in &parts[1..] {
        let Some((kind, values)) = part.split_once('=') else {
            return Err(format!("expected key=value segment, got '{part}'"));
        };
        let list: Vec<String> = values
            .split(',')
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(String::from)
            .collect();
        match kind.trim() {
            "suffix" => rule.domain_suffix = list,
            "keyword" => rule.domain_keyword = list,
            "cidr" => rule.ip_cidr = list,
            other => return Err(format!("unknown dns rule kind '{other}' (use suffix/keyword/cidr)")),
        }
    }
    if rule.is_empty() {
        return Err("dns rule needs at least one condition".into());
    }
    Ok(rule)
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;

    fn sample_spec() -> DnsConfigSpec {
        DnsConfigSpec {
            servers: vec![
                DnsServerSpec {
                    tag: "dns-remote".into(),
                    kind: DnsServerKind::Https,
                    server: Some("dns.google".into()),
                    server_port: Some(443),
                    path: None,
                    detour: Some("PROXY".into()),
                    inet4_range: None,
                    inet6_range: None,
                },
                DnsServerSpec {
                    tag: "dns-local".into(),
                    kind: DnsServerKind::Local,
                    server: None,
                    server_port: None,
                    path: None,
                    detour: None,
                    inet4_range: None,
                    inet6_range: None,
                },
                DnsServerSpec {
                    tag: "dns-fakeip".into(),
                    kind: DnsServerKind::Fakeip,
                    server: None,
                    server_port: None,
                    path: None,
                    detour: None,
                    inet4_range: None,
                    inet6_range: None,
                },
            ],
            rules: vec![DnsRuleSpec {
                server: "dns-local".into(),
                query_type: Vec::new(),
                domain_suffix: vec!["cn".into(), "example.com".into()],
                domain_keyword: vec![],
                ip_cidr: vec!["10.0.0.0/8".into()],
            }],
            domain_resolver: Some("dns-local".into()),
        }
    }

    #[test]
    fn maps_supported_profile_dns_endpoints_and_domain_policy() {
        let yaml = r#"
dns:
  enable: true
  ipv6: true
  enhanced-mode: fake-ip
  fake-ip-range: 198.18.0.1/16
  fake-ip-range6: fc00::1/18
  nameserver: [1.1.1.1:53, tls://8.8.8.8:853]
  nameserver-policy:
    +.internal.example: 1.1.1.1:53
    keyword:corp: tls://8.8.8.8:853
"#;
        let spec = dns_spec_from_clash_yaml(yaml).unwrap().unwrap();
        assert_eq!(spec.servers.len(), 3);
        assert_eq!(spec.servers[0].kind, DnsServerKind::Udp);
        assert_eq!(spec.servers[0].server.as_deref(), Some("1.1.1.1"));
        assert_eq!(spec.servers[0].server_port, Some(53));
        assert_eq!(spec.servers[1].kind, DnsServerKind::Tls);
        assert_eq!(spec.domain_resolver.as_deref(), Some(spec.servers[0].tag.as_str()));
        assert_eq!(
            default_domain_resolver(&spec).as_deref(),
            spec.domain_resolver.as_deref()
        );
        assert_eq!(spec.rules[0].server, "profile-fakeip");
        assert_eq!(spec.rules[0].query_type, ["A", "AAAA"]);
        assert_eq!(spec.rules[1].domain_suffix, ["internal.example"]);
        assert_eq!(spec.rules[2].domain_keyword, ["corp"]);
        let built = build_dns_section(&spec).unwrap().unwrap();
        assert_eq!(built["servers"][2]["type"], "fakeip");
        assert_eq!(built["servers"][2]["inet4_range"], "198.18.0.1/16");
        assert_eq!(built["servers"][2]["inet6_range"], "fc00::1/18");
        assert_eq!(built["rules"][0]["query_type"], json!(["A", "AAAA"]));
        assert_eq!(built["rules"][0]["server"], "profile-fakeip");
        assert_eq!(built["rules"][1]["domain_suffix"], json!(["internal.example"]));
    }

    #[test]
    fn fakeip_server_cannot_be_used_for_host_bootstrap_resolution() {
        let mut spec = DnsConfigSpec {
            servers: vec![DnsServerSpec {
                tag: "dns-fakeip".into(),
                kind: DnsServerKind::Fakeip,
                server: None,
                server_port: None,
                path: None,
                detour: None,
                inet4_range: None,
                inet6_range: None,
            }],
            rules: Vec::new(),
            domain_resolver: Some("dns-fakeip".into()),
        };

        assert!(spec.validate().unwrap_err().contains("fake-IP server"));
        assert!(build_dns_section(&spec).unwrap_err().contains("fake-IP server"));
        // This helper is used to populate route.default_domain_resolver; it
        // must never direct the host's bootstrap lookup into the fake pool.
        assert_eq!(default_domain_resolver(&spec), None);

        spec.domain_resolver = None;
        assert_eq!(default_domain_resolver(&spec), None);
    }

    #[test]
    fn absent_profile_dns_keeps_sidecar_and_unsupported_policy_is_rejected() {
        assert!(dns_spec_from_clash_yaml("proxies: []\n").unwrap().is_none());
        assert!(
            dns_spec_from_clash_yaml("dns:\n  nameserver: [1.1.1.1]\n  fallback: [8.8.8.8]\n")
                .unwrap_err()
                .contains("dns.fallback")
        );
        assert!(
            dns_spec_from_clash_yaml("dns:\n  nameserver: [https://doh.example/custom-path]\n")
                .unwrap_err()
                .contains("unsupported path")
        );
        let doh = dns_spec_from_clash_yaml("dns:\n  nameserver: [https://doh.example/dns-query, 1.1.1.1]\n")
            .unwrap()
            .unwrap();
        assert_eq!(doh.servers[0].path.as_deref(), Some("/dns-query"));
        assert_eq!(
            build_dns_section(&doh).unwrap().unwrap()["servers"][0]["path"],
            "/dns-query"
        );
        assert!(
            dns_spec_from_clash_yaml("dns:\n  enhanced-mode: fake-ip\n  fake-ip-filter: ['+.lan']\n")
                .unwrap_err()
                .contains("fake-IP domains")
        );
        assert!(dns_spec_from_clash_yaml("dns:\n  nameserver: [1.1.1.1, resolver.example]\n").is_ok());
        assert!(
            dns_spec_from_clash_yaml("dns:\n  nameserver: [resolver.example]\n")
                .unwrap_err()
                .contains("bootstrap resolver")
        );
    }

    #[test]
    fn unsupported_dns_endpoint_diagnostics_do_not_expose_credentials_or_tokens() {
        let error = dns_spec_from_clash_yaml(
            "dns:\n  nameserver: ['https://user:secret@resolver.example/dns-query?token=private']\n",
        )
        .unwrap_err();
        assert!(error.contains("unsupported credentials"));
        assert!(!error.contains("secret"));
        assert!(!error.contains("private"));
        assert!(!error.contains("https://"));
    }

    #[test]
    fn builds_new_format_section_with_fakeip_and_split_rules() {
        let section = build_dns_section(&sample_spec()).expect("section").expect("non-empty");

        let servers = section["servers"].as_array().expect("servers");
        assert_eq!(servers.len(), 3);

        let remote = &servers[0];
        assert_eq!(remote["type"], "https");
        assert_eq!(remote["server"], "dns.google");
        assert_eq!(remote["server_port"], 443);
        assert_eq!(remote["detour"], "PROXY");
        assert_eq!(
            remote["domain_resolver"], "dns-local",
            "domain-addressed remote gets bootstrap"
        );

        assert_eq!(servers[1]["type"], "local");
        assert!(servers[1].get("server").is_none());

        let fakeip = &servers[2];
        assert_eq!(fakeip["type"], "fakeip");
        assert_eq!(fakeip["inet4_range"], DEFAULT_FAKEIP_V4);
        assert_eq!(fakeip["inet6_range"], DEFAULT_FAKEIP_V6);

        let rules = section["rules"].as_array().expect("rules");
        assert_eq!(rules[0]["server"], "dns-local");
        assert_eq!(rules[0]["domain_suffix"], json!(["cn", "example.com"]));
        assert_eq!(rules[0]["ip_cidr"], json!(["10.0.0.0/8"]));

        // Legacy syntax must never appear (F4: removed in 1.14).
        let rendered = serde_json::to_string(&section).unwrap();
        assert!(!rendered.contains("\"address\""), "{rendered}");
        assert!(!rendered.contains("address_resolver"), "{rendered}");
        assert!(!rendered.contains("address_strategy"), "{rendered}");
    }

    #[test]
    fn ip_addressed_remote_skips_domain_resolver_bootstrap() {
        let mut spec = sample_spec();
        spec.servers[0].server = Some("8.8.8.8".into());
        let section = build_dns_section(&spec).expect("section").expect("non-empty");
        assert!(section["servers"][0].get("domain_resolver").is_none());
    }

    #[test]
    fn empty_spec_generates_nothing() {
        assert_eq!(build_dns_section(&DnsConfigSpec::default()).unwrap(), None);
    }

    #[test]
    fn invalid_specs_are_rejected_before_generation() {
        // Rule pointing at an unknown server.
        let mut spec = sample_spec();
        spec.rules[0].server = "nope".into();
        let err = build_dns_section(&spec).expect_err("unknown target");
        assert!(err.contains("nope"), "{err}");

        // Remote server without an address.
        let mut spec = sample_spec();
        spec.servers[0].server = None;
        assert!(build_dns_section(&spec).is_err());

        // Duplicate tags.
        let mut spec = sample_spec();
        spec.servers[1].tag = "dns-remote".into();
        assert!(build_dns_section(&spec).is_err());
    }

    #[test]
    fn default_domain_resolver_requires_a_known_server() {
        let mut spec = sample_spec();
        assert_eq!(default_domain_resolver(&spec).as_deref(), Some("dns-local"));
        spec.domain_resolver = Some("ghost".into());
        assert_eq!(default_domain_resolver(&spec), None);
    }

    #[test]
    fn spec_round_trips_through_json_storage() {
        let spec = sample_spec();
        let body = serde_json::to_string_pretty(&spec).unwrap();
        let back: DnsConfigSpec = serde_json::from_str(&body).unwrap();
        assert_eq!(back, spec);
    }
}
#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod form_tests {
    use super::*;

    #[test]
    fn parses_server_specs() {
        let parsed = parse_server_spec("https|dns-remote|dns.google|443|PROXY").expect("remote");
        assert_eq!(
            parsed,
            DnsServerSpec {
                tag: "dns-remote".into(),
                kind: DnsServerKind::Https,
                server: Some("dns.google".into()),
                server_port: Some(443),
                path: None,
                detour: Some("PROXY".into()),
                inet4_range: None,
                inet6_range: None,
            }
        );
        let parsed_local = parse_server_spec("local|dns-local|||").expect("local");
        assert_eq!(parsed_local.tag, "dns-local");
        assert_eq!(parsed_local.kind, DnsServerKind::Local);
        assert_eq!(parsed_local.server, None);
        assert_eq!(parsed_local.server_port, None);
    }

    #[test]
    fn rejects_bad_server_specs() {
        assert!(parse_server_spec("bogus|tag").is_err(), "unknown kind");
        assert!(parse_server_spec("udp|").is_err(), "missing segments");
        assert!(parse_server_spec("udp|tag").is_err(), "remote needs address");
        assert!(parse_server_spec("udp|tag|host|99999").is_err(), "bad port");
    }

    #[test]
    fn parses_rule_specs() {
        let parsed = parse_rule_spec("dns-local|suffix=cn,example.com|cidr=10.0.0.0/8").expect("rule");
        assert_eq!(parsed.server, "dns-local");
        assert_eq!(parsed.domain_suffix, vec!["cn", "example.com"]);
        assert_eq!(parsed.ip_cidr, vec!["10.0.0.0/8"]);
        assert!(parsed.domain_keyword.is_empty());
        assert!(parse_rule_spec("dns-local").is_err(), "needs conditions");
        assert!(parse_rule_spec("dns-local|bogus=x").is_err());
    }
}
