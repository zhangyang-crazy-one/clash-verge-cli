// SSRF host check: resolves URL host to IPs and blocks private/loopback/link-local ranges.
// Allowlist: user may whitelist specific hosts.

use std::fmt;
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::sync::Arc;

use reqwest::dns::{Addrs, Name, Resolve, Resolving};

type BlockedRange = (fn(&IpAddr) -> bool, &'static str);

const BLOCKED_RANGES: &[BlockedRange] = &[
    (|ip| ip.is_loopback(), "loopback"),
    (|ip| is_private(ip), "private"),
    (|ip| matches!(ip, IpAddr::V4(v4) if v4.is_link_local()), "link-local-v4"),
    (
        |ip| matches!(ip, IpAddr::V6(v6) if v6.is_unicast_link_local()),
        "link-local-v6",
    ),
    (|ip| matches!(ip, IpAddr::V6(v6) if is_ula_v6(v6)), "unique-local"),
    (|ip| ip.is_unspecified(), "unspecified"),
    (|ip| matches!(ip, IpAddr::V4(v4) if v4.octets()[0] == 0), "this-network"),
    (
        |ip| matches!(ip, IpAddr::V4(v4) if v4.octets()[0] == 100 && (64..=127).contains(&v4.octets()[1])),
        "shared-address",
    ),
    (
        |ip| matches!(ip, IpAddr::V6(v6) if v6.segments()[0] & 0xffc0 == 0xfec0),
        "site-local",
    ),
    (|ip| ip.is_multicast(), "multicast"),
    (|ip| matches!(ip, IpAddr::V4(v4) if v4.is_broadcast()), "broadcast"),
];

fn is_private(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_private(),
        IpAddr::V6(v6) => is_ula_v6(v6) || v6.is_unique_local(),
    }
}

fn is_ula_v6(ip: &std::net::Ipv6Addr) -> bool {
    (ip.segments()[0] & 0xfe00) == 0xfc00
}

/// Why a host failed the SSRF check.
///
/// `Blocked` is the only variant that means a private/loopback/link-local
/// address was actually resolved. The DNS variants mean no address could be
/// established at all. Callers that offer "trust this host" (e.g. the TUI
/// import flow) must match on `Blocked` so a transient DNS failure is never
/// presented as, or persisted as, a trusted host.
#[derive(Debug)]
pub enum CheckError {
    /// URL could not be parsed, or has no host.
    InvalidUrl(String),
    /// Host could not be resolved (DNS error).
    DnsFailed { host: String, source: std::io::Error },
    /// Host resolved but to zero addresses.
    NoAddress { host: String },
    /// Host resolved to an address in a blocked range.
    Blocked {
        host: String,
        label: &'static str,
        ip: IpAddr,
    },
}

impl fmt::Display for CheckError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CheckError::InvalidUrl(message) => write!(f, "{message}"),
            CheckError::DnsFailed { host, source } => {
                write!(f, "DNS resolution failed for '{host}': {source}")
            }
            CheckError::NoAddress { host } => write!(f, "host '{host}' resolved to no addresses"),
            CheckError::Blocked { host, label, ip } => {
                write!(f, "SSRF blocked: host '{host}' resolves to {label} address {ip}")
            }
        }
    }
}

impl std::error::Error for CheckError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            CheckError::DnsFailed { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// Check whether a URL host is safe to fetch.
///
/// Returns Ok(()) if the host resolves to a public IP or is in the allowlist.
/// Returns Err with a specific `CheckError` kind: `Blocked` means an
/// SSRF-blocked address was resolved, while the DNS variants mean the host
/// could not be resolved (and no blocked address exists).
pub fn check_url_host(url: &str, allowlist: &[String]) -> Result<(), CheckError> {
    let host = extract_host(url)?;

    // Allowlist check (before DNS resolution)
    if allowlist.iter().any(|a| a == &host) {
        return Ok(());
    }

    // Resolve host to IPs
    // URL hosts include brackets for IPv6, while socket tuples do not.
    if let Ok(ip) = host.trim_matches(['[', ']']).parse() {
        return check_ip(&host, ip);
    }
    let addrs: Vec<_> = (host.as_str(), 0)
        .to_socket_addrs()
        .map_err(|source| CheckError::DnsFailed {
            host: host.clone(),
            source,
        })?
        .collect();

    check_addresses(&host, &addrs, allowlist)
}

fn check_ip(host: &str, ip: IpAddr) -> Result<(), CheckError> {
    let classified = match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(ip),
        _ => ip,
    };
    for (predicate, label) in BLOCKED_RANGES {
        if predicate(&classified) {
            return Err(CheckError::Blocked {
                host: host.into(),
                label,
                ip,
            });
        }
    }
    Ok(())
}

