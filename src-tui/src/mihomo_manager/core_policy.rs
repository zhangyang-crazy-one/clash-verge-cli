//! Offline compatibility policy shared by the mihomo and sing-box resolvers.

use std::cmp::Ordering;

pub const MIHOMO_POLICY_VERSION: &str = "1.19.32";
pub const SINGBOX_POLICY_VERSION: &str = "1.14.2";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    prerelease: Vec<Identifier>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Identifier {
    Numeric(u64),
    Text(String),
}

impl Version {
    pub fn parse(input: &str) -> anyhow::Result<Self> {
        let raw = input.strip_prefix('v').unwrap_or(input);
        let (without_build, build) = raw.split_once('+').unwrap_or((raw, ""));
        if raw.matches('+').count() > 1
            || (!build.is_empty()
                && build
                    .split('.')
                    .any(|part| part.is_empty() || !part.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')))
        {
            anyhow::bail!("malformed semantic version: {input}");
        }
        let (core, pre) = without_build.split_once('-').unwrap_or((without_build, ""));
        if raw.ends_with('-') || raw.ends_with('+') {
            anyhow::bail!("malformed semantic version: {input}");
        }
        let mut nums = core.split('.');
        let parse_num = |s: Option<&str>| -> anyhow::Result<u64> {
            let s = s.ok_or_else(|| anyhow::anyhow!("version must have major.minor.patch"))?;
            if s.is_empty() || (s.len() > 1 && s.starts_with('0')) || !s.bytes().all(|b| b.is_ascii_digit()) {
                anyhow::bail!("malformed semantic version: {input}");
            }
            s.parse()
                .map_err(|_| anyhow::anyhow!("malformed semantic version: {input}"))
        };
        let major = parse_num(nums.next())?;
        let minor = parse_num(nums.next())?;
        let patch = parse_num(nums.next())?;
        if nums.next().is_some() {
            anyhow::bail!("malformed semantic version: {input}");
        }
        let prerelease = if pre.is_empty() {
            Vec::new()
        } else {
            pre.split('.')
                .map(|part| {
                    if part.is_empty() || !part.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
                        anyhow::bail!("malformed semantic version: {input}");
                    }
                    if part.bytes().all(|b| b.is_ascii_digit()) {
                        if part.len() > 1 && part.starts_with('0') {
                            anyhow::bail!("malformed semantic version: {input}");
                        }
                        Ok(Identifier::Numeric(part.parse()?))
                    } else {
                        Ok(Identifier::Text(part.to_string()))
                    }
                })
                .collect::<anyhow::Result<Vec<_>>>()?
        };
        Ok(Self {
            major,
            minor,
            patch,
            prerelease,
        })
    }

    fn cmp(&self, other: &Self) -> Ordering {
        (self.major, self.minor, self.patch)
            .cmp(&(other.major, other.minor, other.patch))
            .then_with(|| match (self.prerelease.is_empty(), other.prerelease.is_empty()) {
                (true, true) => Ordering::Equal,
                (true, false) => Ordering::Greater,
                (false, true) => Ordering::Less,
                (false, false) => {
                    for (left, right) in self.prerelease.iter().zip(&other.prerelease) {
                        let ordering = match (left, right) {
                            (Identifier::Numeric(a), Identifier::Numeric(b)) => a.cmp(b),
                            (Identifier::Numeric(_), Identifier::Text(_)) => Ordering::Less,
                            (Identifier::Text(_), Identifier::Numeric(_)) => Ordering::Greater,
                            (Identifier::Text(a), Identifier::Text(b)) => a.cmp(b),
                        };
                        if ordering != Ordering::Equal {
                            return ordering;
                        }
                    }
                    self.prerelease.len().cmp(&other.prerelease.len())
                }
            })
    }
}

/// Decision for an observed core version against the pinned download target.
///
/// The pinned version is the *download target* for fresh installs and
/// updates, not an equality gate: an already installed binary that is newer
/// than the pin stays usable so a GUI-side update cannot brick the CLI.
/// Anything outside the pinned major line, or any prerelease, stays
/// fail-closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VersionPolicy {
    /// Observed equals the pinned target.
    Exact,
    /// Same major, no prerelease, and newer than the pin — accepted.
    NewerWithinMajor,
    /// Same major but older than the pin — guided update/download.
    OlderThanPinned,
    /// Different major than the pin — rejected, never auto-replaced.
    UnsupportedMajor,
    /// Any prerelease identifier — rejected.
    Prerelease,
}

impl VersionPolicy {
    pub fn is_usable(self) -> bool {
        matches!(self, Self::Exact | Self::NewerWithinMajor)
    }

    pub fn reason(self) -> &'static str {
        match self {
            Self::Exact => "matches the pinned download target",
            Self::NewerWithinMajor => "newer than the pinned target within the same major version",
            Self::OlderThanPinned => "older than the pinned target",
            Self::UnsupportedMajor => "different major version than the pinned target",
            Self::Prerelease => "prerelease version is not accepted",
        }
    }
}

/// Classify `observed` against the pinned `required` version.
///
/// Applies uniformly to both cores: the pinned version comes from
/// [`MIHOMO_POLICY_VERSION`] or [`SINGBOX_POLICY_VERSION`].
pub fn classify(observed: &str, required: &str) -> anyhow::Result<VersionPolicy> {
    let got = Version::parse(observed)?;
    let pin = Version::parse(required)?;
    if !got.prerelease.is_empty() {
        return Ok(VersionPolicy::Prerelease);
    }
    if got.major != pin.major {
        return Ok(VersionPolicy::UnsupportedMajor);
    }
    Ok(match got.cmp(&pin) {
        Ordering::Equal => VersionPolicy::Exact,
        Ordering::Greater => VersionPolicy::NewerWithinMajor,
        Ordering::Less => VersionPolicy::OlderThanPinned,
    })
}

