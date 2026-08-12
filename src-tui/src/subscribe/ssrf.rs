// SSRF host check: resolves URL host to IPs and blocks private/loopback/link-local ranges.
// Allowlist: user may whitelist specific hosts.

use std::fmt;
use std::net::{IpAddr, ToSocketAddrs};

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
    let addrs: Vec<_> = format!("{host}:0")
        .to_socket_addrs()
        .map_err(|source| CheckError::DnsFailed {
            host: host.clone(),
            source,
        })?
        .collect();

    if addrs.is_empty() {
        return Err(CheckError::NoAddress { host });
    }

    for addr in &addrs {
        let ip = addr.ip();
        for (predicate, label) in BLOCKED_RANGES {
            if predicate(&ip) {
                return Err(CheckError::Blocked {
                    host: host.clone(),
                    label,
                    ip,
                });
            }
        }
    }

    Ok(())
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
