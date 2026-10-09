//! Offline sing-box capability contract for the pinned compatibility target.
//!
//! This is schema-policy data, not a claim of live-core certification. Bump
//! the version and fixtures together when the supported config surface moves.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CapabilityMatrix {
    pub version: &'static str,
    pub route_rule_sets_nested: bool,
    pub route_rule_references: bool,
    pub client_fingerprint: bool,
    pub tls_reality: bool,
    pub structured_dns: bool,
    pub tun_gvisor: bool,
    pub native_json_preserved: bool,
}

pub const SING_BOX_1_14_2: CapabilityMatrix = CapabilityMatrix {
    version: "1.14.2",
    route_rule_sets_nested: true,
    route_rule_references: true,
    client_fingerprint: true,
    tls_reality: true,
    structured_dns: true,
    tun_gvisor: true,
    native_json_preserved: true,
};

/// How a clash proxy type's `udp:` flag maps onto the pinned sing-box
/// outbound.
///
/// Verified empirically against sing-box 1.14.2 (build `af6e64c3`, go1.26.8)
/// by driving a real socks inbound with a UDP ASSOCIATE toward each outbound
/// and reading the core's own debug log, plus `sing-box schema` for the
/// `network` field. The log line `outbound packet connection` (or, for
/// anytls, `outbound UoT packet connection`) means the outbound accepted the
/// UDP association; `router: UDP is not supported by outbound` means it did
/// not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UdpRelay {
    /// The outbound relays UDP by default AND accepts `network: ["tcp","udp"]`,
    /// so `udp: false` is exactly `network: "tcp"`.
    ///
    /// Verified for shadowsocks, vmess, vless, trojan, hysteria, hysteria2,
    /// tuic and socks: each logged an outbound packet connection, and
    /// `sing-box schema` lists `network` with enum `["tcp","udp"]` for each.
    NetworkToggle,
    /// The outbound relays UDP over its own TCP/TLS stream but has no
    /// `network` option, so `udp: false` cannot be expressed.
    ///
    /// Verified for anytls: the core logged
    /// `outbound/anytls[out]: outbound UoT packet connection to 1.1.1.1:53`
    /// (UDP-over-TCP), while `sing-box check` rejects
    /// `outbounds[0].network: json: unknown field "network"`.
    UdpOverTcp,
    /// The outbound has no UDP relay at all. Verified for http: the router
    /// logged `UDP is not supported by outbound: out`.
    NoRelay,
    /// Could not be exercised on the pinned build, so it is treated like
    /// [`UdpRelay::NoRelay`] for `udp: true` and refused for
    /// `udp: false`. Verified only as far as: the outbound initializes
    /// (given the TLS block it demands) — naive needs the cronet library,
    /// which this build does not embed (`FATAL initialize outbound[0]: cronet:
    /// library not found`).
    Unverified,
}

/// UDP capability of the sing-box outbound a clash proxy type converts to.
pub fn udp_relay(clash_type: &str) -> UdpRelay {
    match clash_type {
        "ss" | "vmess" | "vless" | "trojan" | "hysteria" | "hysteria2" | "tuic" | "socks5" => UdpRelay::NetworkToggle,
        "anytls" => UdpRelay::UdpOverTcp,
        "http" => UdpRelay::NoRelay,
        "naive" => UdpRelay::Unverified,
        _ => UdpRelay::Unverified,
    }
}

pub fn for_version(version: &str) -> Option<&'static CapabilityMatrix> {
    let version = version.strip_prefix('v').unwrap_or(version);
    match version {
        "1.14.2" => Some(&SING_BOX_1_14_2),
        _ => None,
    }
}

pub fn require_for_version(version: &str) -> Result<&'static CapabilityMatrix, String> {
    for_version(version).ok_or_else(|| {
        format!("sing-box {version} has no reviewed capability matrix; refusing version-dependent configuration")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pinned_matrix_is_explicit_and_unknown_versions_are_not_assumed_compatible() {
        let matrix = for_version("v1.14.2").expect("pinned matrix");
        assert!(matrix.route_rule_sets_nested);
        assert!(matrix.route_rule_references);
        assert!(matrix.client_fingerprint && matrix.tls_reality);
        assert!(matrix.structured_dns && matrix.tun_gvisor && matrix.native_json_preserved);
        assert!(for_version("1.15.0").is_none());
        assert!(for_version("vv1.14.2").is_none());
        assert!(
            require_for_version("1.15.0")
                .unwrap_err()
                .contains("no reviewed capability matrix")
        );
    }

    #[test]
    fn udp_relay_matches_the_verified_sing_box_1_14_2_matrix() {
        for type_with_toggle in [
            "ss",
            "vmess",
            "vless",
            "trojan",
            "hysteria",
            "hysteria2",
            "tuic",
            "socks5",
        ] {
            assert_eq!(
                udp_relay(type_with_toggle),
                UdpRelay::NetworkToggle,
                "{type_with_toggle}"
            );
        }
        // anytls relays UDP over TCP but cannot be restricted to TCP-only.
        assert_eq!(udp_relay("anytls"), UdpRelay::UdpOverTcp);
        assert_eq!(udp_relay("http"), UdpRelay::NoRelay);
        // Unreviewed types are never assumed to relay UDP.
        assert_eq!(udp_relay("naive"), UdpRelay::Unverified);
        assert_eq!(udp_relay("mieru"), UdpRelay::Unverified);
    }
}
