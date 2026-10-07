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
}
