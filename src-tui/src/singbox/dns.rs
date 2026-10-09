//! Structured sing-box DNS configuration (task 8.1, design D8).
//!
//! **1.12+ new format only** — the legacy `address`/`address_resolver`
//! syntax is never generated (F4: deprecated in 1.12, removed in 1.14;
//! target core is pinned to stable 1.14.2). Fields outside the structured form go
//! through the raw JSON editor (design D7), not here.
//!
//! Covered surface: multiple servers (`udp`/`tls`/`https`/`quic`/`h3`/`local`/
//! `fakeip`), per-server `detour`, domain/IP split rules, fakeip ranges and
//! A/AAAA routing rules, and the `domain_resolver`
//! bootstrap attached to remote servers whose address is a domain.
//!
//! Profile conversion degrades instead of aborting (#52 philosophy): every
//! Clash DNS policy the typed model cannot express (`fallback`,
//! `fallback-filter`, `listen`, `fake-ip-filter`, …) is dropped with an
//! explicit report note. Only structural breakage — a `dns` block that is not
//! a mapping, a `nameserver` that is not a list of strings — fails the
//! conversion.

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
    /// DNS-over-HTTP/3. Verified present in sing-box 1.14.2
    /// (`initialize DNS server`: only `udp`/`tls`/`https`/`quic`/`h3`/
    /// `fakeip`/`hosts`/`local` are accepted transport types).
    H3,
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
            "h3" => Self::H3,
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
            Self::H3 => "h3",
            Self::Local => "local",
            Self::Fakeip => "fakeip",
        }
    }

    /// Remote server kinds carry an upstream address; local/fakeip do not.
    pub fn is_remote(self) -> bool {
        matches!(self, Self::Udp | Self::Tls | Self::Https | Self::Quic | Self::H3)
    }

    /// Kinds that terminate TLS and therefore accept `tls_insecure`.
    pub fn is_tls(self) -> bool {
        matches!(self, Self::Tls | Self::Https | Self::Quic | Self::H3)
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
    /// Disable TLS certificate verification (Clash `#skip-cert-verify=true`),
    /// rendered as `tls: {"enabled": true, "insecure": true}`. Only the
    /// TLS-capable remote kinds (`tls`/`https`/`quic`/`h3`) accept it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_insecure: Option<bool>,
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
/// Tag of the server standing in for Clash's `system` resolver.
const SYSTEM_SERVER_TAG: &str = "profile-system-dns";
/// Tag of the generated fake-IP pool.
const FAKEIP_SERVER_TAG: &str = "profile-fakeip";

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
            if server.path.is_some() && !matches!(server.kind, DnsServerKind::Https | DnsServerKind::H3) {
                return Err(format!(
                    "dns server {:?}: path is supported only for DoH/DoH3 servers",
                    server.tag
                ));
            }
            if server.tls_insecure.is_some() && !server.kind.is_tls() {
                return Err(format!(
                    "dns server {:?}: tls_insecure is only supported for TLS DNS servers ({})",
                    server.tag,
                    server.kind.as_str()
                ));
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

/// Result of converting a profile's `dns` block: the typed spec plus the
/// degradation report. `spec: None` means the profile has no usable DNS
/// block, so callers retain the user's persisted sidecar settings.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DnsConversionReport {
    pub spec: Option<DnsConfigSpec>,
    /// Human-readable degradation lines, one per unrepresentable policy
    /// (`dns.<key>: …`), surfaced by `status` next to the node-level
    /// conversion notes.
    pub notes: Vec<String>,
}

impl DnsConversionReport {
    fn note(&mut self, key: impl std::fmt::Display, message: impl std::fmt::Display) {
        self.notes.push(format!("dns.{key}: {message}"));
    }
}

/// Convert the profile's Clash `dns` block into the typed sing-box 1.14 DNS
/// model, degrading with a report instead of aborting (#52).
///
/// Structural breakage — `dns` that is not a mapping, a `nameserver` that is
/// not a list of strings, a `nameserver-policy` that is not a mapping — is
/// still fatal, because nothing about the intended resolver can be guessed.
/// Everything the typed model cannot express is dropped with a note.
pub fn dns_conversion_from_clash_yaml(yaml: &str) -> Result<DnsConversionReport, String> {
    let mut report = DnsConversionReport::default();
    let root: Yaml = serde_yaml_ng::from_str(yaml).map_err(|error| format!("invalid profile YAML: {error}"))?;
    let Some(root) = root.as_mapping() else {
        return Err("profile YAML root is not a mapping".into());
    };
    let Some(dns_value) = root.get(Yaml::String("dns".into())) else {
        return Ok(report);
    };
    let Some(dns) = dns_value.as_mapping() else {
        return Err("dns must be a mapping".into());
    };
    let get = |key: &str| dns.get(Yaml::String(key.into()));
    if get("enable").is_some() && get("enable").and_then(Yaml::as_bool).is_none() {
        return Err("dns.enable must be a boolean".into());
    }
    if get("enable").and_then(Yaml::as_bool) == Some(false) {
        report.note(
            "enable",
            "DNS is disabled by the profile; its resolver policy is dropped",
        );
        return Ok(report);
    }
    if get("ipv6").is_some() && get("ipv6").and_then(Yaml::as_bool).is_none() {
        return Err("dns.ipv6 must be a boolean".into());
    }
    if get("ipv6").and_then(Yaml::as_bool) == Some(false) {
        report.note(
            "ipv6",
            "disabling IPv6 DNS has no sing-box equivalent; AAAA answers are left to the resolver",
        );
    }
    if get("enhanced-mode").is_some() && get("enhanced-mode").and_then(Yaml::as_str).is_none() {
        return Err("dns.enhanced-mode must be a string".into());
    }
    for key in ["fake-ip-range", "fake-ip-range6"] {
        if get(key).is_some() && get(key).and_then(Yaml::as_str).is_none() {
            return Err(format!("dns.{key} must be a CIDR string"));
        }
    }

    // Fields the typed model cannot express: reported, never fatal.
    for key in ["fallback", "proxy-server-nameserver", "listen", "respect-rules"] {
        if get(key).is_some_and(|value| !value.is_null()) {
            report.note(
                key,
                "dropped; sing-box resolves through the servers listed under `nameserver`",
            );
        }
    }
    if get("fallback-filter").is_some_and(|value| !value.is_null()) {
        report.note(
            "fallback-filter",
            "dropped; sing-box has no parallel/fallback resolver split (all servers are used per `dns.rules`)",
        );
    }
    for key in ["use-system-hosts", "use-hosts"] {
        match get(key).filter(|value| !value.is_null()) {
            None => {}
            Some(value) if value.as_bool() == Some(false) => {}
            Some(value) if value.as_bool().is_none() => return Err(format!("dns.{key} must be a boolean")),
            Some(_) => {
                report.note(
                    key,
                    "dropped; the profile's hosts table is not imported into a sing-box `hosts` DNS server",
                );
            }
        }
    }
    if let Some(filter) = get("fake-ip-filter").filter(|value| !value.is_null()) {
        let count = filter.as_sequence().map_or(1, |values| values.len());
        report.note(
            "fake-ip-filter",
            format!(
                "dropped ({count} entr{}); those domains now resolve into the fake-IP pool",
                if count == 1 { "y" } else { "ies" }
            ),
        );
    }
    if get("fake-ip-filter-mode").is_some_and(|value| !value.is_null()) {
        report.note(
            "fake-ip-filter-mode",
            "dropped; sing-box has no fake-IP filter mode selector",
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
                | "use-hosts"
                | "use-system-hosts"
        ) {
            report.note(key, "unknown profile DNS field; dropped");
        }
    }

    let mut spec = DnsConfigSpec::default();
    let mut tags = std::collections::HashMap::<String, String>::new();

    // `default-nameserver` is the bootstrap that resolves the *hostnames* of
    // the encrypted upstreams (doh.pub, dns.alidns.com, …). Without it the
    // core refuses to start, so it is generated first and referenced as
    // `domain_resolver` by every remote server addressed by a domain.
    if let Some(defaults) = get("default-nameserver").filter(|value| !value.is_null()) {
        let entries = as_string_list(defaults, "default-nameserver")?;
        for entry in &entries {
            add_bootstrap_server(entry, &mut spec, &mut tags, &mut report);
        }
        add_system_bootstrap(&entries, &mut spec, &mut report);
        report.note(
            "default-nameserver",
            format!(
                "mapped to {} bootstrap DNS server(s); `{}` resolves upstream hostnames",
                spec.servers.len(),
                spec.domain_resolver.as_deref().unwrap_or_default()
            ),
        );
    }

    if let Some(nameservers) = get("nameserver").filter(|value| !value.is_null()) {
        for endpoint in as_string_list(nameservers, "nameserver")? {
            add_clash_dns_endpoint(&endpoint, &mut spec, &mut tags, &mut report);
        }
    }
    if let Some(policy) = get("nameserver-policy").filter(|value| !value.is_null()) {
        let Some(policy) = policy.as_mapping() else {
            return Err("dns.nameserver-policy must be a mapping".into());
        };
        for (pattern, endpoints) in policy {
            let Some(pattern) = pattern.as_str() else {
                return Err("dns.nameserver-policy keys must be strings".to_string());
            };
            let entries = as_string_list(endpoints, &format!("nameserver-policy.{pattern}"))?;
            let Some((field, value)) = parse_clash_dns_policy_pattern(pattern) else {
                report.note(
                    format!("nameserver-policy.{pattern}"),
                    "unsupported matcher; use +.domain, keyword:, or ipcidr:",
                );
                continue;
            };
            if entries.len() > 1 {
                report.note(
                    format!("nameserver-policy.{pattern}"),
                    format!(
                        "{} of the {} listed servers are dropped; sing-box `dns.rules` use the first match",
                        entries.len() - 1,
                        entries.len()
                    ),
                );
            }
            let Some(tag) = add_clash_dns_endpoint(&entries[0], &mut spec, &mut tags, &mut report) else {
                report.note(
                    format!("nameserver-policy.{pattern}"),
                    "every listed server was dropped, so the policy is not applied",
                );
                continue;
            };
            let mut rule = DnsRuleSpec {
                server: tag,
                query_type: Vec::new(),
                domain_suffix: Vec::new(),
                domain_keyword: Vec::new(),
                ip_cidr: Vec::new(),
            };
            match field {
                "domain_suffix" => rule.domain_suffix.push(value),
                "domain_keyword" => rule.domain_keyword.push(value),
                _ => unreachable!("ipcidr policies are rejected by parse_clash_dns_policy_pattern"),
            }
            spec.rules.push(rule);
        }
    }

    match get("enhanced-mode").and_then(Yaml::as_str).unwrap_or("redir-host") {
        "redir-host" => {
            for key in ["fake-ip-range", "fake-ip-range6"] {
                if get(key).is_some() {
                    report.note(key, "ignored; only valid with enhanced-mode: fake-ip");
                }
            }
        }
        "fake-ip" => {
            let inet4_range = get("fake-ip-range").and_then(Yaml::as_str).map(str::to_string);
            let inet6_range = get("fake-ip-range6").and_then(Yaml::as_str).map(str::to_string);
            for (field, range) in [
                ("fake-ip-range", inet4_range.as_deref()),
                ("fake-ip-range6", inet6_range.as_deref()),
            ] {
                if let Some(range) = range
                    && validate_cidr(range).is_err()
                {
                    report.note(
                        field,
                        format!("invalid CIDR {range:?}; the sing-box default is used instead"),
                    );
                }
            }
            let (inet4_range, inet6_range) = (
                inet4_range.filter(|range| validate_cidr(range).is_ok()),
                inet6_range.filter(|range| validate_cidr(range).is_ok()),
            );
            spec.servers.push(DnsServerSpec {
                tag: FAKEIP_SERVER_TAG.into(),
                kind: DnsServerKind::Fakeip,
                server: None,
                server_port: None,
                path: None,
                detour: None,
                tls_insecure: None,
                inet4_range,
                inet6_range,
            });
            spec.rules.insert(
                0,
                DnsRuleSpec {
                    server: FAKEIP_SERVER_TAG.into(),
                    query_type: vec!["A".into(), "AAAA".into()],
                    domain_suffix: Vec::new(),
                    domain_keyword: Vec::new(),
                    ip_cidr: Vec::new(),
                },
            );
        }
        mode => report.note(
            "enhanced-mode",
            format!("unsupported value {mode:?}; treated as redir-host (no fake-IP pool)"),
        ),
    }

    // A fake-IP pool may not be sing-box's default server, so the profile
    // needs at least one real upstream before it.
    if spec
        .servers
        .first()
        .is_some_and(|server| server.kind == DnsServerKind::Fakeip)
    {
        spec.servers.insert(
            0,
            DnsServerSpec {
                tag: SYSTEM_SERVER_TAG.into(),
                kind: DnsServerKind::Local,
                server: None,
                server_port: None,
                path: None,
                detour: None,
                tls_insecure: None,
                inet4_range: None,
                inet6_range: None,
            },
        );
        report.note(
            "nameserver",
            "the profile lists no upstream resolver, so the system resolver is used as sing-box's default server",
        );
    }

    // `default-nameserver` wins; otherwise fall back to the first
    // IP-literal plain-DNS upstream so hostname-addressed servers
    // (doh.pub, dot.pub, …) can be bootstrapped.
    match spec.domain_resolver.clone().or_else(|| first_ip_literal_server(&spec)) {
        Some(resolver) => spec.domain_resolver = Some(resolver),
        // Nothing can resolve a hostname: drop those servers (with a note)
        // instead of emitting a config the core refuses to start.
        None => {
            spec.servers.retain(|server| {
                let unresolvable =
                    server.kind.is_remote() && server.server.as_deref().is_some_and(|address| !looks_like_ip(address));
                if unresolvable {
                    report.note(
                        "nameserver",
                        format!(
                            "{} ({}://{}) is dropped: no IP-literal bootstrap resolver is available to resolve it",
                            server.tag,
                            server.kind.as_str(),
                            server.server.as_deref().unwrap_or_default()
                        ),
                    );
                }
                !unresolvable
            });
            spec.rules
                .retain(|rule| spec.servers.iter().any(|server| server.tag == rule.server));
        }
    }

    spec.validate()?;
    report.spec = Some(spec);
    Ok(report)
}

/// A YAML value that must be a string or a list of strings.
fn as_string_list(value: &Yaml, key: &str) -> Result<Vec<String>, String> {
    let items: Vec<&Yaml> = match value {
        Yaml::Sequence(values) => values.iter().collect(),
        other => vec![other],
    };
    items
        .into_iter()
        .map(|item| {
            item.as_str()
                .map(str::to_string)
                .ok_or_else(|| format!("dns.{key} entries must be strings"))
        })
        .collect()
}

fn first_ip_literal_server(spec: &DnsConfigSpec) -> Option<String> {
    spec.servers
        .iter()
        .find(|server| server.kind == DnsServerKind::Udp && server.server.as_deref().is_some_and(looks_like_ip))
        .map(|server| server.tag.clone())
}

/// Register one `default-nameserver` entry as a plain-UDP bootstrap server.
fn add_bootstrap_server(
    entry: &str,
    spec: &mut DnsConfigSpec,
    tags: &mut std::collections::HashMap<String, String>,
    report: &mut DnsConversionReport,
) {
    let entry = entry.trim();
    if entry.is_empty() || matches!(entry, "system" | "system://") {
        return;
    }
    let (address, port) = split_host_port(entry);
    if !looks_like_ip(&address) {
        report.note(
            "default-nameserver",
            format!("{entry:?} is not an IP literal and is dropped from the bootstrap list"),
        );
        return;
    }
    let tag = register_server(
        spec,
        tags,
        DnsServerSpec {
            tag: String::new(),
            kind: DnsServerKind::Udp,
            server: Some(address),
            server_port: port,
            path: None,
            detour: None,
            tls_insecure: None,
            inet4_range: None,
            inet6_range: None,
        },
    );
    // The first IP-literal entry wins; a `system` entry is only used when the
    // profile offers nothing else, so sing-box's default server stays a real
    // upstream rather than the host resolver.
    if spec.domain_resolver.is_none() {
        spec.domain_resolver = Some(tag);
    }
}

/// Clash's `system` bootstrap, used only when no IP-literal resolver exists.
fn add_system_bootstrap(entries: &[String], spec: &mut DnsConfigSpec, report: &mut DnsConversionReport) {
    if spec.domain_resolver.is_some()
        || !entries
            .iter()
            .any(|entry| matches!(entry.trim(), "system" | "system://"))
    {
        return;
    }
    add_system_server(spec);
    spec.domain_resolver = Some(SYSTEM_SERVER_TAG.to_string());
    report.note(
        "default-nameserver",
        "the system resolver is used to bootstrap the hostname-addressed upstreams",
    );
}

fn add_system_server(spec: &mut DnsConfigSpec) {
    if !spec.servers.iter().any(|server| server.tag == SYSTEM_SERVER_TAG) {
        spec.servers.push(DnsServerSpec {
            tag: SYSTEM_SERVER_TAG.into(),
            kind: DnsServerKind::Local,
            server: None,
            server_port: None,
            path: None,
            detour: None,
            tls_insecure: None,
            inet4_range: None,
            inet6_range: None,
        });
    }
}

/// Register one `nameserver` / `nameserver-policy` endpoint, returning its tag
/// or `None` when the endpoint was dropped with a note.
fn add_clash_dns_endpoint(
    endpoint: &str,
    spec: &mut DnsConfigSpec,
    tags: &mut std::collections::HashMap<String, String>,
    report: &mut DnsConversionReport,
) -> Option<String> {
    let endpoint = endpoint.trim();
    let key = "nameserver";
    if endpoint.is_empty() {
        report.note(key, "an empty nameserver entry is dropped");
        return None;
    }
    if matches!(endpoint, "system://" | "system") {
        add_system_server(spec);
        if spec.domain_resolver.is_none() {
            spec.domain_resolver = Some(SYSTEM_SERVER_TAG.to_string());
        }
        return Some(SYSTEM_SERVER_TAG.to_string());
    }
    let (base, fragment) = endpoint.split_once('#').unwrap_or((endpoint, ""));
    // Notes quote the endpoint without its scheme, path or credentials.
    let host = endpoint_host(base);
    let mut force_h3 = false;
    let mut insecure = false;
    for option in fragment.split(['&', ',']).filter(|part| !part.trim().is_empty()) {
        let (name, value) = option.split_once('=').unwrap_or((option, ""));
        match name.trim().to_ascii_lowercase().as_str() {
            "h3" => {
                force_h3 = matches!(value.trim().to_ascii_lowercase().as_str(), "true" | "1" | "yes");
                if force_h3 {
                    report.note(key, format!("{host}: '#h3=true' mapped to the h3 DNS server type"));
                }
            }
            "skip-cert-verify" | "skip-cert-verification" => {
                if matches!(value.trim().to_ascii_lowercase().as_str(), "true" | "1" | "yes") {
                    insecure = true;
                    report.note(
                        key,
                        format!("{host}: '#skip-cert-verify=true' mapped to `tls.insecure`"),
                    );
                }
            }
            other => {
                report.note(key, format!("{host}: unsupported '#{other}' option is dropped"));
            }
        }
    }

    // Bare IP literals (IPv6 in particular) are not URL-parsable.
    if let Ok(address) = base.parse::<std::net::IpAddr>() {
        let tag = register_server(
            spec,
            tags,
            DnsServerSpec {
                tag: String::new(),
                kind: DnsServerKind::Udp,
                server: Some(address.to_string()),
                server_port: None,
                path: None,
                detour: None,
                tls_insecure: None,
                inet4_range: None,
                inet6_range: None,
            },
        );
        return Some(tag);
    }

    let uri = if base.contains("://") {
        base.to_string()
    } else {
        format!("udp://{base}")
    };
    let Ok(url) = reqwest::Url::parse(&uri) else {
        report.note(key, "an unparsable nameserver entry is dropped");
        return None;
    };
    if !url.username().is_empty() || url.password().is_some() {
        report.note(key, "a nameserver entry carrying URL credentials is dropped");
        return None;
    }
    let scheme = url.scheme().to_string();
    let mut kind = match scheme.as_str() {
        "udp" => DnsServerKind::Udp,
        "tls" => DnsServerKind::Tls,
        "https" => DnsServerKind::Https,
        "quic" => DnsServerKind::Quic,
        "tcp" => {
            report.note(
                key,
                format!("{host}: dropped — sing-box 1.14 has no DNS-over-TCP server type"),
            );
            return None;
        }
        "http" => {
            report.note(
                key,
                format!("{host}: dropped — sing-box 1.14 has no plain DNS-over-HTTP server type"),
            );
            return None;
        }
        other => {
            report.note(key, format!("unsupported DNS scheme {other:?}; the entry is dropped"));
            return None;
        }
    };
    if force_h3 {
        if kind == DnsServerKind::Https {
            kind = DnsServerKind::H3;
        } else {
            report.note(
                key,
                format!("{host}: '#h3=true' only applies to https:// entries and is ignored"),
            );
        }
    }
    let Some(server) = url.host_str().map(str::to_string) else {
        report.note(key, "a nameserver entry without a host is dropped");
        return None;
    };
    if url.query().is_some() {
        report.note(
            key,
            format!("{host}: query parameters are dropped; sing-box DNS servers have no query knobs"),
        );
    }
    let raw_path = url.path();
    let path = match (kind, raw_path) {
        (DnsServerKind::Https | DnsServerKind::H3, "/" | "") => None,
        (DnsServerKind::Https | DnsServerKind::H3, path) => Some(path.to_string()),
        (_, "/" | "") => None,
        _ => {
            report.note(
                key,
                format!("{host}: the URL path is ignored for {} servers", kind.as_str()),
            );
            None
        }
    };
    Some(register_server(
        spec,
        tags,
        DnsServerSpec {
            tag: String::new(),
            kind,
            server: Some(server),
            server_port: url.port(),
            path,
            detour: None,
            tls_insecure: insecure.then_some(true),
            inet4_range: None,
            inet6_range: None,
        },
    ))
}

/// Endpoint without its scheme, path or credentials — safe to quote in a note.
fn endpoint_host(base: &str) -> String {
    let trimmed = base.split_once("://").map_or(base, |(_, rest)| rest);
    let host = trimmed
        .split(['/', '?', '#'])
        .next()
        .unwrap_or(trimmed)
        .rsplit('@')
        .next()
        .unwrap_or(trimmed);
    host.chars().take(120).collect()
}

fn split_host_port(entry: &str) -> (String, Option<u16>) {
    if let Ok(address) = entry.parse::<std::net::IpAddr>() {
        return (address.to_string(), None);
    }
    if let Some((host, port)) = entry.rsplit_once(':')
        && let Ok(port) = port.parse::<u16>()
        && !host.is_empty()
    {
        return (host.trim_matches(['[', ']']).to_string(), Some(port));
    }
    (entry.to_string(), None)
}

/// Append a server under a content-derived tag, reusing the tag of an
/// identical endpoint so rules keep pointing at one server.
fn register_server(
    spec: &mut DnsConfigSpec,
    tags: &mut std::collections::HashMap<String, String>,
    mut server: DnsServerSpec,
) -> String {
    let key = format!(
        "{}|{}|{}|{}|{}",
        server.kind.as_str(),
        server.server.clone().unwrap_or_default(),
        server.server_port.unwrap_or_default(),
        server.path.clone().unwrap_or_default(),
        server.tls_insecure.unwrap_or_default()
    );
    if let Some(tag) = tags.get(&key) {
        return tag.clone();
    }
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    key.hash(&mut hasher);
    let tag = format!("profile-dns-{:016x}", hasher.finish());
    server.tag = tag.clone();
    spec.servers.push(server);
    tags.insert(key, tag.clone());
    tag
}

/// Map a Clash `nameserver-policy` key to the typed matcher it becomes.
/// `None` means the matcher has no typed equivalent (the policy line is
/// reported and dropped instead of failing the whole DNS layer).
fn parse_clash_dns_policy_pattern(pattern: &str) -> Option<(&'static str, String)> {
    let value = |field: &'static str, value: &str| {
        let value = value.trim();
        (!value.is_empty()).then(|| (field, value.to_string()))
    };
    if let Some(rest) = pattern.strip_prefix("+.") {
        return value("domain_suffix", rest);
    }
    for (prefix, field) in [
        ("domain:", "domain_suffix"),
        ("keyword:", "domain_keyword"),
        ("ipcidr:", "ip_cidr"),
    ] {
        if let Some(rest) = pattern.strip_prefix(prefix) {
            let value = value(field, rest)?;
            if field == "domain_suffix" {
                // `domain:` matches one exact name; the typed rule only has
                // `domain_suffix`, and widening it to a suffix would send
                // unrelated names to this resolver.
                return None;
            }
            if field == "ip_cidr" {
                // sing-box 1.14.2 rejects an `ip_cidr` DNS rule without a
                // response-evaluation rule ("Response Match Fields ...
                // require match_response to be enabled"), and the typed spec
                // cannot express one — so the policy is dropped.
                return None;
            }
            return Some(value);
        }
    }
    None
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
            DnsServerKind::Udp
            | DnsServerKind::Tls
            | DnsServerKind::Https
            | DnsServerKind::Quic
            | DnsServerKind::H3 => {
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
                // Clash's `#skip-cert-verify=true`. Verified accepted by
                // sing-box 1.14.2 (`tls: {"enabled": true, "insecure": true}`
                // on tls/https/quic/h3 servers); the flat `insecure` key is
                // rejected ("dns.servers[0].insecure: unknown field").
                if s.kind.is_tls()
                    && let Some(true) = s.tls_insecure
                {
                    entry["tls"] = json!({ "enabled": true, "insecure": true });
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
        tls_insecure: None,
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

    /// Trimmed copy of a real subscription `dns` block: every field below is
    /// emitted by commercial Clash providers (trimmed for length).
    const REAL_WORLD_DNS_YAML: &str = r#"
dns:
  enable: true
  ipv6: true
  use-system-hosts: false
  listen: 127.0.0.1:5335
  enhanced-mode: fake-ip
  fake-ip-range: 198.18.0.1/16
  fake-ip-filter:
    - "*.lan"
    - time.windows.com
  default-nameserver:
    - system
    - 180.76.76.76
    - 2400:3200::1
  nameserver:
    - https://223.5.5.5/dns-query#skip-cert-verify=true
    - https://doh.pub/dns-query#skip-cert-verify=true
    - tls://dot.pub#skip-cert-verify=true
    - https://223.6.6.6/dns-query#skip-cert-verify=true&h3=true
  nameserver-policy:
    +.quandao.com:
      - https://api-d.dohcore.com:2096/dns-query/2dd6e008-2226-49fa-8f84-ba392a637f4d#skip-cert-verify=true
      - https://108.62.161.127:2096/dns-query/2dd6e008-2226-49fa-8f84-ba392a637f4d#skip-cert-verify=true
  fallback-filter:
    geoip: true
    ipcidr:
      - 240.0.0.0/4
"#;

    /// Spec-only view of a profile DNS block (degradation notes are asserted
    /// through `dns_conversion_from_clash_yaml`).
    fn spec_from_clash_yaml(yaml: &str) -> Result<Option<DnsConfigSpec>, String> {
        dns_conversion_from_clash_yaml(yaml).map(|report| report.spec)
    }

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
                    tls_insecure: None,
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
                    tls_insecure: None,
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
                    tls_insecure: None,
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
        let spec = spec_from_clash_yaml(yaml).unwrap().unwrap();
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
                tls_insecure: None,
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
    fn absent_profile_dns_keeps_sidecar_and_unsupported_policy_is_reported() {
        assert!(spec_from_clash_yaml("proxies: []\n").unwrap().is_none());
        let report = dns_conversion_from_clash_yaml("dns:\n  nameserver: [1.1.1.1]\n  fallback: [8.8.8.8]\n")
            .expect("fallback is degraded, not fatal");
        assert!(
            report.notes.iter().any(|note| note.starts_with("dns.fallback:")),
            "{:?}",
            report.notes
        );
        assert_eq!(
            report.spec.expect("spec").servers[0].server.as_deref(),
            Some("1.1.1.1"),
            "the supported nameserver still converts"
        );
        // Any DoH path is accepted: sing-box forwards the configured path.
        let doh = spec_from_clash_yaml("dns:\n  nameserver: [https://1.2.3.4/custom-path, 1.1.1.1]\n")
            .unwrap()
            .unwrap();
        assert_eq!(doh.servers[0].path.as_deref(), Some("/custom-path"));
        assert_eq!(
            build_dns_section(&doh).unwrap().unwrap()["servers"][0]["path"],
            "/custom-path"
        );
        let filtered = dns_conversion_from_clash_yaml("dns:\n  enhanced-mode: fake-ip\n  fake-ip-filter: ['+.lan']\n")
            .expect("fake-ip-filter is degraded");
        assert!(
            filtered.notes.iter().any(|note| note.contains("dns.fake-ip-filter:")),
            "{:?}",
            filtered.notes
        );
    }

    #[test]
    fn real_world_clash_dns_degrades_with_notes_instead_of_aborting() {
        // Shape of a real subscription DNS block (Trimmed for length; every
        // field below is one a Clash provider actually emits.)
        let report = dns_conversion_from_clash_yaml(REAL_WORLD_DNS_YAML).expect("conversion must not abort");
        let notes = &report.notes;
        for expected in [
            "dns.listen:",
            "dns.fake-ip-filter:",
            "dns.fallback-filter:",
            "dns.default-nameserver:",
            "'#skip-cert-verify=true' mapped to `tls.insecure`",
            "'#h3=true'",
            "dns.nameserver-policy.+.quandao.com:",
        ] {
            assert!(
                notes.iter().any(|note| note.contains(expected)),
                "missing note for {expected}: {notes:#?}"
            );
        }

        let spec = report.spec.expect("spec");
        // fake-IP pool survives, with the profile's own range.
        let fakeip = spec
            .servers
            .iter()
            .find(|s| s.kind == DnsServerKind::Fakeip)
            .expect("fakeip");
        assert_eq!(fakeip.inet4_range.as_deref(), Some("198.18.0.1/16"));
        assert_eq!(spec.rules[0].server, fakeip.tag);
        assert_eq!(spec.rules[0].query_type, ["A", "AAAA"]);

        // default-nameserver became the bootstrap that resolves doh.pub & co.
        let resolver = spec.domain_resolver.clone().expect("bootstrap resolver");
        let bootstrap = spec
            .servers
            .iter()
            .find(|s| s.tag == resolver)
            .expect("bootstrap server");
        assert_eq!(bootstrap.kind, DnsServerKind::Udp);
        assert!(bootstrap.server.as_deref().is_some_and(looks_like_ip));
        assert_ne!(resolver, fakeip.tag, "fake-IP must never resolve hostnames");

        // '#h3=true' selected the h3 transport; DoT stayed DoT.
        assert!(
            spec.servers
                .iter()
                .any(|s| s.kind == DnsServerKind::H3 && s.server.as_deref() == Some("223.6.6.6"))
        );
        assert!(
            spec.servers
                .iter()
                .any(|s| s.kind == DnsServerKind::Tls && s.server.as_deref() == Some("dot.pub"))
        );
        assert!(
            !spec.servers.iter().any(|s| s.kind == DnsServerKind::Local),
            "the OS resolver is not an upstream for proxied lookups"
        );

        // nameserver-policy '+.x' became a dns.rules domain_suffix rule.
        let policy_rule = spec
            .rules
            .iter()
            .find(|rule| rule.domain_suffix == vec!["quandao.com".to_string()])
            .expect("policy rule");
        assert!(
            spec.servers.iter().any(|s| s.tag == policy_rule.server),
            "policy rule targets a registered server"
        );

        // The emitted section is valid sing-box 1.12+ shape: every remote
        // server addressed by a domain carries a bootstrap resolver.
        let section = build_dns_section(&spec).expect("section").expect("non-empty");
        for server in section["servers"].as_array().expect("servers") {
            if matches!(server["type"].as_str(), Some("udp" | "tls" | "https" | "quic" | "h3"))
                && server["server"].as_str().is_some_and(|address| !looks_like_ip(address))
            {
                assert_eq!(
                    server["domain_resolver"], resolver,
                    "domain-addressed server needs a bootstrap: {server}"
                );
            }
            assert!(server.get("domain_resolver").is_none() || server["type"] != "fakeip");
        }
        assert_eq!(default_domain_resolver(&spec).as_deref(), Some(resolver.as_str()));
    }

    #[test]
    fn h3_query_fragment_selects_the_h3_transport() {
        let spec = spec_from_clash_yaml(
            "dns:\n  default-nameserver: [223.5.5.5]\n  nameserver: ['https://223.6.6.6/dns-query#skip-cert-verify=true&h3=true']\n",
        )
        .unwrap()
        .unwrap();
        let h3 = spec
            .servers
            .iter()
            .find(|s| s.kind == DnsServerKind::H3)
            .expect("h3 server");
        assert_eq!(h3.server.as_deref(), Some("223.6.6.6"));
        assert_eq!(h3.path.as_deref(), Some("/dns-query"));
        assert_eq!(build_dns_section(&spec).unwrap().unwrap()["servers"][1]["type"], "h3");
    }

    #[test]
    fn skip_cert_verify_fragment_becomes_tls_insecure() {
        let yaml = "dns:\n  default-nameserver: [223.5.5.5]\n  nameserver:\n    - https://223.5.5.5/dns-query#skip-cert-verify=true\n    - tls://dot.pub#skip-cert-verify=false\n    - https://doh.pub/dns-query\n";
        let report = dns_conversion_from_clash_yaml(yaml).expect("converted");
        assert_eq!(
            report
                .notes
                .iter()
                .filter(|note| note.contains("skip-cert-verify"))
                .count(),
            1,
            "only the enabled variant is reported: {:?}",
            report.notes
        );
        let spec = report.spec.expect("spec");
        let insecure: Vec<_> = spec.servers.iter().filter(|s| s.tls_insecure == Some(true)).collect();
        assert_eq!(insecure.len(), 1, "one endpoint opted out of verification");
        assert_eq!(insecure[0].server.as_deref(), Some("223.5.5.5"));

        let section = build_dns_section(&spec).unwrap().unwrap();
        let servers = section["servers"].as_array().expect("servers");
        let doh_by_ip = servers
            .iter()
            .find(|s| s["server"] == "223.5.5.5" && s["type"] == "https")
            .expect("DoH by IP");
        assert_eq!(doh_by_ip["tls"], json!({"enabled": true, "insecure": true}));
        assert!(
            servers
                .iter()
                .filter(|s| s["server"] == "dot.pub" || s["server"] == "doh.pub")
                .all(|s| s.get("tls").is_none()),
            "endpoints that kept verification must not grow a tls block: {servers:?}"
        );
        // The plain-UDP bootstraps never carry TLS options.
        assert!(
            servers
                .iter()
                .filter(|s| s["type"] == "udp")
                .all(|s| s.get("tls").is_none())
        );
    }

    #[test]
    fn tls_insecure_is_rejected_for_non_tls_servers() {
        let mut spec = sample_spec();
        spec.servers[0].kind = DnsServerKind::Udp;
        spec.servers[0].tls_insecure = Some(true);
        let error = spec.validate().expect_err("udp has no TLS");
        assert!(error.contains("tls_insecure"), "{error}");
        assert!(build_dns_section(&spec).is_err());
        // h3 and quic are TLS transports and accept the flag.
        for kind in [
            DnsServerKind::H3,
            DnsServerKind::Quic,
            DnsServerKind::Tls,
            DnsServerKind::Https,
        ] {
            let mut spec = sample_spec();
            spec.servers[0].kind = kind;
            spec.servers[0].tls_insecure = Some(true);
            spec.validate()
                .unwrap_or_else(|error| panic!("{kind:?} must accept tls_insecure: {error}"));
            let section = build_dns_section(&spec).unwrap().unwrap();
            assert_eq!(section["servers"][0]["tls"], json!({"enabled": true, "insecure": true}));
        }
    }

    #[test]
    fn nameserver_policy_keeps_the_first_server_and_reports_the_rest() {
        let report = dns_conversion_from_clash_yaml(
            "dns:\n  default-nameserver: [223.5.5.5]\n  nameserver-policy:\n    +.example.com: ['https://1.1.1.1/dns-query', 'https://2.2.2.2/dns-query']\n",
        )
        .expect("policy");
        assert!(
            report
                .notes
                .iter()
                .any(|note| note.contains("dns.nameserver-policy.+.example.com:") && note.contains("first match")),
            "{:?}",
            report.notes
        );
        let spec = report.spec.expect("spec");
        let rules: Vec<_> = spec.rules.iter().filter(|r| !r.domain_suffix.is_empty()).collect();
        assert_eq!(rules.len(), 1, "{rules:?}");
    }

    #[test]
    fn unsupported_policy_matchers_are_dropped_with_a_note() {
        for (pattern, why) in [
            ("ipcidr:10.0.0.0/8", "sing-box rejects `ip_cidr` DNS rules"),
            ("domain:example.com", "exact-domain"),
            ("geosite:cn", "geosite"),
        ] {
            let yaml =
                format!("dns:\n  default-nameserver: [223.5.5.5]\n  nameserver-policy:\n    {pattern}: 1.1.1.1\n");
            let report = dns_conversion_from_clash_yaml(&yaml).expect("degraded");
            assert!(
                report
                    .notes
                    .iter()
                    .any(|note| note.contains(&format!("dns.nameserver-policy.{pattern}:"))),
                "{pattern}: {:?}",
                report.notes
            );
            assert!(report.spec.expect("spec").rules.is_empty(), "{pattern}: {why}");
        }
    }

    #[test]
    fn hostname_upstreams_survive_when_a_bootstrap_is_available() {
        let report = dns_conversion_from_clash_yaml("dns:\n  nameserver: [https://doh.pub/dns-query, 1.1.1.1]\n")
            .expect("converted");
        let spec = report.spec.expect("spec");
        let resolver = spec.domain_resolver.clone().expect("bootstrap");
        assert!(spec.servers.iter().any(|s| s.server.as_deref() == Some("doh.pub")));
        let section = build_dns_section(&spec).unwrap().unwrap();
        let doh = section["servers"]
            .as_array()
            .expect("servers")
            .iter()
            .find(|server| server["server"] == "doh.pub")
            .expect("doh.pub server");
        assert_eq!(doh["domain_resolver"], resolver, "the DoH hostname is bootstrapped");
    }

    #[test]
    fn hostname_upstreams_without_any_bootstrap_are_dropped_with_a_note() {
        let report =
            dns_conversion_from_clash_yaml("dns:\n  nameserver: [https://doh.pub/dns-query]\n").expect("degraded");
        assert!(
            report
                .notes
                .iter()
                .any(|note| note.contains("doh.pub") && note.contains("is dropped")),
            "{:?}",
            report.notes
        );
        let spec = report.spec.expect("spec");
        assert!(spec.servers.is_empty(), "{:?}", spec.servers);
        assert_eq!(spec.domain_resolver, None);
    }

    #[test]
    fn unsupported_endpoint_transports_are_dropped_with_notes() {
        let report = dns_conversion_from_clash_yaml(
            "dns:\n  nameserver: ['tcp://1.1.1.1', 'http://1.1.1.1', 'rhey://1.1.1.1', 'https://1.1.1.1/dns-query?edns=1']\n",
        )
        .expect("degraded");
        assert!(
            report.notes.iter().any(|n| n.contains("DNS-over-TCP")),
            "{:?}",
            report.notes
        );
        assert!(
            report.notes.iter().any(|n| n.contains("DNS-over-HTTP")),
            "{:?}",
            report.notes
        );
        assert!(
            report
                .notes
                .iter()
                .any(|n| n.contains("unsupported DNS scheme \"rhey\"")),
            "{:?}",
            report.notes
        );
        assert!(
            report.notes.iter().any(|n| n.contains("query parameters")),
            "{:?}",
            report.notes
        );
        let spec = report.spec.expect("spec");
        assert_eq!(spec.servers.len(), 1);
        assert_eq!(spec.servers[0].server.as_deref(), Some("1.1.1.1"));
    }

    #[test]
    fn fake_ip_without_any_upstream_still_generates_a_startable_spec() {
        // sing-box refuses `default server cannot be fakeip`.
        let spec = spec_from_clash_yaml("dns:\n  enable: true\n  enhanced-mode: fake-ip\n")
            .unwrap()
            .unwrap();
        assert_eq!(spec.servers[0].kind, DnsServerKind::Local);
        assert_eq!(spec.servers[1].kind, DnsServerKind::Fakeip);
        let section = build_dns_section(&spec).unwrap().unwrap();
        assert_eq!(section["servers"][0]["type"], "local");
        assert_eq!(section["servers"][1]["type"], "fakeip");
    }

    #[test]
    fn unknown_dns_fields_and_disabled_dns_are_reported_not_fatal() {
        let report = dns_conversion_from_clash_yaml(
            "dns:\n  enable: true\n  ipv6: false\n  bogus-policy: yes\n  nameserver: [1.1.1.1]\n",
        )
        .expect("degraded");
        for expected in ["dns.ipv6:", "dns.bogus-policy: unknown profile DNS field"] {
            assert!(
                report.notes.iter().any(|note| note.contains(expected)),
                "{:?}",
                report.notes
            );
        }
        let disabled = dns_conversion_from_clash_yaml("dns:\n  enable: false\n  nameserver: [1.1.1.1]\n").unwrap();
        assert!(disabled.spec.is_none());
        assert!(
            disabled.notes.iter().any(|n| n.starts_with("dns.enable:")),
            "{:?}",
            disabled.notes
        );
    }

    #[test]
    fn invalid_fake_ip_range_falls_back_to_the_sing_box_default() {
        let spec = spec_from_clash_yaml(
            "dns:\n  nameserver: [1.1.1.1]\n  enhanced-mode: fake-ip\n  fake-ip-range: not-a-cidr\n",
        )
        .unwrap()
        .unwrap();
        let fakeip = spec
            .servers
            .iter()
            .find(|s| s.kind == DnsServerKind::Fakeip)
            .expect("fakeip");
        assert!(fakeip.inet4_range.is_none());
        assert_eq!(
            build_dns_section(&spec).unwrap().unwrap()["servers"][1]["inet4_range"],
            DEFAULT_FAKEIP_V4
        );
    }

    #[test]
    fn structural_dns_damage_is_still_fatal() {
        for (yaml, expected) in [
            ("dns: [1.1.1.1]\n", "dns must be a mapping"),
            ("dns:\n  nameserver: {a: 1}\n", "dns.nameserver entries must be strings"),
            (
                "dns:\n  nameserver-policy: [1.1.1.1]\n",
                "nameserver-policy must be a mapping",
            ),
            ("dns:\n  enable: yes-please\n", "dns.enable must be a boolean"),
            ("dns:\n  ipv6: 6\n", "dns.ipv6 must be a boolean"),
            (
                "dns:\n  enhanced-mode: [fake-ip]\n",
                "dns.enhanced-mode must be a string",
            ),
        ] {
            let error = dns_conversion_from_clash_yaml(yaml)
                .err()
                .unwrap_or_else(|| panic!("{yaml} must fail"));
            assert!(error.contains(expected), "{yaml}: {error}");
        }
    }

    #[test]
    fn dns_notes_never_echo_credentials_or_tokens() {
        let report = dns_conversion_from_clash_yaml(
            "dns:\n  nameserver: ['https://user:secret@resolver.example/dns-query?token=private', 'https://resolver.example/dns-query#weird-secret-option']\n",
        )
        .expect("degraded");
        assert!(
            report.notes.iter().any(|note| note.contains("URL credentials")),
            "{:?}",
            report.notes
        );
        let rendered = report.notes.join("\n");
        assert!(!rendered.contains("secret@"), "{rendered}");
        assert!(!rendered.contains("token=private"), "{rendered}");
        assert!(!rendered.contains("user:"), "{rendered}");
        // Only the host is quoted, never the whole URL.
        assert!(rendered.contains("resolver.example"), "{rendered}");
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
                tls_insecure: None,
                inet4_range: None,
                inet6_range: None,
            }
        );
        let parsed_h3 = parse_server_spec("h3|dns-h3|dns.google|443|").expect("h3");
        assert_eq!(parsed_h3.kind, DnsServerKind::H3);
        assert!(parsed_h3.kind.is_remote());
        assert_eq!(parsed_h3.server.as_deref(), Some("dns.google"));
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