/// Accept the pinned version and any newer release in the same major line.
pub fn is_compatible(_core: &str, observed: &str, required: &str) -> anyhow::Result<bool> {
    Ok(classify(observed, required)?.is_usable())
}

pub fn is_newer_than(observed: &str, target: &str) -> anyhow::Result<bool> {
    Ok(Version::parse(observed)?.cmp(&Version::parse(target)?) == Ordering::Greater)
}

pub fn incompatibility(core: &str, observed: &str, required: &str) -> String {
    let reason = classify(observed, required)
        .map(|policy| policy.reason())
        .unwrap_or("version is malformed");
    format!(
        "{core} version {observed} is outside the supported policy ({reason}); pinned download target is {required}"
    )
}

/// Log the acceptance decision for a newer-than-pinned binary so the
/// substitution stays visible in logs.
pub fn log_acceptance(core: &str, observed: &str, policy: VersionPolicy) {
    if policy == VersionPolicy::NewerWithinMajor {
        tracing::info!(
            target: "core_policy",
            "accepting {core} {observed}: {} (pinned download target unchanged)",
            policy.reason()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pinned_policy_accepts_newer_within_the_same_major_for_both_cores() {
        // Pinned target itself.
        assert!(is_compatible("mihomo", "v1.19.32", MIHOMO_POLICY_VERSION).unwrap());
        assert!(is_compatible("sing-box", "1.14.2", SINGBOX_POLICY_VERSION).unwrap());
        // Same major, newer than the pin → accepted (GUI-updated binary).
        assert!(is_compatible("mihomo", "1.19.33", MIHOMO_POLICY_VERSION).unwrap());
        assert!(is_compatible("mihomo", "1.19.99", MIHOMO_POLICY_VERSION).unwrap());
        assert!(is_compatible("mihomo", "1.20.0", MIHOMO_POLICY_VERSION).unwrap());
        assert!(is_compatible("sing-box", "1.15.0", SINGBOX_POLICY_VERSION).unwrap());
        assert!(is_compatible("sing-box", "v1.14.3", SINGBOX_POLICY_VERSION).unwrap());
    }

    #[test]
    fn pinned_policy_rejects_older_than_pinned_with_guidance() {
        assert!(!is_compatible("mihomo", "1.19.31", MIHOMO_POLICY_VERSION).unwrap());
        assert!(!is_compatible("sing-box", "1.14.1", SINGBOX_POLICY_VERSION).unwrap());
        assert_eq!(
            classify("1.19.31", MIHOMO_POLICY_VERSION).unwrap(),
            VersionPolicy::OlderThanPinned
        );
        assert!(incompatibility("mihomo", "1.19.31", MIHOMO_POLICY_VERSION).contains("older than the pinned target"));
    }

    #[test]
    fn pinned_policy_rejects_different_major() {
        assert!(!is_compatible("mihomo", "2.19.32", MIHOMO_POLICY_VERSION).unwrap());
        assert!(!is_compatible("sing-box", "0.14.2", SINGBOX_POLICY_VERSION).unwrap());
        assert_eq!(
            classify("2.0.0", MIHOMO_POLICY_VERSION).unwrap(),
            VersionPolicy::UnsupportedMajor
        );
        assert!(incompatibility("mihomo", "2.19.32", MIHOMO_POLICY_VERSION).contains("different major"));
    }

    #[test]
    fn pinned_policy_rejects_any_prerelease() {
        assert!(!is_compatible("sing-box", "1.14.2-rc.1", SINGBOX_POLICY_VERSION).unwrap());
        assert!(!is_compatible("sing-box", "1.14.3-alpha.1", SINGBOX_POLICY_VERSION).unwrap());
        assert!(!is_compatible("mihomo", "1.19.33-beta.1", MIHOMO_POLICY_VERSION).unwrap());
        assert_eq!(
            classify("1.19.33-beta.1", MIHOMO_POLICY_VERSION).unwrap(),
            VersionPolicy::Prerelease
        );
    }

    #[test]
    fn malformed_versions_never_classify() {
        assert!(classify("not-a-version", MIHOMO_POLICY_VERSION).is_err());
        assert!(is_compatible("mihomo", "", MIHOMO_POLICY_VERSION).is_err());
        assert!(incompatibility("mihomo", "not-a-version", MIHOMO_POLICY_VERSION).contains("malformed"));
    }

    #[test]
    fn semantic_versions_order_prereleases_and_reject_ambiguity() {
        assert!(
            Version::parse("1.14.2-alpha.2")
                .unwrap()
                .cmp(&Version::parse("1.14.2-alpha.10").unwrap())
                .is_lt()
        );
        assert!(
            Version::parse("1.14.2-rc.1")
                .unwrap()
                .cmp(&Version::parse("1.14.2").unwrap())
                .is_lt()
        );
        for bad in [
            "",
            "1.14",
            "1.14.x",
            "1.014.2",
            "1.14.2garbage",
            "1.14.2-",
            "1.14.2+",
            "1.14.2+a+b",
        ] {
            assert!(Version::parse(bad).is_err(), "accepted {bad}");
        }
    }

    #[test]
    fn errors_identify_core_observed_and_required_versions() {
        let message = incompatibility("sing-box", "1.13.21", SINGBOX_POLICY_VERSION);
        assert!(message.contains("sing-box") && message.contains("1.13.21") && message.contains("1.14.2"));
        assert!(is_newer_than("1.19.33", MIHOMO_POLICY_VERSION).unwrap());
    }
}
