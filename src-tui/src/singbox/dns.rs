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
//! `fallback-filter`, `listen`, …) is dropped with an
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
    /// Interface used by a direct DNS dialer (Clash `#eth0` when no proxy
    /// with that name exists). Kept optional for stored DNS compatibility.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bind_interface: Option<String>,
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

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DnsRuleSpec {
    /// Server tag the matched queries are routed to.
    pub server: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub query_type: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub domain: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub domain_regex: Vec<String>,
    /// Logical AND of matchers, used to retain nameserver-policy when a
    /// fake-IP filter requests real answers. Children contain no actions.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conditions: Vec<DnsRuleSpec>,
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
            && self.domain.is_empty()
            && self.domain_regex.is_empty()
            && self.conditions.is_empty()
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
    /// Only bootstraps upstream hostnames; node dialing uses real policy
    /// resolvers or the business final server independently.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domain_resolver: Option<String>,
    /// The server unmatched queries are sent to (`dns.final`). Without it
    /// sing-box uses `servers[0]`, which during a profile conversion is the
    /// plaintext `default-nameserver` bootstrap — that would silently
    /// downgrade every normal lookup to cleartext DNS. Never a fake-IP pool.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_server: Option<String>,
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
            if let Some(interface) = &server.bind_interface
                && (!server.kind.is_remote()
                    || !valid_interface_name(interface)
                    || server
                        .detour
                        .as_deref()
                        .is_some_and(|tag| !tag.is_empty() && tag != "direct"))
            {
                return Err(format!(
                    "dns server {:?}: bind_interface requires a valid interface and a direct remote dialer",
                    server.tag
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
        if let Some(final_server) = &self.final_server {
            let server = self
                .servers
                .iter()
                .find(|server| &server.tag == final_server)
                .ok_or_else(|| {
                    // sing-box 1.14.2 accepts this at `check` and only fails
                    // at startup ("default DNS server not found"), so the
                    // core would die on a config we generated.
                    format!("dns.final references unknown DNS server {final_server:?}")
                })?;
            if server.kind == DnsServerKind::Fakeip {
                return Err("a fake-IP server cannot be the default DNS server (dns.final)".into());
            }
        } else if self
            .servers
            .first()
            .is_some_and(|server| server.kind == DnsServerKind::Fakeip)
        {
            // Without `final`, sing-box defaults to servers[0] and refuses a
            // fake-IP pool there ("default server cannot be fakeip").
            return Err(format!(
                "dns server {:?}: a fake-IP pool needs `final` to point at a real upstream",
                self.servers[0].tag
            ));
        }
        for rule in &self.rules {
            if rule.is_empty() {
                return Err(format!("dns rule for {:?} has no match conditions", rule.server));
            }
            if !tags.contains(&rule.server) {
                return Err(format!("dns rule targets unknown server {:?}", rule.server));
            }
            validate_matcher(rule)?;
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
    let fake_filters = get("fake-ip-filter")
        .filter(|value| !value.is_null())
        .map(|value| as_string_list(value, "fake-ip-filter"))
        .transpose()?
        .unwrap_or_default();
    let fake_filter_mode = match get("fake-ip-filter-mode").filter(|value| !value.is_null()) {
        Some(value) => value.as_str().ok_or("dns.fake-ip-filter-mode must be a string")?,
        None => "blacklist",
    };
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
    let dial_targets = DnsDialTargets::from_profile(root, yaml)?;
    let mut tags = std::collections::HashMap::<String, String>::new();
    // Tags registered from `default-nameserver`. They exist only to resolve
    // the *hostnames* of the encrypted upstreams, so they must never become
    // the default query path (see `final_server` below).
    let mut bootstrap_tags: Vec<String> = Vec::new();
    // Tags registered from the profile's `nameserver` list, in order.
    let mut upstream_tags: Vec<String> = Vec::new();

    // `default-nameserver` is the bootstrap that resolves the *hostnames* of
    // the encrypted upstreams (doh.pub, dns.alidns.com, …). Without it the
    // core refuses to start, so it is generated first and referenced as
    // `domain_resolver` by every remote server addressed by a domain.
    if let Some(defaults) = get("default-nameserver").filter(|value| !value.is_null()) {
        let entries = as_string_list(defaults, "default-nameserver")?;
        for entry in &entries {
            if let Some(tag) = add_bootstrap_server(entry, &mut spec, &mut tags, &mut report) {
                bootstrap_tags.push(tag);
            }
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
        for (index, endpoint) in as_string_list(nameservers, "nameserver")?.iter().enumerate() {
            if let Some(tag) = add_clash_dns_endpoint(
                endpoint,
                &format!("nameserver[{index}]"),
                &dial_targets,
                &mut spec,
                &mut tags,
                &mut report,
            ) {
                upstream_tags.push(tag);
            }
        }
    }
    if let Some(policy) = get("nameserver-policy").filter(|value| !value.is_null()) {
        let Some(policy) = policy.as_mapping() else {
            return Err("dns.nameserver-policy must be a mapping".into());
        };
        // Mihomo's domain trie chooses static labels before '*' before
        // subdomain wildcards, regardless of YAML insertion order.
        let mut policies: Vec<_> = policy.iter().collect();
        policies.sort_by(|(left, _), (right, _)| {
            policy_priority(right.as_str().unwrap_or_default()).cmp(&policy_priority(left.as_str().unwrap_or_default()))
        });
        for (pattern, endpoints) in policies {
            let Some(pattern) = pattern.as_str() else {
                return Err("dns.nameserver-policy keys must be strings".to_string());
            };
            let entries = as_string_list(endpoints, &format!("nameserver-policy.{pattern}"))?;
            let Some((field, value)) = parse_clash_dns_policy_pattern(pattern) else {
                report.note(
                    format!("nameserver-policy.{pattern}"),
                    "unsupported matcher; use an exact domain, +.suffix, *.label, .suffix, or keyword:",
                );
                continue;
            };
            // A policy with no server (or only dropped servers) simply does
            // not apply; indexing the list here used to panic.
            let Some(first) = entries.first() else {
                report.note(
                    format!("nameserver-policy.{pattern}"),
                    "the server list is empty, so the policy is not applied",
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
            let Some(tag) = add_clash_dns_endpoint(
                first,
                &format!("nameserver-policy.{pattern}[0]"),
                &dial_targets,
                &mut spec,
                &mut tags,
                &mut report,
            ) else {
                report.note(
                    format!("nameserver-policy.{pattern}"),
                    "the first server was dropped and the remaining servers are not used, so the policy is not applied",
                );
                continue;
            };
            let mut rule = DnsRuleSpec {
                server: tag,
                query_type: Vec::new(),
                domain_suffix: Vec::new(),
                domain_keyword: Vec::new(),
                ip_cidr: Vec::new(),
                ..Default::default()
            };
            match field {
                "domain" => rule.domain.push(value),
                "domain_regex" => rule.domain_regex.push(value),
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
                bind_interface: None,
                tls_insecure: None,
                inet4_range,
                inet6_range,
            });
        }
        mode => report.note(
            "enhanced-mode",
            format!("unsupported value {mode:?}; treated as redir-host (no fake-IP pool)"),
        ),
    }

    // Whether the profile asked for any DNS policy at all — a block that is
    // dropped because nothing survived still has to be reported.
    let requested_servers = spec.servers.len();

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

    // The default query path is the profile's own upstream, never the
    // plaintext bootstrap: sing-box would otherwise default to `servers[0]`
    // and quietly downgrade every unmatched lookup to cleartext DNS.
    upstream_tags.retain(|tag| spec.servers.iter().any(|server| &server.tag == tag));
    let final_server = upstream_tags
        .iter()
        .find(|tag| {
            spec.servers
                .iter()
                .any(|server| &server.tag == *tag && server.kind.is_remote())
        })
        .or_else(|| {
            upstream_tags.iter().find(|tag| {
                spec.servers
                    .iter()
                    .any(|server| &server.tag == *tag && server.kind != DnsServerKind::Fakeip)
            })
        })
        .cloned();
    match &final_server {
        Some(tag) => spec.final_server = Some(tag.clone()),
        None => {
            // Fail visible, not silently wrong: with no usable upstream the
            // only way to reach it as `final` would be the bootstrap
            // (`system://` in `default-nameserver` counts as a bootstrap
            // here), which is exactly the plaintext downgrade we refuse.
            // Emitting nothing keeps the caller's sidecar/default resolver,
            // so the profile's DNS block is dropped with a note instead.
            if requested_servers > 0 || !spec.rules.is_empty() || get("nameserver").is_some() {
                let described = spec
                    .servers
                    .iter()
                    .map(|server| {
                        if bootstrap_tags.contains(&server.tag) {
                            format!("{} (bootstrap from default-nameserver)", server.kind.as_str())
                        } else {
                            format!("{} ({})", server.kind.as_str(), server.tag)
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                report.note(
                    "nameserver",
                    format!(
                        "no usable upstream resolver is left, so the profile's DNS block is dropped \
                         (available: {described}); the stored/default resolver is used instead \
                         rather than routing normal queries through the plaintext bootstrap"
                    ),
                );
            }
            return Ok(report);
        }
    }

    if spec.servers.iter().any(|server| server.kind == DnsServerKind::Fakeip) {
        compile_fake_ip_filters(&mut spec, &fake_filters, fake_filter_mode, &mut report)?;
    } else if !fake_filters.is_empty() || get("fake-ip-filter-mode").is_some() {
        report.note("fake-ip-filter", "ignored; enhanced-mode is not fake-ip");
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
        .find(|server| {
            server.kind == DnsServerKind::Udp
                && server.server.as_deref().is_some_and(looks_like_ip)
                && server.detour.as_deref().is_none_or(|tag| tag == "direct")
        })
        .map(|server| server.tag.clone())
}

/// Register one `default-nameserver` entry as a plain-UDP bootstrap server,
/// returning its tag. Bootstrap servers only resolve upstream hostnames.
fn add_bootstrap_server(
    entry: &str,
    spec: &mut DnsConfigSpec,
    tags: &mut std::collections::HashMap<String, String>,
    report: &mut DnsConversionReport,
) -> Option<String> {
    let entry = entry.trim();
    if entry.is_empty() || matches!(entry, "system" | "system://") {
        return None;
    }
    let (address, port) = split_host_port(entry);
    if !looks_like_ip(&address) {
        report.note(
            "default-nameserver",
            format!("{entry:?} is not an IP literal and is dropped from the bootstrap list"),
        );
        return None;
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
            bind_interface: None,
            tls_insecure: None,
            inet4_range: None,
            inet6_range: None,
        },
    );
    // The first IP-literal entry wins; a `system` entry is only used when the
    // profile offers nothing else, so the bootstrap stays a real upstream.
    if spec.domain_resolver.is_none() {
        spec.domain_resolver = Some(tag.clone());
    }
    Some(tag)
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
            bind_interface: None,
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
    key: &str,
    dial_targets: &DnsDialTargets,
    spec: &mut DnsConfigSpec,
    tags: &mut std::collections::HashMap<String, String>,
    report: &mut DnsConversionReport,
) -> Option<String> {
    let endpoint = endpoint.trim();
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
    let mut detour = None;
    let mut bind_interface = None;
    // Go's url.Parse decodes the fragment before Mihomo splits it on '&'.
    // Do the same; '+' remains literal and commas are part of a name.
    if fragment.match_indices('%').any(|(index, _)| {
        fragment
            .as_bytes()
            .get(index + 1..index + 3)
            .is_none_or(|digits| !digits.iter().all(u8::is_ascii_hexdigit))
    }) {
        report.note(
            key,
            format!("{host}: invalid percent-encoded fragment; the entry is dropped"),
        );
        return None;
    }
    let Ok(fragment) = percent_encoding::percent_decode_str(fragment).decode_utf8() else {
        report.note(key, format!("{host}: invalid UTF-8 fragment; the entry is dropped"));
        return None;
    };
    let mut selector = None;
    for option in fragment.split('&').filter(|part| !part.trim().is_empty()) {
        let Some((name, value)) = option.split_once('=') else {
            if selector.is_some_and(|previous| previous != option) {
                report.note(
                    key,
                    format!("{host}: multiple proxy/interface selectors are unsupported; the entry is dropped"),
                );
                return None;
            }
            selector = Some(option);
            continue;
        };
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
                report.note(
                    key,
                    format!(
                        "{host}: unsupported '#{other}' parameter is dropped; it is not a proxy/interface selector"
                    ),
                );
            }
        }
    }
    if let Some(selector) = selector {
        if selector == "RULES" {
            report.note(key, format!("{host}: '#RULES' routing semantics are unsupported; the entry is dropped instead of choosing an outbound"));
            return None;
        } else if selector == "DIRECT" {
            // New-format DNS servers use a direct dialer without detour.
        } else if dial_targets.converted.contains(selector) {
            detour = Some(selector.to_string());
        } else if dial_targets.proxy_names.contains(selector) {
            report.note(key, format!("{host}: selected proxy/group cannot be emitted (unsupported or reserved tag); the entry is dropped instead of binding an interface"));
            return None;
        } else if valid_interface_name(selector) {
            bind_interface = Some(selector.to_string());
        } else {
            report.note(key, format!("{host}: selector is neither a converted proxy/group nor a valid Linux interface name; the entry is dropped"));
            return None;
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
                detour: detour.clone(),
                bind_interface,
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
    let Some(server) = url.host_str().map(|host| host.trim_matches(['[', ']']).to_string()) else {
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
            detour,
            bind_interface,
            tls_insecure: insecure.then_some(true),
            inet4_range: None,
            inet6_range: None,
        },
    ))
}

struct DnsDialTargets {
    proxy_names: std::collections::HashSet<String>,
    converted: std::collections::HashSet<String>,
}

impl DnsDialTargets {
    fn from_profile(root: &serde_yaml_ng::Mapping, yaml: &str) -> Result<Self, String> {
        let mut proxy_names: std::collections::HashSet<String> = ["proxies", "proxy-groups"]
            .into_iter()
            .flat_map(|key| {
                root.get(Yaml::String(key.into()))
                    .and_then(Yaml::as_sequence)
                    .into_iter()
                    .flatten()
            })
            .filter_map(|entry| {
                entry
                    .get(Yaml::String("name".into()))
                    .and_then(Yaml::as_str)
                    .map(str::to_string)
            })
            .collect();
        let profile = super::convert::convert_profile(yaml)?;
        // outbound_tags() includes internal route aliases such as `direct`
        // and `block`; only actual declared/emittable proxies are DNS detours.
        let converted = profile
            .outbounds
            .iter()
            .filter_map(|outbound| outbound.get("tag").and_then(Value::as_str))
            .chain(profile.groups.iter().map(|group| group.name.as_str()))
            .filter(|tag| proxy_names.contains(*tag))
            .filter(|tag| !matches!(tag.to_ascii_uppercase().as_str(), "DIRECT" | "REJECT" | "BLOCK"))
            .map(str::to_string)
            .collect();
        // Mihomo synthesizes these case-sensitive proxy names. If no group
        // was emitted for one, report the unsupported policy, never bind it
        // as an interface. An explicitly declared GLOBAL group can survive.
        proxy_names.extend(["GLOBAL", "REJECT", "REJECT-DROP", "COMPATIBLE", "PASS", "PASS-RULE"].map(str::to_string));
        Ok(Self { proxy_names, converted })
    }
}

fn valid_interface_name(name: &str) -> bool {
    // Linux IFNAMSIZ includes the terminating NUL. Availability is checked
    // by the core on the target host, never by conversion on another host.
    !name.is_empty()
        && name.len() < 16
        && !matches!(name, "." | "..")
        && !name
            .chars()
            .any(|ch| ch.is_whitespace() || matches!(ch, '/' | ':' | '\0'))
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
        "{}|{}|{}|{}|{}|{}|{}",
        server.kind.as_str(),
        server.server.clone().unwrap_or_default(),
        server.server_port.unwrap_or_default(),
        server.path.clone().unwrap_or_default(),
        server.tls_insecure.unwrap_or_default(),
        server.detour.as_deref().unwrap_or_default(),
        server.bind_interface.as_deref().unwrap_or_default()
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
    let pattern = pattern.trim().to_ascii_lowercase();
    if let Some(keyword) = pattern.strip_prefix("keyword:") {
        return (!keyword.is_empty()).then(|| ("domain_keyword", keyword.to_string()));
    }
    let pattern = pattern.strip_prefix("domain:").unwrap_or(&pattern);
    if pattern.contains(':') || pattern.contains(',') || pattern.contains('/') {
        // geosite/rule-set/IP-response selectors cannot be represented by
        // a query-domain matcher. Never broaden them to a suffix.
        return None;
    }
    let suffix = pattern.strip_prefix("+.");
    let subdomains = pattern.strip_prefix('.');
    let labels: Vec<_> = suffix.or(subdomains).unwrap_or(pattern).split('.').collect();
    if labels
        .iter()
        .any(|label| label.is_empty() || label.contains('+') || (label.contains('*') && *label != "*"))
    {
        return None;
    }
    if let Some(suffix) = suffix.filter(|_| !pattern.contains('*')) {
        return Some(("domain_suffix", suffix.to_string()));
    }
    if suffix.is_some() || subdomains.is_some() || pattern.contains('*') {
        let body = labels
            .iter()
            .map(|label| {
                if *label == "*" {
                    "[^.]+".into()
                } else {
                    regex::escape(label)
                }
            })
            .collect::<Vec<_>>()
            .join("\\.");
        let prefix = if suffix.is_some() {
            "(?:[^.]+\\.)*"
        } else if subdomains.is_some() {
            "(?:[^.]+\\.)+"
        } else {
            ""
        };
        return Some(("domain_regex", format!("^{prefix}{body}$")));
    }
    Some(("domain", pattern.to_string()))
}

fn policy_priority(pattern: &str) -> Vec<(u8, String)> {
    if pattern.starts_with("keyword:") {
        return Vec::new();
    }
    pattern
        .strip_prefix("domain:")
        .unwrap_or(pattern)
        .to_ascii_lowercase()
        .split('.')
        .rev()
        .map(|label| {
            (
                match label {
                    "" | "+" => 0,
                    "*" => 1,
                    _ => 2,
                },
                label.to_string(),
            )
        })
        .collect()
}

fn domain_matcher(field: &str, value: String) -> DnsRuleSpec {
    let mut rule = DnsRuleSpec::default();
    match field {
        "domain" => rule.domain.push(value),
        "domain_suffix" => rule.domain_suffix.push(value),
        "domain_keyword" => rule.domain_keyword.push(value),
        "domain_regex" => rule.domain_regex.push(value),
        _ => unreachable!("only domain matchers are parsed"),
    }
    rule
}

/// Compile client fake-IP decisions ahead of normal DNS rules. Real-IP
/// decisions retain the matching nameserver-policy rather than bypassing it
/// through dns.final. Node dialing ignores these A/AAAA-only decisions.
fn compile_fake_ip_filters(
    spec: &mut DnsConfigSpec,
    filters: &[String],
    mode: &str,
    report: &mut DnsConversionReport,
) -> Result<(), String> {
    if !matches!(mode, "blacklist" | "whitelist" | "rule") {
        return Err(format!(
            "dns.fake-ip-filter-mode {mode:?} is unsupported (use blacklist, whitelist, or rule)"
        ));
    }
    let final_server = spec
        .final_server
        .clone()
        .ok_or("fake-IP filters require a real final DNS server")?;
    let policies = spec.rules.clone();
    let mut decisions = Vec::new();
    for (index, entry) in filters.iter().enumerate() {
        let (matcher, fake) = if mode == "rule" {
            let parts: Vec<_> = entry.split(',').map(str::trim).collect();
            let parsed = match parts.as_slice() {
                ["MATCH", target] => Some((DnsRuleSpec::default(), *target)),
                [kind, value, target] => {
                    let field = match *kind {
                        "DOMAIN" => Some("domain"),
                        "DOMAIN-SUFFIX" => Some("domain_suffix"),
                        "DOMAIN-KEYWORD" => Some("domain_keyword"),
                        "DOMAIN-REGEX" if regex::Regex::new(value).is_ok() => Some("domain_regex"),
                        _ => None,
                    };
                    field.filter(|_| !value.is_empty()).map(|field| {
                        let value = if field == "domain_regex" {
                            value.to_string()
                        } else {
                            value.to_ascii_lowercase()
                        };
                        (domain_matcher(field, value), *target)
                    })
                }
                _ => None,
            };
            match parsed {
                Some((matcher, "fake-ip")) => (matcher, true),
                Some((matcher, "real-ip")) => (matcher, false),
                _ => {
                    report.note(
                        format!("fake-ip-filter[{index}]"),
                        format!("{entry:?}: unsupported rule or target; dropped"),
                    );
                    continue;
                }
            }
        } else {
            let Some((field, value)) = parse_clash_dns_policy_pattern(entry) else {
                report.note(
                    format!("fake-ip-filter[{index}]"),
                    format!("{entry:?}: unsupported matcher; dropped"),
                );
                continue;
            };
            if field == "domain_keyword" {
                report.note(
                    format!("fake-ip-filter[{index}]"),
                    format!("{entry:?}: keyword matchers require rule mode DOMAIN-KEYWORD; dropped"),
                );
                continue;
            }
            (domain_matcher(field, value), mode == "whitelist")
        };
        let mut matched = matcher;
        matched.query_type = vec!["A".into(), "AAAA".into()];
        if fake {
            matched.server = FAKEIP_SERVER_TAG.into();
            decisions.push(matched);
        } else {
            for policy in &policies {
                let mut condition = policy.clone();
                condition.server.clear();
                decisions.push(DnsRuleSpec {
                    server: policy.server.clone(),
                    conditions: vec![matched.clone(), condition],
                    ..Default::default()
                });
            }
            matched.server = final_server.clone();
            decisions.push(matched);
        }
    }
    if mode == "blacklist" || mode == "rule" {
        // Mihomo rule-mode defaults to fake-IP when no decision matches.
        decisions.push(DnsRuleSpec {
            server: FAKEIP_SERVER_TAG.into(),
            query_type: vec!["A".into(), "AAAA".into()],
            ..Default::default()
        });
    }
    decisions.extend(policies);
    spec.rules = decisions;
    Ok(())
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
                if let Some(detour) = s.detour.as_deref().filter(|tag| !tag.is_empty() && *tag != "direct") {
                    entry["detour"] = json!(detour);
                }
                if let Some(interface) = &s.bind_interface {
                    entry["bind_interface"] = json!(interface);
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
            let mut rule = render_matcher(r);
            rule["action"] = json!("route");
            rule["server"] = json!(r.server);
            rule
        })
        .collect();
    if !rules.is_empty() {
        section["rules"] = Value::Array(rules);
    }
    if let Some(final_server) = &spec.final_server {
        // Explicit default query path: without it the core would use
        // `servers[0]`, i.e. the plaintext bootstrap.
        section["final"] = json!(final_server);
    }
    Ok(Some(section))
}

fn validate_matcher(rule: &DnsRuleSpec) -> Result<(), String> {
    if !rule.conditions.is_empty()
        && (!rule.query_type.is_empty()
            || !rule.domain.is_empty()
            || !rule.domain_regex.is_empty()
            || !rule.domain_suffix.is_empty()
            || !rule.domain_keyword.is_empty()
            || !rule.ip_cidr.is_empty())
    {
        return Err("logical DNS rules must put all match fields in conditions".into());
    }
    for pattern in &rule.domain_regex {
        regex::Regex::new(pattern).map_err(|error| format!("invalid DNS domain_regex {pattern:?}: {error}"))?;
    }
    for condition in &rule.conditions {
        if !condition.server.is_empty() {
            return Err("logical DNS conditions cannot contain a server action".into());
        }
        if condition.is_empty() {
            return Err("logical DNS rule has an empty condition".into());
        }
        validate_matcher(condition)?;
    }
    Ok(())
}

fn render_matcher(r: &DnsRuleSpec) -> Value {
    if !r.conditions.is_empty() {
        return json!({"type": "logical", "mode": "and", "rules": r.conditions.iter().map(render_matcher).collect::<Vec<_>>()});
    }
    let mut rule = json!({});
    for (key, values) in [
        ("query_type", &r.query_type),
        ("domain", &r.domain),
        ("domain_regex", &r.domain_regex),
        ("domain_suffix", &r.domain_suffix),
        ("domain_keyword", &r.domain_keyword),
        ("ip_cidr", &r.ip_cidr),
    ] {
        if !values.is_empty() {
            rule[key] = json!(values);
        }
    }
    rule
}

/// Default real domain resolver for node dialing; upstream bootstrap stays
/// on the DNS server's own domain_resolver field. Fake-IP is never eligible.
pub fn default_domain_resolver(spec: &DnsConfigSpec) -> Option<String> {
    spec.final_server
        .iter()
        .chain(spec.domain_resolver.iter())
        .find(|tag| {
            spec.servers
                .iter()
                .any(|server| &server.tag == *tag && server.kind != DnsServerKind::Fakeip)
        })
        .cloned()
}

fn matches_domain(rule: &DnsRuleSpec, hostname: &str) -> bool {
    let hostname = hostname.trim_end_matches('.').to_ascii_lowercase();
    rule.domain.contains(&hostname)
        || rule
            .domain_suffix
            .iter()
            .any(|suffix| hostname == *suffix || hostname.ends_with(&format!(".{suffix}")))
        || rule.domain_keyword.iter().any(|keyword| hostname.contains(keyword))
        || rule
            .domain_regex
            .iter()
            .any(|pattern| regex::Regex::new(pattern).is_ok_and(|re| re.is_match(&hostname)))
}

/// Select policy resolvers for node hostnames before generation. IP literals
/// need no lookup. Reject DNS -> proxy -> resolver cycles, including group
/// members and outbound detour dependencies, before starting the core.
pub fn apply_outbound_domain_resolvers(
    spec: &DnsConfigSpec,
    outbounds: &mut [Value],
    groups: &[super::GroupSpec],
) -> Result<(), String> {
    if spec.is_empty() {
        return Ok(());
    }
    spec.validate()?;
    let fallback = default_domain_resolver(spec);
    for outbound in outbounds.iter_mut() {
        let Some(hostname) = outbound.get("server").and_then(Value::as_str) else {
            continue;
        };
        if looks_like_ip(hostname.trim_matches(['[', ']'])) {
            continue;
        }
        let resolver = spec
            .rules
            .iter()
            .find(|rule| {
                rule.query_type.is_empty()
                    && rule.conditions.is_empty()
                    && rule.ip_cidr.is_empty()
                    && matches_domain(rule, hostname)
                    && spec
                        .servers
                        .iter()
                        .any(|server| server.tag == rule.server && server.kind != DnsServerKind::Fakeip)
            })
            .map(|rule| rule.server.clone())
            .or_else(|| fallback.clone());
        if let Some(resolver) = resolver {
            outbound["domain_resolver"] = json!({"server": resolver});
        }
    }
    let mut graph = std::collections::HashMap::<String, Vec<String>>::new();
    for outbound in outbounds.iter() {
        let Some(tag) = outbound.get("tag").and_then(Value::as_str) else {
            continue;
        };
        let edges = graph.entry(format!("outbound:{tag}")).or_default();
        if let Some(detour) = outbound.get("detour").and_then(Value::as_str) {
            edges.push(format!("outbound:{detour}"));
        }
        if outbound
            .get("server")
            .and_then(Value::as_str)
            .is_some_and(|host| !looks_like_ip(host.trim_matches(['[', ']'])))
            && let Some(resolver) = outbound
                .get("domain_resolver")
                .and_then(|r| r.as_str().or_else(|| r.get("server").and_then(Value::as_str)))
        {
            edges.push(format!("dns:{resolver}"));
        }
    }
    for group in groups {
        graph.insert(
            format!("outbound:{}", group.name),
            group
                .members
                .iter()
                .map(|tag| format!("outbound:{}", if tag == "DIRECT" { "direct" } else { tag }))
                .collect(),
        );
    }
    for server in &spec.servers {
        let edges = graph.entry(format!("dns:{}", server.tag)).or_default();
        if let Some(detour) = server.detour.as_deref().filter(|tag| *tag != "direct") {
            edges.push(format!("outbound:{detour}"));
        }
        if server.kind.is_remote() && server.server.as_deref().is_some_and(|host| !looks_like_ip(host)) {
            let resolver = spec
                .domain_resolver
                .as_ref()
                .ok_or_else(|| format!("DNS server {:?} needs a bootstrap domain_resolver", server.tag))?;
            edges.push(format!("dns:{resolver}"));
        }
    }
    fn visit(
        node: &str,
        graph: &std::collections::HashMap<String, Vec<String>>,
        visiting: &mut Vec<String>,
        done: &mut std::collections::HashSet<String>,
    ) -> Result<(), String> {
        if done.contains(node) {
            return Ok(());
        }
        if let Some(index) = visiting.iter().position(|n| n == node) {
            return Err(format!(
                "DNS/outbound resolver dependency cycle: {} -> {node}",
                visiting[index..].join(" -> ")
            ));
        }
        visiting.push(node.to_string());
        if let Some(edges) = graph.get(node) {
            for edge in edges {
                visit(edge, graph, visiting, done)?;
            }
        }
        visiting.pop();
        done.insert(node.to_string());
        Ok(())
    }
    let mut done = std::collections::HashSet::new();
    for node in graph.keys() {
        visit(node, &graph, &mut Vec::new(), &mut done)?;
    }
    Ok(())
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
        bind_interface: None,
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
        ..Default::default()
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
#[path = "dns_runtime_tests.rs"]
mod runtime_tests;

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;

    pub(super) fn generated_profile_dns(yaml: &str) -> (DnsConversionReport, Value) {
        let report = dns_conversion_from_clash_yaml(yaml).unwrap();
        let mut converted = super::super::convert::convert_profile(yaml).unwrap();
        let dns = report.spec.as_ref().and_then(|spec| {
            apply_outbound_domain_resolvers(spec, &mut converted.outbounds, &converted.groups).unwrap();
            build_dns_section(spec).unwrap()
        });
        let config = super::super::generate_config(&super::super::ConfigInput {
            outbounds: converted.outbounds,
            groups: converted.groups,
            mixed_port: 17890,
            enable_tun: false,
            tun: super::super::TunSettings {
                stack: "mixed".into(),
                mtu: 1500,
            },
            clash_api: super::super::ClashApiSettings {
                listen: "127.0.0.1:19090".parse().unwrap(),
                secret: "fixture-secret".into(),
            },
            rule_sets: Vec::new(),
            route_rules: Vec::new(),
            dns,
        })
        .unwrap();
        super::super::config_gen::validate_native_config(&config).unwrap();
        (report, config)
    }

    #[test]
    fn dns_fragment_interface_is_not_a_dangling_detour() {
        let (_, config) = generated_profile_dns("dns:\n  nameserver: ['https://1.1.1.1/dns-query#eth0']\n");
        assert_eq!(config["dns"]["servers"][0]["bind_interface"], "eth0");
        assert!(config["dns"]["servers"][0].get("detour").is_none());
    }

    #[test]
    fn dns_fragment_rules_is_reported_without_fabricating_an_outbound() {
        let (report, config) = generated_profile_dns("dns:\n  nameserver: ['https://1.1.1.1/dns-query#RULES']\n");
        assert!(report.spec.is_none());
        assert!(config.get("dns").is_none());
        assert!(
            report
                .notes
                .iter()
                .any(|note| note.contains("nameserver[0]") && note.contains("RULES") && note.contains("unsupported")),
            "{:?}",
            report.notes
        );
        assert!(
            report.notes.iter().any(|note| note.contains("stored/default resolver")),
            "{:?}",
            report.notes
        );
    }

    #[test]
    fn dns_fragment_prefers_defined_proxies_and_groups_over_interfaces() {
        let (report, config) = generated_profile_dns(
            r#"
proxies:
  - {name: eth0, type: http, server: 127.0.0.1, port: 8080}
proxy-groups:
  - {name: 'Proxy 中', type: select, proxies: [eth0, DIRECT]}
  - {name: 'Proxy,Comma', type: select, proxies: [DIRECT]}
dns:
  nameserver:
    - https://1.1.1.1/dns-query#eth0
    - https://1.1.1.1/dns-query#Proxy%20%E4%B8%AD
    - https://1.1.1.1/dns-query#Proxy,Comma
    - https://1.1.1.1/dns-query#DIRECT
"#,
        );
        let servers = config["dns"]["servers"].as_array().unwrap();
        assert_eq!(servers.len(), 4);
        for (server, expected) in servers
            .iter()
            .zip([Some("eth0"), Some("Proxy 中"), Some("Proxy,Comma"), None])
        {
            assert_eq!(server.get("detour").and_then(Value::as_str), expected);
            assert!(server.get("bind_interface").is_none());
        }
        assert!(report.notes.is_empty(), "{:?}", report.notes);
    }

    #[test]
    fn dns_fragment_parameters_do_not_become_selectors() {
        let (_, config) = generated_profile_dns(
            r#"
dns:
  nameserver:
    - https://1.1.1.1/dns-query#eth0&skip-cert-verify=true&h3=true
    - https://1.1.1.1/dns-query#eth0=true&RULES=true&ecs=1.1.1.1/24
    - https://1.1.1.1/dns-query#h3
"#,
        );
        let servers = config["dns"]["servers"].as_array().unwrap();
        assert_eq!(servers[0]["type"], "h3");
        assert_eq!(servers[0]["bind_interface"], "eth0");
        assert_eq!(servers[0]["tls"]["insecure"], true);
        assert!(servers[1].get("detour").is_none());
        assert!(servers[1].get("bind_interface").is_none());
        assert_eq!(servers[2]["type"], "https");
        assert_eq!(servers[2]["bind_interface"], "h3");
        assert!(servers.iter().all(|s| s.get("detour").is_none()));
    }

    #[test]
    fn dns_fragment_drops_unsupported_entries_with_their_own_notes() {
        let (report, config) = generated_profile_dns(
            r#"
proxies:
  - {name: eth0, type: unknown, server: 127.0.0.1, port: 8080}
  - {name: RULES, type: http, server: 127.0.0.1, port: 8080}
dns:
  nameserver:
    - https://1.1.1.1/dns-query#eth0
    - https://1.1.1.1/dns-query#RULES
    - https://1.1.1.1/dns-query#eth0&lo
    - https://1.1.1.1/dns-query#eth0%26lo
    - https://1.1.1.1/dns-query#missing%20proxy
    - https://1.1.1.1/dns-query#DIRECT
  nameserver-policy:
    exact.example: https://9.9.9.9/dns-query#RULES
"#,
        );
        assert_eq!(config["dns"]["servers"].as_array().unwrap().len(), 1);
        assert!(report.spec.unwrap().rules.is_empty());
        for key in [
            "nameserver[0]",
            "nameserver[1]",
            "nameserver[2]",
            "nameserver[3]",
            "nameserver[4]",
            "nameserver-policy.exact.example[0]",
        ] {
            assert!(
                report
                    .notes
                    .iter()
                    .any(|note| note.starts_with(&format!("dns.{key}:")) && note.contains("dropped")),
                "{key}: {:?}",
                report.notes
            );
        }
        assert!(!report.notes.iter().any(|note| note.contains("nameserver[5]")));
    }

    #[test]
    fn dns_fragment_interfaces_are_distinct_and_stored_specs_remain_compatible() {
        let (report, _) = generated_profile_dns(
            "dns:\n  nameserver: ['1.1.1.1#eth0', '1.1.1.1#lo', '1.1.1.1#eth0', '1.1.1.1#DIRECT']\n",
        );
        let spec = report.spec.unwrap();
        assert_eq!(spec.servers.len(), 3, "interface must be part of server identity");
        let old: DnsServerSpec =
            serde_json::from_value(json!({"tag":"old", "kind":"udp", "server":"1.1.1.1"})).unwrap();
        assert!(old.bind_interface.is_none());
        assert!(serde_json::to_value(old).unwrap().get("bind_interface").is_none());
        let mut invalid = spec;
        invalid.servers[0].detour = Some("proxy".into());
        assert!(invalid.validate().unwrap_err().contains("direct remote dialer"));
    }

    #[test]
    fn dns_fragment_invalid_escapes_are_reported_instead_of_bound() {
        for fragment in ["eth%ZZ", "eth%", "%FF"] {
            let (report, config) = generated_profile_dns(&format!(
                "dns:\n  nameserver: ['https://1.1.1.1/dns-query#{fragment}']\n"
            ));
            assert!(report.spec.is_none());
            assert!(config.get("dns").is_none());
            assert!(
                report
                    .notes
                    .iter()
                    .any(|note| note.contains("nameserver[0]") && note.contains("invalid")),
                "{:?}",
                report.notes
            );
        }
    }

    #[test]
    fn dns_fragment_internal_tags_do_not_invent_profile_proxies() {
        for selector in ["direct", "block", "reject"] {
            let (_, config) = generated_profile_dns(&format!(
                "dns:\n  nameserver: ['https://1.1.1.1/dns-query#{selector}']\n"
            ));
            assert_eq!(config["dns"]["servers"][0]["bind_interface"], selector);
            assert!(config["dns"]["servers"][0].get("detour").is_none());
        }
        let report = dns_conversion_from_clash_yaml("proxies:\n  - {name: direct, type: http, server: 127.0.0.1, port: 8080}\ndns:\n  nameserver: ['https://1.1.1.1/dns-query#direct']\n").unwrap();
        assert!(
            report.spec.is_none(),
            "reserved-name proxy must not silently become the built-in direct dialer"
        );
        assert!(
            report.notes.iter().any(|note| note.contains("nameserver[0]")
                && note.contains("selected proxy/group")
                && note.contains("dropped")),
            "{:?}",
            report.notes
        );
    }

    #[test]
    fn dns_fragment_implicit_mihomo_policies_are_not_network_interfaces() {
        for selector in ["GLOBAL", "REJECT", "REJECT-DROP", "COMPATIBLE", "PASS", "PASS-RULE"] {
            let (report, config) = generated_profile_dns(&format!(
                "dns:\n  nameserver: ['https://1.1.1.1/dns-query#{selector}']\n"
            ));
            assert!(
                report.spec.is_none(),
                "{selector} requires an explicit supported policy mapping"
            );
            assert!(config.get("dns").is_none());
            assert!(
                report
                    .notes
                    .iter()
                    .any(|note| note.contains("selected proxy/group") && note.contains("dropped")),
                "{selector}: {:?}",
                report.notes
            );
        }
        let (_, config) = generated_profile_dns(
            "proxy-groups:\n  - {name: GLOBAL, type: select, proxies: [DIRECT]}\ndns:\n  nameserver: ['https://1.1.1.1/dns-query#GLOBAL']\n",
        );
        assert_eq!(config["dns"]["servers"][0]["detour"], "GLOBAL");
        assert!(config["dns"]["servers"][0].get("bind_interface").is_none());
    }

    fn real_server<'a>(spec: &'a DnsConfigSpec, address: &str) -> &'a str {
        &spec
            .servers
            .iter()
            .find(|s| s.server.as_deref() == Some(address))
            .unwrap()
            .tag
    }

    fn client_server<'a>(spec: &'a DnsConfigSpec, hostname: &str, query_type: &str) -> &'a str {
        fn matches(rule: &DnsRuleSpec, host: &str, query: &str) -> bool {
            if !rule.conditions.is_empty() {
                return rule.conditions.iter().all(|r| matches(r, host, query));
            }
            let has_domain = !rule.domain.is_empty()
                || !rule.domain_suffix.is_empty()
                || !rule.domain_keyword.is_empty()
                || !rule.domain_regex.is_empty();
            (rule.query_type.is_empty() || rule.query_type.iter().any(|q| q == query))
                && (!has_domain || matches_domain(rule, host))
        }
        spec.rules
            .iter()
            .find(|r| matches(r, hostname, query_type))
            .map(|r| r.server.as_str())
            .or(spec.final_server.as_deref())
            .unwrap()
    }

    #[test]
    fn fake_ip_blacklist_preserves_exact_wildcard_and_suffix_semantics() {
        let spec = spec_from_clash_yaml(
            r#"
dns:
  nameserver: [1.1.1.1]
  enhanced-mode: fake-ip
  fake-ip-filter: [exact.example, '*.lan', '.internal', '+.corp', 'xbox.*.example']
"#,
        )
        .unwrap()
        .unwrap();
        for query in ["A", "AAAA"] {
            for domain in [
                "exact.example",
                "host.lan",
                "a.b.internal",
                "corp",
                "x.corp",
                "xbox.one.example",
            ] {
                assert_eq!(
                    client_server(&spec, domain, query),
                    real_server(&spec, "1.1.1.1"),
                    "{domain}/{query}"
                );
            }
            for domain in [
                "sub.exact.example",
                "lan",
                "deep.host.lan",
                "internal",
                "notcorp",
                "xbox.two.one.example",
                "elsewhere.example",
            ] {
                assert_eq!(
                    client_server(&spec, domain, query),
                    FAKEIP_SERVER_TAG,
                    "{domain}/{query}"
                );
            }
        }
        assert_eq!(
            client_server(&spec, "elsewhere.example", "TXT"),
            real_server(&spec, "1.1.1.1")
        );
    }

    #[test]
    fn fake_ip_whitelist_and_rule_mode_keep_direction_and_order() {
        let whitelist = spec_from_clash_yaml("dns:\n  nameserver: [1.1.1.1]\n  enhanced-mode: fake-ip\n  fake-ip-filter-mode: whitelist\n  fake-ip-filter: ['+.example']\n").unwrap().unwrap();
        assert_eq!(client_server(&whitelist, "example", "A"), FAKEIP_SERVER_TAG);
        assert_eq!(
            client_server(&whitelist, "elsewhere.test", "A"),
            real_server(&whitelist, "1.1.1.1")
        );
        let spec = spec_from_clash_yaml(
            r#"
dns:
  nameserver: [1.1.1.1]
  nameserver-policy: {'+.example': 9.9.9.9}
  enhanced-mode: fake-ip
  fake-ip-filter-mode: rule
  fake-ip-filter:
    - DOMAIN,real.example,real-ip
    - DOMAIN-SUFFIX,example,fake-ip
    - DOMAIN-KEYWORD,clock,real-ip
    - MATCH,real-ip
"#,
        )
        .unwrap()
        .unwrap();
        assert_eq!(client_server(&spec, "real.example", "A"), real_server(&spec, "9.9.9.9"));
        assert_eq!(client_server(&spec, "sub.real.example", "AAAA"), FAKEIP_SERVER_TAG);
        assert_eq!(client_server(&spec, "clock.test", "A"), real_server(&spec, "1.1.1.1"));
        assert_eq!(client_server(&spec, "other.test", "A"), real_server(&spec, "1.1.1.1"));
        assert_eq!(
            client_server(&spec, "sub.real.example", "TXT"),
            real_server(&spec, "9.9.9.9")
        );
        let built = build_dns_section(&spec).unwrap().unwrap();
        assert_eq!(built["rules"][0]["type"], "logical");
        assert!(built["rules"][0]["rules"][0].get("server").is_none());
    }

    #[test]
    fn unsupported_fake_ip_entries_are_reported_individually() {
        let report = dns_conversion_from_clash_yaml("dns:\n  nameserver: [1.1.1.1]\n  enhanced-mode: fake-ip\n  fake-ip-filter-mode: whitelist\n  fake-ip-filter: ['geosite:cn', 'rule-set:private', '*bad.example', 'keyword:clock', exact.example]\n").unwrap();
        for index in 0..4 {
            assert!(
                report
                    .notes
                    .iter()
                    .any(|note| note.contains(&format!("fake-ip-filter[{index}]"))),
                "{:?}",
                report.notes
            );
        }
        let spec = report.spec.unwrap();
        assert_eq!(client_server(&spec, "exact.example", "A"), FAKEIP_SERVER_TAG);
        assert_ne!(client_server(&spec, "other.example", "A"), FAKEIP_SERVER_TAG);
        assert!(
            dns_conversion_from_clash_yaml(
                "dns:\n  nameserver: [1.1.1.1]\n  enhanced-mode: fake-ip\n  fake-ip-filter-mode: surprise\n"
            )
            .unwrap_err()
            .contains("fake-ip-filter-mode")
        );
    }

    #[test]
    fn nodes_use_policy_or_business_dns_and_literals_need_no_lookup() {
        let spec = spec_from_clash_yaml(
            r#"
dns:
  default-nameserver: [223.5.5.5]
  nameserver: [https://resolver.example/dns-query]
  nameserver-policy:
    '+.example': 9.9.9.9
    node.example: 'udp://[::1]:5353'
  enhanced-mode: fake-ip
  fake-ip-filter: [node.example]
"#,
        )
        .unwrap()
        .unwrap();
        let mut nodes = vec![
            json!({"type":"http", "tag":"exact", "server":"node.example"}),
            json!({"type":"http", "tag":"suffix", "server":"a.example"}),
            json!({"type":"http", "tag":"miss", "server":"other.test"}),
            json!({"type":"http", "tag":"ipv4", "server":"127.0.0.1"}),
            json!({"type":"http", "tag":"ipv6", "server":"::1"}),
        ];
        apply_outbound_domain_resolvers(&spec, &mut nodes, &[]).unwrap();
        assert_eq!(nodes[0]["domain_resolver"]["server"], real_server(&spec, "::1"));
        assert_eq!(nodes[1]["domain_resolver"]["server"], real_server(&spec, "9.9.9.9"));
        assert_eq!(
            nodes[2]["domain_resolver"]["server"],
            spec.final_server.clone().unwrap()
        );
        assert!(nodes[3].get("domain_resolver").is_none());
        assert!(nodes[4].get("domain_resolver").is_none());
        assert_ne!(default_domain_resolver(&spec), spec.domain_resolver);
        let dns = build_dns_section(&spec).unwrap().unwrap();
        assert_eq!(
            dns["servers"]
                .as_array()
                .unwrap()
                .iter()
                .find(|s| s["server"] == "resolver.example")
                .unwrap()["domain_resolver"],
            spec.domain_resolver.clone().unwrap()
        );
    }

    #[test]
    fn resolver_detour_cycles_through_nodes_groups_and_dependencies_are_rejected() {
        for detour in ["node", "group", "chain"] {
            let spec = spec_from_clash_yaml(&format!(
                "proxies:\n  - {{name: node, type: http, server: node.example, port: 8080}}\n  - {{name: chain, type: http, server: 127.0.0.1, port: 8080}}\nproxy-groups:\n  - {{name: group, type: select, proxies: [DIRECT, node]}}\ndns:\n  default-nameserver: [1.1.1.1]\n  nameserver: ['udp://9.9.9.9#{detour}']\n"
            ))
            .unwrap()
            .unwrap();
            let mut nodes = vec![
                json!({"type":"http", "tag":"node", "server":"node.example"}),
                json!({"type":"http", "tag":"chain", "server":"127.0.0.1", "detour":"node"}),
            ];
            let groups = vec![super::super::GroupSpec {
                name: "group".into(),
                kind: super::super::GroupKind::Selector,
                members: vec!["DIRECT".into(), "node".into()],
            }];
            let error = apply_outbound_domain_resolvers(&spec, &mut nodes, &groups).unwrap_err();
            assert!(error.contains("dependency cycle"), "{detour}: {error}");
        }
        let spec = spec_from_clash_yaml("proxies:\n  - {name: node, type: http, server: 127.0.0.1, port: 8080}\ndns:\n  nameserver: ['udp://9.9.9.9#node']\n")
            .unwrap()
            .unwrap();
        let mut nodes = vec![json!({"type":"http", "tag":"node", "server":"127.0.0.1"})];
        apply_outbound_domain_resolvers(&spec, &mut nodes, &[]).unwrap();
        let mut invalid = spec;
        invalid.servers[0].server = Some("dns.example".into());
        invalid.domain_resolver = Some(invalid.servers[0].tag.clone());
        assert!(
            apply_outbound_domain_resolvers(&invalid, &mut nodes, &[])
                .unwrap_err()
                .contains("dependency cycle")
        );
    }

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
                    bind_interface: None,
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
                    bind_interface: None,
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
                    bind_interface: None,
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
                ..Default::default()
            }],
            domain_resolver: Some("dns-local".into()),
            final_server: Some("dns-remote".into()),
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
                bind_interface: None,
                tls_insecure: None,
                inet4_range: None,
                inet6_range: None,
            }],
            rules: Vec::new(),
            domain_resolver: Some("dns-fakeip".into()),
            final_server: None,
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
        let filtered = dns_conversion_from_clash_yaml(
            "dns:\n  nameserver: [1.1.1.1]\n  enhanced-mode: fake-ip\n  fake-ip-filter: ['+.lan']\n",
        )
        .expect("fake-ip-filter converts");
        assert!(!filtered.notes.iter().any(|note| note.contains("dns.fake-ip-filter:")));
        assert_eq!(filtered.spec.unwrap().rules[0].domain_suffix, ["lan"]);
    }

    #[test]
    fn real_world_clash_dns_degrades_with_notes_instead_of_aborting() {
        // Shape of a real subscription DNS block (Trimmed for length; every
        // field below is one a Clash provider actually emits.)
        let report = dns_conversion_from_clash_yaml(REAL_WORLD_DNS_YAML).expect("conversion must not abort");
        let notes = &report.notes;
        for expected in [
            "dns.listen:",
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
        assert!(
            spec.rules
                .iter()
                .any(|rule| rule.server == fakeip.tag && rule.query_type == ["A", "AAAA"])
        );
        assert_ne!(spec.rules[0].server, fakeip.tag, "real-IP exclusions precede fake-IP");

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
        assert_eq!(default_domain_resolver(&spec).as_deref(), spec.final_server.as_deref());
    }

    #[test]
    fn normal_queries_go_to_the_profile_nameserver_and_never_the_bootstrap() {
        // Review finding #1: sing-box defaults `final` to servers[0], which is
        // the plaintext bootstrap during a conversion — that silently routed
        // every unmatched lookup to cleartext DNS.
        let yaml = "dns:\n  default-nameserver: [180.76.76.76]\n  nameserver:\n    - https://doh.pub/dns-query\n    - 1.1.1.1\n";
        let report = dns_conversion_from_clash_yaml(yaml).expect("converted");
        let spec = report.spec.expect("spec");
        let bootstrap = spec.domain_resolver.clone().expect("bootstrap");
        assert_eq!(
            spec.servers[0].kind,
            DnsServerKind::Udp,
            "the bootstrap is registered first, which is exactly why `final` must be explicit"
        );
        assert_eq!(spec.servers[0].tag, bootstrap);
        assert_ne!(
            spec.final_server.as_deref(),
            Some(bootstrap.as_str()),
            "a plaintext bootstrap must never become the default query path"
        );
        let final_server = spec
            .servers
            .iter()
            .find(|s| &s.tag == spec.final_server.as_ref().unwrap())
            .expect("final");
        assert_eq!(final_server.kind, DnsServerKind::Https);
        assert_eq!(final_server.server.as_deref(), Some("doh.pub"));

        let section = build_dns_section(&spec).unwrap().unwrap();
        assert_eq!(section["final"], final_server.tag.as_str());
        assert_eq!(
            section["servers"][0]["domain_resolver"],
            json!(null),
            "bootstrap is IP-literal"
        );
        let doh = &section["servers"][1];
        assert_eq!(doh["server"], "doh.pub");
        assert_eq!(
            doh["domain_resolver"], bootstrap,
            "the DoH hostname is still bootstrapped through the plaintext resolver"
        );
    }

    #[test]
    fn final_follows_the_first_usable_upstream_including_local_resolvers() {
        // `tcp://` is dropped, so the first *surviving* upstream is the plain
        // UDP entry; a `system://` upstream may be the final when it is all
        // the profile has.
        let spec = spec_from_clash_yaml("dns:\n  nameserver: ['tcp://1.1.1.1', 8.8.8.8, system://]\n")
            .unwrap()
            .unwrap();
        assert_eq!(spec.final_server.as_deref(), Some(spec.servers[0].tag.as_str()));
        assert_eq!(spec.servers[0].server.as_deref(), Some("8.8.8.8"));

        let local_only =
            spec_from_clash_yaml("dns:\n  default-nameserver: [180.76.76.76]\n  nameserver: [system://]\n")
                .unwrap()
                .unwrap();
        let final_server = local_only
            .servers
            .iter()
            .find(|s| Some(&s.tag) == local_only.final_server.as_ref())
            .expect("final");
        assert_eq!(final_server.kind, DnsServerKind::Local);
    }

    #[test]
    fn nameserver_policy_server_never_becomes_the_default_query_path() {
        let spec = spec_from_clash_yaml(
            "dns:\n  default-nameserver: [223.5.5.5]\n  nameserver: [9.9.9.9]\n  nameserver-policy:\n    +.policy.example: 1.1.1.1\n",
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            spec.final_server.as_deref(),
            spec.servers
                .iter()
                .find(|s| s.server.as_deref() == Some("9.9.9.9"))
                .map(|s| s.tag.as_str()),
            "`nameserver-policy` targets are not defaults"
        );
        assert!(spec.servers.iter().any(|s| s.server.as_deref() == Some("1.1.1.1")));
    }

    #[test]
    fn profile_without_a_usable_upstream_drops_the_dns_block_with_a_note() {
        // Fail visible: a bootstrap must not silently inherit the default
        // query path, and a config whose only DNS content is a bootstrap would
        // ship cleartext lookups the profile never asked for.
        for (yaml, expected) in [
            (
                "dns:\n  enable: true\n  default-nameserver: [180.76.76.76]\n",
                "no usable upstream resolver is left",
            ),
            (
                "dns:\n  enable: true\n  default-nameserver: [180.76.76.76]\n  nameserver: ['tcp://1.1.1.1']\n",
                "no usable upstream resolver is left",
            ),
            (
                "dns:\n  enable: true\n  nameserver-policy:\n    +.example.com: 1.1.1.1\n",
                "no usable upstream resolver is left",
            ),
        ] {
            let report = dns_conversion_from_clash_yaml(yaml).expect("degraded");
            assert_eq!(report.spec, None, "{yaml} must not ship a DNS section");
            assert!(
                report.notes.iter().any(|note| note.contains(expected)),
                "{yaml}: {:?}",
                report.notes
            );
            assert!(
                report
                    .notes
                    .iter()
                    .any(|note| note.contains("stored/default resolver is used instead")),
                "{yaml}: {:?}",
                report.notes
            );
        }
        // The bootstrap is named in the note so the loss is traceable.
        let report = dns_conversion_from_clash_yaml("dns:\n  default-nameserver: [180.76.76.76]\n").expect("degraded");
        assert!(
            report
                .notes
                .iter()
                .any(|note| note.contains("bootstrap from default-nameserver")),
            "{:?}",
            report.notes
        );
        // An empty DNS block has nothing to report and changes nothing.
        let empty = dns_conversion_from_clash_yaml("dns:\n  enable: true\n").expect("empty");
        assert_eq!(empty.spec, None);
        assert!(empty.notes.is_empty(), "{:?}", empty.notes);
    }

    #[test]
    fn final_server_is_validated_before_generation() {
        let mut spec = sample_spec();
        // Unknown tag: sing-box 1.14.2 only fails at startup, so we refuse to
        // write it at all.
        spec.final_server = Some("ghost".into());
        let error = spec.validate().expect_err("unknown final");
        assert!(error.contains("dns.final"), "{error}");
        assert!(build_dns_section(&spec).is_err());

        // A fake-IP pool can never be the default server.
        let mut spec = sample_spec();
        spec.final_server = Some("dns-fakeip".into());
        assert!(spec.validate().expect_err("fakeip final").contains("dns.final"));

        // Without `final`, a fake-IP pool at index 0 would be the default and
        // the core would refuse the config ("default server cannot be fakeip").
        let mut spec = sample_spec();
        spec.servers.swap(0, 2);
        spec.final_server = None;
        assert!(spec.validate().expect_err("fakeip default").contains("`final`"));

        // Old sidecars without `final` keep working.
        let mut spec = sample_spec();
        spec.final_server = None;
        spec.validate().expect("legacy spec");
        assert!(build_dns_section(&spec).unwrap().unwrap().get("final").is_none());
    }

    #[test]
    fn empty_nameserver_policy_list_is_reported_instead_of_panicking() {
        // Review finding #2: `entries[0]` panicked on an empty list.
        let report = dns_conversion_from_clash_yaml(
            "dns:\n  default-nameserver: [223.5.5.5]\n  nameserver: [9.9.9.9]\n  nameserver-policy:\n    +.empty.example: []\n    '+.other.example': ''\n",
        )
        .expect("degraded");
        assert!(
            report
                .notes
                .iter()
                .any(|note| note.contains("dns.nameserver-policy.+.empty.example:") && note.contains("empty")),
            "{:?}",
            report.notes
        );
        let spec = report.spec.expect("spec");
        assert!(
            spec.rules.is_empty(),
            "an empty policy must not produce a rule: {:?}",
            spec.rules
        );
        assert_eq!(
            spec.final_server.as_deref(),
            spec.servers
                .iter()
                .find(|s| s.server.as_deref() == Some("9.9.9.9"))
                .map(|s| s.tag.as_str()),
            "an empty policy does not disturb the default query path"
        );
        let section = build_dns_section(&spec).unwrap().unwrap();
        assert!(section.get("rules").is_none(), "{section}");
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
            "dns:\n  default-nameserver: [223.5.5.5]\n  nameserver: [https://9.9.9.9/dns-query]\n  nameserver-policy:\n    +.example.com: ['https://1.1.1.1/dns-query', 'https://2.2.2.2/dns-query']\n",
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
            ("geosite:cn", "geosite"),
        ] {
            let yaml = format!(
                "dns:\n  default-nameserver: [223.5.5.5]\n  nameserver: [https://9.9.9.9/dns-query]\n  nameserver-policy:\n    {pattern}: 1.1.1.1\n"
            );
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
        // doh.pub cannot be resolved (no IP-literal bootstrap), so it is
        // dropped — and with it the last upstream, so the whole DNS block
        // goes away instead of shipping a config that cannot resolve anything.
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
        assert!(
            report
                .notes
                .iter()
                .any(|note| note.contains("no usable upstream resolver")),
            "{:?}",
            report.notes
        );
        assert_eq!(report.spec, None, "the DNS block is dropped, not shipped broken");
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
    fn fake_ip_without_any_upstream_drops_the_block_instead_of_defaulting_to_bootstrap() {
        // A fake-IP pool may never be the default server, and without an
        // upstream there is nothing else to point `final` at.
        let report =
            dns_conversion_from_clash_yaml("dns:\n  enable: true\n  enhanced-mode: fake-ip\n").expect("dropped");
        assert_eq!(report.spec, None);
        assert!(
            report
                .notes
                .iter()
                .any(|note| note.contains("no usable upstream resolver")),
            "{:?}",
            report.notes
        );
        // With an upstream it is generated again, and `final` keeps the pool
        // off the default path.
        let spec = spec_from_clash_yaml("dns:\n  enhanced-mode: fake-ip\n  nameserver: [1.1.1.1]\n")
            .unwrap()
            .unwrap();
        assert_eq!(spec.servers[1].kind, DnsServerKind::Fakeip);
        assert_eq!(spec.final_server.as_deref(), Some(spec.servers[0].tag.as_str()));
        let section = build_dns_section(&spec).unwrap().unwrap();
        assert_eq!(section["final"], spec.servers[0].tag.as_str());
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
        assert_eq!(default_domain_resolver(&spec).as_deref(), Some("dns-remote"));
        spec.final_server = Some("ghost-final".into());
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
                bind_interface: None,
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