fn check_addresses(host: &str, addrs: &[SocketAddr], allowlist: &[String]) -> Result<(), CheckError> {
    if addrs.is_empty() {
        return Err(CheckError::NoAddress { host: host.into() });
    }
    if !is_trusted(host, allowlist) {
        for addr in addrs {
            check_ip(host, addr.ip())?;
        }
    }
    Ok(())
}

fn is_trusted(host: &str, allowlist: &[String]) -> bool {
    allowlist.iter().any(|entry| entry == host)
}

/// Validate each request before reqwest can skip DNS for an IP literal.
/// Domain connections in direct mode are checked by `SafeResolver` instead.
/// Remote-DNS proxies cannot provide verifiable target addresses, so domain
/// targets in proxy modes require an explicit, per-host trust exemption.
pub fn check_fetch_url(url: &url::Url, allowlist: &[String], proxy_mode: bool) -> anyhow::Result<()> {
    if !matches!(url.scheme(), "http" | "https") {
        anyhow::bail!("subscription URL must use http or https");
    }
    let host = url
        .host_str()
        .ok_or_else(|| CheckError::InvalidUrl("URL has no host".into()))?;
    if is_trusted(host, allowlist) {
        return Ok(());
    }
    match url.host() {
        Some(url::Host::Ipv4(ip)) => check_ip(host, IpAddr::V4(ip))?,
        Some(url::Host::Ipv6(ip)) => check_ip(host, IpAddr::V6(ip))?,
        Some(url::Host::Domain(_)) if proxy_mode => {
            anyhow::bail!(
                "SSRF protection cannot verify remote proxy DNS for host '{host}'; add this specific host to option.trusted_hosts only if you trust it, or explicitly choose direct fetching"
            );
        }
        _ => {}
    }
    Ok(())
}

struct SystemResolver;

impl Resolve for SystemResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().to_owned();
        Box::pin(async move {
            let addrs: Vec<_> = tokio::net::lookup_host((host.as_str(), 0)).await?.collect();
            Ok(Box::new(addrs.into_iter()) as Addrs)
        })
    }
}

/// Checks the exact DNS answers handed to the direct connection, eliminating
/// the gap between a preflight DNS lookup and reqwest's independent lookup.
/// Do not install on proxy clients: selected proxy endpoints may be private.
pub struct SafeResolver {
    inner: Arc<dyn Resolve>,
    allowlist: Vec<String>,
}

impl SafeResolver {
    pub fn new(allowlist: &[String]) -> Self {
        Self {
            inner: Arc::new(SystemResolver),
            allowlist: allowlist.to_vec(),
        }
    }

    #[cfg(test)]
    pub(super) fn with_inner(inner: Arc<dyn Resolve>, allowlist: &[String]) -> Self {
        Self {
            inner,
            allowlist: allowlist.to_vec(),
        }
    }
}

impl Resolve for SafeResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().to_owned();
        let future = self.inner.resolve(name);
        let allowlist = self.allowlist.clone();
        Box::pin(async move {
            let addrs: Vec<_> = future.await?.collect();
            check_addresses(&host, &addrs, &allowlist)?;
            Ok(Box::new(addrs.into_iter()) as Addrs)
        })
    }
}

