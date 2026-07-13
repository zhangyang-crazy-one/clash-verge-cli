// SSRF host check: resolves URL host to IPs and blocks private/loopback/link-local ranges.
// Allowlist: user may whitelist specific hosts.

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

/// Check whether a URL host is safe to fetch.
///
/// Returns Ok(()) if the host resolves to a public IP or is in the allowlist.
/// Returns Err with a description if the host resolves to a blocked range.
pub fn check_url_host(url: &str, allowlist: &[String]) -> Result<(), String> {
    let host = extract_host(url)?;

    // Allowlist check (before DNS resolution)
    if allowlist.iter().any(|a| a == &host) {
        return Ok(());
    }

    // Resolve host to IPs
    let addrs: Vec<_> = format!("{host}:0")
        .to_socket_addrs()
        .map_err(|e| format!("DNS resolution failed for '{host}': {e}"))?
        .collect();

    if addrs.is_empty() {
        return Err(format!("host '{host}' resolved to no addresses"));
    }

    for addr in &addrs {
        let ip = addr.ip();
        for (predicate, label) in BLOCKED_RANGES {
            if predicate(&ip) {
                return Err(format!("SSRF blocked: host '{host}' resolves to {label} address {ip}"));
            }
        }
    }

    Ok(())
}

/// Extract the hostname from a URL string.
fn extract_host(url: &str) -> Result<String, String> {
    let parsed = url::Url::parse(url).map_err(|e| format!("invalid URL: {e}"))?;
    parsed
        .host_str()
        .map(|h| h.to_string())
        .ok_or_else(|| "URL has no host".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_loopback_blocked() {
        let result = check_url_host("http://127.0.0.1:8080/test", &[]);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("loopback"));
    }

    #[test]
    fn test_private_192_blocked() {
        let result = check_url_host("http://192.168.1.1/config", &[]);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("private"));
    }

    #[test]
    fn test_private_10_blocked() {
        let result = check_url_host("https://10.0.0.5/sub", &[]);
        assert!(result.is_err());
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
