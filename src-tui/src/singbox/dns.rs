//! Structured sing-box DNS configuration (task 8.1, design D8).
//!
//! **1.12+ new format only** — the legacy `address`/`address_resolver`
//! syntax is never generated (F4: deprecated in 1.12, removed in 1.14;
//! target core is stable 1.13.x). Fields outside the structured form go
//! through the raw JSON editor (design D7), not here.
//!
//! Covered surface: multiple servers (`udp`/`tls`/`https`/`quic`/`local`/
//! `fakeip`), per-server `detour`, domain/IP split rules, fakeip ranges
//! (presence of a fakeip server is the switch), and the `domain_resolver`
//! bootstrap attached to remote servers whose address is a domain.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

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
    pub domain_suffix: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub domain_keyword: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ip_cidr: Vec<String>,
}

impl DnsRuleSpec {
    fn is_empty(&self) -> bool {
        self.domain_suffix.is_empty() && self.domain_keyword.is_empty() && self.ip_cidr.is_empty()
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

    fn validate(&self) -> Result<(), String> {
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
    spec.domain_resolver
        .clone()
        .filter(|tag| spec.servers.iter().any(|s| &s.tag == tag))
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
    if rule.domain_suffix.is_empty() && rule.domain_keyword.is_empty() && rule.ip_cidr.is_empty() {
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
                    detour: Some("PROXY".into()),
                    inet4_range: None,
                    inet6_range: None,
                },
                DnsServerSpec {
                    tag: "dns-local".into(),
                    kind: DnsServerKind::Local,
                    server: None,
                    server_port: None,
                    detour: None,
                    inet4_range: None,
                    inet6_range: None,
                },
                DnsServerSpec {
                    tag: "dns-fakeip".into(),
                    kind: DnsServerKind::Fakeip,
                    server: None,
                    server_port: None,
                    detour: None,
                    inet4_range: None,
                    inet6_range: None,
                },
            ],
            rules: vec![DnsRuleSpec {
                server: "dns-local".into(),
                domain_suffix: vec!["cn".into(), "example.com".into()],
                domain_keyword: vec![],
                ip_cidr: vec!["10.0.0.0/8".into()],
            }],
            domain_resolver: Some("dns-local".into()),
        }
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