/// Extract the hostname from a URL string.
fn extract_host(url: &str) -> Result<String, CheckError> {
    let parsed = url::Url::parse(url).map_err(|e| CheckError::InvalidUrl(format!("invalid URL: {e}")))?;
    parsed
        .host_str()
        .map(|h| h.to_string())
        .ok_or_else(|| CheckError::InvalidUrl("URL has no host".to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn special_ranges_and_mapped_addresses_are_blocked_without_dns() {
        for host in [
            "0.1.2.3",
            "100.64.0.1",
            "100.127.255.254",
            "[fec0::1]",
            "[feff::1]",
            "[::ffff:127.0.0.1]",
            "[::ffff:10.1.2.3]",
            "[::ffff:100.64.0.1]",
            "224.0.0.1",
            "255.255.255.255",
        ] {
            let raw = format!("http://{host}/sub");
            assert!(
                matches!(check_url_host(&raw, &[]), Err(CheckError::Blocked { .. })),
                "{host}"
            );
            let url = url::Url::parse(&raw).unwrap();
            assert!(check_fetch_url(&url, &[], false).is_err(), "{host}");
            assert!(check_fetch_url(&url, &[], true).is_err(), "{host}");
            let trusted = vec![url.host_str().unwrap().into()];
            assert!(check_url_host(&raw, &trusted).is_ok(), "explicit trust exempts {host}");
            assert!(check_fetch_url(&url, &trusted, true).is_ok());
        }
        for host in [
            "100.63.255.254",
            "100.128.0.1",
            "8.8.8.8",
            "[::ffff:8.8.8.8]",
            "[2001:4860:4860::8888]",
        ] {
            assert!(check_url_host(&format!("http://{host}/sub"), &[]).is_ok(), "{host}");
        }
    }

    #[test]
    fn mixed_public_private_dns_answer_rejected_and_trust_exempts_exact_host_only() {
        let addrs = vec!["8.8.8.8:0".parse().unwrap(), "127.0.0.1:0".parse().unwrap()];
        assert!(matches!(
            check_addresses("mixed.test", &addrs, &[]),
            Err(CheckError::Blocked { .. })
        ));
        assert!(check_addresses("mixed.test", &addrs, &["mixed.test".into()]).is_ok());
        assert!(check_addresses("other.test", &addrs, &["mixed.test".into()]).is_err());
        assert!(matches!(
            check_addresses("mixed.test", &[], &["mixed.test".into()]),
            Err(CheckError::NoAddress { .. })
        ));
    }

    #[test]
    fn proxy_domains_need_explicit_trust_but_public_literals_do_not() {
        let domain = url::Url::parse("https://example.com/sub").unwrap();
        assert!(check_fetch_url(&domain, &[], false).is_ok());
        assert!(
            check_fetch_url(&domain, &[], true)
                .unwrap_err()
                .to_string()
                .contains("option.trusted_hosts")
        );
        assert!(check_fetch_url(&domain, &["example.com".into()], true).is_ok());
        let public = url::Url::parse("http://8.8.8.8/sub").unwrap();
        assert!(check_fetch_url(&public, &[], true).is_ok());
    }

    #[test]
    fn test_loopback_blocked() {
        let result = check_url_host("http://127.0.0.1:8080/test", &[]);
        assert!(matches!(result, Err(CheckError::Blocked { label: "loopback", .. })));
    }

    #[test]
    fn test_private_192_blocked() {
        let result = check_url_host("http://192.168.1.1/config", &[]);
        assert!(matches!(result, Err(CheckError::Blocked { label: "private", .. })));
    }

    #[test]
    fn test_private_10_blocked() {
        let result = check_url_host("https://10.0.0.5/sub", &[]);
        assert!(result.is_err());
    }

    #[test]
    fn test_ula_v6_blocked() {
        // IPv6 unique-local (fc00::/7) stays blocked: the trust flow depends
        // on this genuine blocked-address condition surviving. The first
        // matching range wins, so ULA v6 reports the "private" label
        // (`is_private` covers ULA); assert the address itself instead.
        let result = check_url_host("http://[fd00::1]/sub", &[]);
        assert!(matches!(
            result,
            Err(CheckError::Blocked { ip: IpAddr::V6(ip), .. })
                if (ip.segments()[0] & 0xfe00) == 0xfc00
        ));
    }

    #[test]
    fn test_dns_failure_is_not_a_block() {
        // `.invalid` is reserved (RFC 6761) and must never resolve, so this is
        // a DNS/no-address failure, not a blocked address. The typed error
        // keeps the two cases distinct so trust flows can rely on it.
        let result = check_url_host("http://host.invalid/sub", &[]);
        if let Err(CheckError::Blocked { .. }) = result {
            panic!("DNS failure must not be reported as an SSRF block");
        }
        // DnsFailed / NoAddress / InvalidUrl — or even Ok in a
        // wildcard-DNS environment — all prove the point: no blocked
        // address exists.
    }

    #[test]
    fn test_allowlist_bypasses_block() {
        let result = check_url_host("http://192.168.1.1/config", &["192.168.1.1".into()]);
        assert!(result.is_ok());
    }

    #[test]
    fn test_invalid_url_rejected() {
        let result = check_url_host("not-a-url", &[]);
        assert!(result.is_err());
    }
}
