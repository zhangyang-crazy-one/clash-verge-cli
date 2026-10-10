//! sing-box runtime configuration generation.
//!
//! The generator produces the `singbox.json` runtime file consumed by
//! `sing-box run -c`. Node outbounds arrive pre-converted (see the
//! subscription converter), while inbound/route/clash_api plumbing is
//! generated here from TUI settings.

// Consumed from task 2.3 of add-singbox-dual-core onward (manager writes
// singbox.json; converter feeds outbounds).
pub mod capabilities;
#[allow(dead_code)]
pub mod config_gen;
pub mod convert;
pub mod dns;

use serde_json::Value;

pub use config_gen::{ClashApiSettings, ConfigInput, GroupKind, GroupSpec, TunSettings, generate_config};
pub use dns::DnsConfigSpec;

/// Task 7.4: durable rule-set storage (TUI-owned, separate from the
/// generated singbox.json which is regenerated on every apply).
pub const RULE_SETS_FILE: &str = "singbox-rule-sets.json";

/// Task 7.5: durable storage for logical route rules. Logical rules have no
/// clash YAML form (`routing::save_profile_rules` rejects them), so they
/// live here as sing-box JSON objects next to the persisted cross-type
/// [`RuleOrder`].
///
/// Two shapes are accepted on read:
/// * the current one, `{"version":1,"entries":[{"profile":0},{"logical":0}]}`,
///   which records the full interleaved order of profile and logical rules;
/// * the legacy one, a bare JSON array of logical rules, which carries no
///   order and keeps the old "append after the profile rules" behaviour.
pub const LOGICAL_RULES_FILE: &str = "singbox-rules.json";

/// Version tag written into [`LOGICAL_RULES_FILE`].
const RULE_ORDER_VERSION: u64 = 1;

/// One slot of the persisted interleaved rule order: either the clash rule
/// at `index` of the COMPOSED profile rule list (the profile's `rules:` after
/// the Rules/Merge/Script chain), or the logical rule at `index` of
/// [`RuleOrder::logical`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RuleOrderEntry {
    Profile(usize),
    Logical(usize),
}

/// Identity of the rule list an order was saved against.
///
/// The order's `Profile(i)` entries are indices into the COMPOSED profile
/// rule list — the one the generator interleaves into the sing-box config —
/// so they only mean anything while that list is the one the user actually
/// arranged. A subscription refresh replaces the list under the sidecar: the
/// indices stay perfectly in range and every one of them now points at a
/// different rule, which silently reorders the generated config. Storing the
/// rule count plus a hash of the rule descriptors turns that into a
/// detectable identity change instead of a silent one.
///
/// Both the editor's save and generation must fingerprint THIS list (see
/// [`crate::runtime_config::composed_profile_rules`]); fingerprinting the raw
/// profile file instead makes every save look like drift as soon as the
/// profile has a prepend fragment.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ProfileFingerprint {
    pub count: usize,
    pub hash: u64,
}

/// Fingerprint of the composed profile rules a saved order refers to.
///
/// FNV-1a over [`crate::routing::describe`] rather than
/// `DefaultHasher`, whose keys are an implementation detail: this value is
/// persisted and must be identical across processes and releases.
pub fn profile_rule_fingerprint(rules: &[crate::routing::IRouteRule]) -> ProfileFingerprint {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let mut absorb = |bytes: &[u8]| {
        for byte in bytes {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    };
    for rule in rules {
        absorb(crate::routing::describe(rule).as_bytes());
        absorb(b"\x1f");
    }
    ProfileFingerprint {
        count: rules.len(),
        hash,
    }
}

/// The durable ordering of profile rules and logical (AND/OR) rules.
///
/// The editor buffer is one interleaved list, but the two halves are stored
/// in different files (profile YAML and sidecar). Persisting only the two
/// partitions lost the order between them: a logical rule the user had
/// dragged above a `MATCH` was re-emitted after the whole profile rule list,
/// i.e. after the catch-all, and stopped matching.
///
/// `entries.is_empty()` is the legacy shape (no order information): profile
/// rules first, logical rules appended after them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RuleOrder {
    pub logical: Vec<crate::routing::IRouteRule>,
    pub entries: Vec<RuleOrderEntry>,
    /// Identity of the COMPOSED profile rule list `entries` was recorded
    /// against. `None` on a sidecar written before drift detection existed.
    pub profile: Option<ProfileFingerprint>,
}

impl RuleOrder {
    /// Whether this order still describes `actual`.
    ///
    /// An order without a recorded fingerprint (legacy sidecar) is trusted:
    /// it was written by a build that had no way to detect the drift, and
    /// refusing it would discard user ordering that is still valid.
    pub fn matches_profile(&self, actual: ProfileFingerprint) -> bool {
        self.profile.is_none_or(|stored| stored == actual)
    }

    /// The order to use for `actual`, plus a note when the stored one was
    /// dropped.
    ///
    /// A subscription refresh that replaced the profile rule list makes every
    /// stored `Profile(i)` point somewhere else. Rather than trust indices
    /// that merely happen to be in range, fall back to append-after — the
    /// same layout a sidecar without order information uses — and report the
    /// mismatch so it is visible instead of silent. The logical rules
    /// themselves are kept: only the cross-type placement is dropped.
    pub fn resolve_profile_drift(&self, actual: ProfileFingerprint) -> (RuleOrder, Option<String>) {
        if self.entries.is_empty() || self.matches_profile(actual) {
            return (self.clone(), None);
        }
        let stored = self.profile.expect("a mismatch implies a recorded fingerprint");
        (
            RuleOrder {
                logical: self.logical.clone(),
                entries: Vec::new(),
                profile: Some(actual),
            },
            Some(format!(
                "rule order reset: the profile rule list changed since it was saved \
(stored {} rules, now {}); logical rules are appended after the profile rules instead \
of interleaved",
                stored.count, actual.count
            )),
        )
    }
}

/// Task 8.1: structured DNS settings (1.12+ new format), TUI-owned.
pub const DNS_CONFIG_FILE: &str = "singbox-dns.json";

fn read_json_file<T: serde::de::DeserializeOwned + Default>(home: &std::path::Path, name: &str) -> Result<T, String> {
    let path = home.join(name);
    let body = match std::fs::read_to_string(&path) {
        Ok(body) => body,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(T::default()),
        Err(error) => return Err(format!("read {}: {error}", path.display())),
    };
    serde_json::from_str(&body).map_err(|error| format!("parse {}: {error}", path.display()))
}

pub fn load_rule_sets(home: &std::path::Path) -> Result<Vec<Value>, String> {
    let sets: Vec<Value> = read_json_file(home, RULE_SETS_FILE)?;
    validate_rule_sets(&sets).map_err(|error| format!("{}: {error}", home.join(RULE_SETS_FILE).display()))?;
    Ok(sets)
}

pub(crate) fn validate_rule_sets(sets: &[Value]) -> Result<(), String> {
    let mut tags = std::collections::HashSet::new();
    for (index, item) in sets.iter().enumerate() {
        let tag = item
            .get("tag")
            .and_then(Value::as_str)
            .filter(|tag| !tag.trim().is_empty());
        if !item.is_object() || tag.is_none() {
            return Err(format!("rule-set {index} must be an object with a non-empty tag"));
        }
        let Some(tag) = tag else { continue };
        if !tags.insert(tag) {
            return Err(format!("duplicate rule-set tag {tag:?}"));
        }
        if !matches!(item.get("type").and_then(Value::as_str), Some("local" | "remote")) {
            return Err(format!("rule-set {tag:?} has unsupported type"));
        }
        match item.get("type").and_then(Value::as_str) {
            Some("remote") if item.get("url").and_then(Value::as_str).is_none_or(str::is_empty) => {
                return Err(format!("remote rule-set {tag:?} requires a URL"));
            }
            Some("local") if item.get("path").and_then(Value::as_str).is_none_or(str::is_empty) => {
                return Err(format!("local rule-set {tag:?} requires a path"));
            }
            _ => {}
        }
    }
    Ok(())
}

pub fn save_rule_sets(home: &std::path::Path, sets: &[Value]) -> std::io::Result<()> {
    validate_rule_sets(sets).map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))?;
    if home.join(LOGICAL_RULES_FILE).is_file() {
        let rules =
            load_logical_rules(home).map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        validate_rule_set_references_against(&rules, sets)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))?;
    }
    let body = serde_json::to_string_pretty(sets)?;
    atomic_write(home, RULE_SETS_FILE, body.as_bytes())
}

/// Load stored logical rules. Parse and reference failures are returned to
/// the caller rather than converted into an empty/default settings value.
/// Works for both [`LOGICAL_RULES_FILE`] shapes.
pub fn load_logical_rules(home: &std::path::Path) -> Result<Vec<crate::routing::IRouteRule>, String> {
    Ok(load_rule_order(home)?.logical)
}

/// Load the persisted interleaved order plus the logical rules it indexes.
pub fn load_rule_order(home: &std::path::Path) -> Result<RuleOrder, String> {
    let path = home.join(LOGICAL_RULES_FILE);
    let body = match std::fs::read_to_string(&path) {
        Ok(body) => body,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(RuleOrder::default()),
        Err(error) => return Err(format!("read {}: {error}", path.display())),
    };
    let value: Value = serde_json::from_str(&body).map_err(|error| format!("parse {}: {error}", path.display()))?;
    let order = parse_rule_order(&value).map_err(|error| format!("{}: {error}", path.display()))?;
    validate_rule_set_references(home, &order.logical).map_err(|error| format!("{}: {error}", path.display()))?;
    Ok(order)
}

/// Parse either supported sidecar shape into the typed order model.
fn parse_rule_order(value: &Value) -> Result<RuleOrder, String> {
    if value.is_array() {
        // Legacy: a bare list of logical rules with no cross-type order.
        return Ok(RuleOrder {
            logical: parse_logical_values(value.as_array().expect("array"))?,
            entries: Vec::new(),
            profile: None,
        });
    }
    if !value.is_object() {
        return Err("rule order must be an object or a legacy array".into());
    }
    let version = value
        .get("version")
        .and_then(Value::as_u64)
        .unwrap_or(RULE_ORDER_VERSION);
    if version != RULE_ORDER_VERSION {
        return Err(format!("unsupported rule order version {version}"));
    }
    let entries = value
        .get("entries")
        .and_then(Value::as_array)
        .ok_or("rule order requires an `entries` array")?;
    let mut parsed = Vec::with_capacity(entries.len());
    for (index, entry) in entries.iter().enumerate() {
        let profile = entry.get("profile").and_then(Value::as_u64);
        let logical = entry.get("logical").and_then(Value::as_u64);
        parsed.push(match (profile, logical) {
            (Some(index), None) => RuleOrderEntry::Profile(
                usize::try_from(index)
                    .map_err(|_| format!("rule order entry {index} has an out-of-range profile index"))?,
            ),
            (None, Some(index)) => RuleOrderEntry::Logical(
                usize::try_from(index)
                    .map_err(|_| format!("rule order entry {index} has an out-of-range logical index"))?,
            ),
            _ => {
                return Err(format!(
                    "rule order entry {index} must name exactly one of profile/logical"
                ));
            }
        });
    }
    let rules = value
        .get("rules")
        .and_then(Value::as_array)
        .ok_or("rule order requires a `rules` array of logical rules")?;
    let profile = match value.get("profile") {
        None | Some(Value::Null) => None,
        Some(fingerprint) => Some(
            serde_json::from_value::<ProfileFingerprint>(fingerprint.clone())
                .map_err(|error| format!("rule order `profile` fingerprint: {error}"))?,
        ),
    };
    Ok(RuleOrder {
        logical: parse_logical_values(rules)?,
        entries: parsed,
        profile,
    })
}

fn parse_logical_values(values: &[Value]) -> Result<Vec<crate::routing::IRouteRule>, String> {
    let rules: Vec<_> = values
        .iter()
        .enumerate()
        .map(|(index, value)| {
            let rule = crate::routing::from_singbox_json(value)
                .filter(|rule| matches!(rule, crate::routing::IRouteRule::Logical { .. }))
                .ok_or_else(|| format!("{} entry {index} is not a supported logical rule", LOGICAL_RULES_FILE))?;
            // `from_singbox_json` intentionally extracts only the rule model's
            // supported fields. Require exact semantic round-trip here so an
            // edited/corrupt durable JSON entry with extra fields or values
            // cannot be partially accepted and then truncated by to_singbox_json.
            let round_trip = crate::routing::to_singbox_json(&rule)
                .ok_or_else(|| format!("{} entry {index} is not sing-box expressible", LOGICAL_RULES_FILE))?;
            if round_trip != *value {
                return Err(format!(
                    "{} entry {index} contains unsupported or lossy rule fields",
                    LOGICAL_RULES_FILE
                ));
            }
            Ok(rule)
        })
        .collect::<Result<_, _>>()?;
    Ok(rules)
}

/// Persist logical rules as sing-box JSON objects (lossless round-trip;
/// see the 6.3 property tests in `routing`).
///
/// This is the legacy "no cross-type order" shape: the rules are appended
/// after the profile rules at generation time. Production saves go through
/// [`save_rule_order`]; this writer stays for the legacy-format fixtures.
#[cfg(test)]
pub fn save_logical_rules(home: &std::path::Path, rules: &[crate::routing::IRouteRule]) -> Result<(), String> {
    let values = logical_rule_values(rules)?;
    validate_rule_set_references(home, rules)?;
    let body = serde_json::to_string_pretty(&values).map_err(|e| e.to_string())?;
    atomic_write(home, LOGICAL_RULES_FILE, body.as_bytes()).map_err(|e| e.to_string())
}

/// Persist the logical rules together with the interleaved order that says
/// where each of them sits relative to the profile rules.
pub fn save_rule_order(home: &std::path::Path, order: &RuleOrder) -> Result<(), String> {
    let rules = logical_rule_values(&order.logical)?;
    for (slot, entry) in order.entries.iter().enumerate() {
        if let RuleOrderEntry::Logical(index) = entry
            && *index >= order.logical.len()
        {
            return Err(format!(
                "rule order entry {slot} references logical rule {index}, but only {} are stored",
                order.logical.len()
            ));
        }
    }
    validate_rule_set_references(home, &order.logical)?;
    let value = serde_json::json!({
        "version": RULE_ORDER_VERSION,
        "rules": rules,
        "profile": order.profile,
        "entries": order
            .entries
            .iter()
            .map(|entry| match entry {
                RuleOrderEntry::Profile(index) => serde_json::json!({ "profile": index }),
                RuleOrderEntry::Logical(index) => serde_json::json!({ "logical": index }),
            })
            .collect::<Vec<Value>>(),
    });
    let body = serde_json::to_string_pretty(&value).map_err(|e| e.to_string())?;
    atomic_write(home, LOGICAL_RULES_FILE, body.as_bytes()).map_err(|e| e.to_string())
}

/// Merge profile-derived route rules with the stored logical rules in the
/// order the editor persisted.
///
/// The convenience form for callers that hold the CONVERTED profile rules
/// only, i.e. without their original-slot identity. Generation uses
/// [`interleave_route_slots`] so a rule the conversion dropped cannot shift
/// every index after it; this shim exists for the tests that exercise the
/// interleaving itself.
#[cfg(test)]
pub fn interleave_route_rules(profile_rules: &[Value], order: &RuleOrder) -> Vec<Value> {
    interleave_route_slots(&profile_rules.iter().cloned().map(Some).collect::<Vec<_>>(), order)
}

/// Merge the converted profile rules — carried in their ORIGINAL profile-rule
/// slots, `None` where the conversion dropped the rule — with the stored
/// logical rules, in the order the editor persisted.
///
/// The sidecar's `Profile(i)` entries index the ORIGINAL YAML rule list, so
/// interleaving has to walk the original slots, not the compressed output.
/// Indexing the compressed list instead made every entry after a dropped
/// rule point one slot too early: with `P0=DOMAIN-REGEX,ads.*,REJECT`
/// (dropped), `L0=logical block`, `P1=MATCH,DIRECT` and a saved order of
/// `P0,L0,P1`, `P0` resolved to the converted `MATCH` and the block rule was
/// emitted *after* the catch-all, where the core never evaluates it.
///
/// A dropped slot keeps its identity (it is simply skipped), so one
/// unrepresentable rule cannot reorder everything below it.
pub fn interleave_route_slots(profile_slots: &[Option<Value>], order: &RuleOrder) -> Vec<Value> {
    let logical_json: Vec<Option<Value>> = order.logical.iter().map(crate::routing::to_singbox_json).collect();
    if order.entries.is_empty() {
        // Legacy sidecar (or a drifted one, which was deliberately demoted to
        // this layout): no cross-type order was ever recorded.
        let mut merged: Vec<Value> = profile_slots.iter().flatten().cloned().collect();
        merged.extend(logical_json.into_iter().flatten());
        return merged;
    }
    let mut merged: Vec<Value> = Vec::with_capacity(profile_slots.len() + order.logical.len());
    let mut referenced = vec![false; profile_slots.len()];
    let mut last_profile_slot = None;
    for entry in &order.entries {
        match entry {
            RuleOrderEntry::Profile(index) => match profile_slots.get(*index) {
                Some(Some(rule)) => {
                    referenced[*index] = true;
                    merged.push(rule.clone());
                    last_profile_slot = Some(merged.len() - 1);
                }
                // Referenced but dropped by the conversion: its slot is
                // consumed, nothing is emitted. Marking it keeps the
                // unplaced fallback from trying to re-insert it later.
                Some(None) => referenced[*index] = true,
                // Past the end of the profile rule list (edited outside the
                // editor): skipped, like before.
                None => continue,
            },
            RuleOrderEntry::Logical(index) => match logical_json.get(*index).and_then(Option::as_ref) {
                Some(rule) => merged.push(rule.clone()),
                None => continue,
            },
        }
    }
    let unplaced: Vec<Value> = profile_slots
        .iter()
        .enumerate()
        .filter(|(index, _)| !referenced[*index])
        .filter_map(|(_, slot)| slot.clone())
        .collect();
    if !unplaced.is_empty() {
        let at = last_profile_slot.map_or(0, |slot| slot + 1);
        merged.splice(at..at, unplaced);
    }
    merged
}

fn logical_rule_values(rules: &[crate::routing::IRouteRule]) -> Result<Vec<Value>, String> {
    if rules
        .iter()
        .any(|r| !matches!(r, crate::routing::IRouteRule::Logical { .. }))
    {
        return Err("logical rule storage accepts only IRouteRule::Logical entries".into());
    }
    rules
        .iter()
        .map(crate::routing::to_singbox_json)
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| "logical rule storage requires sing-box-expressible rules".to_string())
}

fn validate_rule_set_references(home: &std::path::Path, rules: &[crate::routing::IRouteRule]) -> Result<(), String> {
    validate_rule_set_references_against(rules, &load_rule_sets(home)?)
}

fn validate_rule_set_references_against(rules: &[crate::routing::IRouteRule], sets: &[Value]) -> Result<(), String> {
    use crate::routing::{IRouteRule, MatchField};
    let known: std::collections::HashSet<String> = sets
        .iter()
        .filter_map(|set| set.get("tag").and_then(Value::as_str).map(str::to_owned))
        .collect();
    fn visit(rule: &IRouteRule, known: &std::collections::HashSet<String>) -> Result<(), String> {
        match rule {
            IRouteRule::Simple { matches, .. } => {
                for field in matches {
                    if let MatchField::RuleSet(tag) = field
                        && !known.contains(tag)
                    {
                        return Err(format!("logical rule references unknown rule-set {tag:?}"));
                    }
                }
            }
            IRouteRule::Logical { rules, .. } => {
                for child in rules {
                    visit(child, known)?;
                }
            }
            IRouteRule::Raw { .. } => return Err("logical rule storage does not accept raw rules".into()),
        }
        Ok(())
    }
    for rule in rules {
        visit(rule, &known)?;
    }
    Ok(())
}

pub fn load_dns_spec(home: &std::path::Path) -> Result<DnsConfigSpec, String> {
    let spec: DnsConfigSpec = read_json_file(home, DNS_CONFIG_FILE)?;
    validate_dns_references(&spec).map_err(|error| format!("{}: {error}", home.join(DNS_CONFIG_FILE).display()))?;
    Ok(spec)
}

pub fn save_dns_spec(home: &std::path::Path, spec: &DnsConfigSpec) -> Result<(), String> {
    validate_dns_references(spec)?;
    let body = serde_json::to_string_pretty(spec).map_err(|e| e.to_string())?;
    atomic_write(home, DNS_CONFIG_FILE, body.as_bytes()).map_err(|e| e.to_string())
}

fn validate_dns_references(spec: &DnsConfigSpec) -> Result<(), String> {
    spec.validate()?;
    if let Some(resolver) = spec.domain_resolver.as_deref()
        && !spec.servers.iter().any(|server| server.tag == resolver)
    {
        return Err(format!("domain_resolver references unknown DNS server {resolver:?}"));
    }
    Ok(())
}

fn atomic_write(home: &std::path::Path, name: &str, body: &[u8]) -> std::io::Result<()> {
    atomic_write_using(home, name, body, |from, to| std::fs::rename(from, to))
}

fn atomic_write_using(
    home: &std::path::Path,
    name: &str,
    body: &[u8],
    rename: impl FnOnce(&std::path::Path, &std::path::Path) -> std::io::Result<()>,
) -> std::io::Result<()> {
    use std::io::Write as _;
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);
    std::fs::create_dir_all(home)?;
    let target = home.join(name);
    let sequence = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
    let temp = home.join(format!(".{name}.{}.{}.tmp", std::process::id(), sequence));
    let result = (|| {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut file = options.open(&temp)?;
        file.write_all(body)?;
        file.sync_all()?;
        rename(&temp, &target)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod storage_tests {
    use super::*;
    use crate::routing::{IRouteRule, LogicOp, MatchField, RuleTarget};
    use crate::singbox::dns::{DnsServerKind, DnsServerSpec};

    fn temp_home(name: &str) -> std::path::PathBuf {
        // Unique per test: parallel tests share the process, and one test's
        // cleanup must not delete another's directory mid-write.
        let dir = std::env::temp_dir().join(format!(
            "singbox-mod-test-{}-{name}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).expect("mkdir");
        dir
    }

    #[test]
    fn logical_rules_round_trip_through_sidecar() {
        let home = temp_home("logical");
        let rules = vec![IRouteRule::Logical {
            op: LogicOp::Or,
            rules: vec![IRouteRule::Simple {
                matches: vec![MatchField::Domain("a.com".into())],
                target: RuleTarget::Direct,
            }],
            target: RuleTarget::Block,
        }];

        save_logical_rules(&home, &rules).expect("save");
        assert_eq!(load_logical_rules(&home).unwrap(), rules);

        // Simple/Raw rules are not sidecar material — saving them is a
        // programming error the storage refuses rather than silently dropping.
        let mixed = vec![
            IRouteRule::Logical {
                op: LogicOp::And,
                rules: vec![],
                target: RuleTarget::Direct,
            },
            IRouteRule::Simple {
                matches: vec![MatchField::Port(443)],
                target: RuleTarget::Direct,
            },
        ];
        assert!(save_logical_rules(&home, &mixed).is_err());

        let with_missing_set = vec![IRouteRule::Logical {
            op: LogicOp::And,
            rules: vec![IRouteRule::Simple {
                matches: vec![MatchField::RuleSet("missing-set".into())],
                target: RuleTarget::Direct,
            }],
            target: RuleTarget::Direct,
        }];
        assert!(
            save_logical_rules(&home, &with_missing_set)
                .unwrap_err()
                .contains("unknown rule-set")
        );
        save_rule_sets(
            &home,
            &[serde_json::json!({"tag":"known-set","type":"local","path":"rules.srs"})],
        )
        .unwrap();
        let with_known_set = vec![IRouteRule::Logical {
            op: LogicOp::And,
            rules: vec![IRouteRule::Simple {
                matches: vec![MatchField::RuleSet("known-set".into())],
                target: RuleTarget::Direct,
            }],
            target: RuleTarget::Direct,
        }];
        save_logical_rules(&home, &with_known_set).expect("known rule-set reference");
        assert_eq!(load_logical_rules(&home).unwrap(), with_known_set);
        assert!(
            save_rule_sets(&home, &[])
                .unwrap_err()
                .to_string()
                .contains("unknown rule-set")
        );
        assert_eq!(load_rule_sets(&home).unwrap()[0]["tag"], "known-set");

        // Corrupt entries are dropped on load, not propagated.
        std::fs::write(home.join(LOGICAL_RULES_FILE), "[{\"bogus\":1}]").expect("write");
        assert!(load_logical_rules(&home).is_err());

        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn stored_logical_rule_rejects_unmodeled_fields_instead_of_truncating() {
        let home = temp_home("lossy-logical");
        std::fs::write(
            home.join(LOGICAL_RULES_FILE),
            r#"[{"type":"logical","mode":"and","rules":[{"domain":["example.com"],"domain_regex":[".*"],"outbound":"direct"}],"outbound":"direct"}]"#,
        )
        .expect("write fixture");

        let error = load_logical_rules(&home).unwrap_err();
        assert!(error.contains("unsupported or lossy rule fields"), "{error}");
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn the_interleaved_order_round_trips_and_keeps_the_legacy_shape_working() {
        let home = temp_home("rule-order");
        let logical = |domain: &str| IRouteRule::Logical {
            op: LogicOp::Or,
            rules: vec![IRouteRule::Simple {
                matches: vec![MatchField::Domain(domain.into())],
                target: RuleTarget::Direct,
            }],
            target: RuleTarget::Block,
        };
        let order = RuleOrder {
            logical: vec![logical("blocked.example")],
            // logical rule first, then profile rule 0, then profile rule 1
            entries: vec![
                RuleOrderEntry::Logical(0),
                RuleOrderEntry::Profile(0),
                RuleOrderEntry::Profile(1),
            ],
            profile: None,
        };
        save_rule_order(&home, &order).expect("save");
        assert_eq!(load_rule_order(&home).unwrap(), order);
        // `load_logical_rules` still answers for callers that do not care
        // about the order.
        assert_eq!(load_logical_rules(&home).unwrap(), vec![logical("blocked.example")]);

        // The persisted body carries the order explicitly.
        let body: Value =
            serde_json::from_str(&std::fs::read_to_string(home.join(LOGICAL_RULES_FILE)).unwrap()).unwrap();
        assert_eq!(body["version"], Value::from(1));
        assert_eq!(body["entries"][0]["logical"], Value::from(0));
        assert_eq!(body["entries"][1]["profile"], Value::from(0));

        // A legacy bare array still loads, with no cross-type order recorded,
        // and generation keeps the historical append-after-profile layout.
        std::fs::write(
            home.join(LOGICAL_RULES_FILE),
            serde_json::to_string(&vec![crate::routing::to_singbox_json(&logical("c.com")).unwrap()]).unwrap(),
        )
        .expect("write legacy fixture");
        let legacy = load_rule_order(&home).unwrap();
        assert!(legacy.entries.is_empty(), "{legacy:?}");
        assert_eq!(legacy.logical, vec![logical("c.com")]);
        let profile = vec![serde_json::json!({"outbound": "direct"})];
        let merged = interleave_route_rules(&profile, &legacy);
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0], profile[0]);
        assert_eq!(merged[1]["outbound"], "block");

        // A stale entry (index past the end of the profile rules) is skipped
        // instead of failing the whole generation.
        let stale = RuleOrder {
            logical: vec![logical("c.com")],
            entries: vec![RuleOrderEntry::Profile(9), RuleOrderEntry::Logical(0)],
            profile: None,
        };
        let merged = interleave_route_rules(&profile, &stale);
        assert_eq!(merged[0], profile[0]);
        assert_eq!(merged[1]["outbound"], "block");
        assert_eq!(merged.len(), 2);

        let _ = std::fs::remove_dir_all(&home);
    }

    /// The fingerprint is part of the sidecar, so a stored order can be
    /// matched against the profile rule list it was recorded against.
    #[test]
    fn the_saved_order_carries_the_profile_rule_list_it_was_recorded_against() {
        let home = temp_home("rule-order-fingerprint");
        let logical = |domain: &str| IRouteRule::Logical {
            op: LogicOp::Or,
            rules: vec![IRouteRule::Simple {
                matches: vec![MatchField::Domain(domain.into())],
                target: RuleTarget::Direct,
            }],
            target: RuleTarget::Block,
        };
        let profile: Vec<IRouteRule> = vec![
            IRouteRule::Simple {
                matches: vec![MatchField::Domain("a.example".into())],
                target: RuleTarget::Direct,
            },
            IRouteRule::Raw {
                clash_raw: "MATCH,DIRECT".into(),
            },
        ];
        let fingerprint = profile_rule_fingerprint(&profile);
        let order = RuleOrder {
            logical: vec![logical("blocked.example")],
            entries: vec![RuleOrderEntry::Logical(0), RuleOrderEntry::Profile(0)],
            profile: Some(fingerprint),
        };
        save_rule_order(&home, &order).expect("save");
        let body: Value =
            serde_json::from_str(&std::fs::read_to_string(home.join(LOGICAL_RULES_FILE)).unwrap()).unwrap();
        assert_eq!(body["profile"]["count"], Value::from(2));
        assert_eq!(
            load_rule_order(&home).unwrap(),
            order,
            "the fingerprint must survive the round-trip"
        );

        // A subscription refresh that replaced the list is an identity
        // change even though every stored index stays in range.
        let refreshed: Vec<IRouteRule> = vec![
            IRouteRule::Simple {
                matches: vec![MatchField::Domain("new.example".into())],
                target: RuleTarget::Direct,
            },
            IRouteRule::Raw {
                clash_raw: "MATCH,DIRECT".into(),
            },
            IRouteRule::Raw {
                clash_raw: "MATCH,DIRECT".into(),
            },
        ];
        let (effective, note) = order.resolve_profile_drift(profile_rule_fingerprint(&refreshed));
        assert!(effective.entries.is_empty(), "stale indices must be dropped");
        assert_eq!(effective.logical.len(), 1, "the logical rule is kept");
        assert!(note.expect("reported").contains("rule order reset"));
        // An unchanged list is trusted.
        assert_eq!(order.resolve_profile_drift(fingerprint).1, None);
        // A legacy sidecar without a fingerprint is trusted too.
        let legacy = RuleOrder {
            logical: order.logical.clone(),
            entries: order.entries.clone(),
            profile: None,
        };
        assert!(legacy.matches_profile(profile_rule_fingerprint(&refreshed)));

        let _ = std::fs::remove_dir_all(&home);
    }

    /// Interleaving against ORIGINAL slots: a dropped rule keeps its index
    /// identity, so nothing below it shifts.
    #[test]
    fn a_dropped_slot_keeps_its_identity_when_interleaving() {
        let logical = IRouteRule::Logical {
            op: LogicOp::Or,
            rules: vec![IRouteRule::Simple {
                matches: vec![MatchField::Domain("blocked.example".into())],
                target: RuleTarget::Direct,
            }],
            target: RuleTarget::Block,
        };
        let order = RuleOrder {
            logical: vec![logical],
            entries: vec![
                RuleOrderEntry::Profile(0),
                RuleOrderEntry::Logical(0),
                RuleOrderEntry::Profile(1),
            ],
            profile: None,
        };
        // P0 dropped by the conversion, P1 = the catch-all.
        let slots: Vec<Option<Value>> = vec![None, Some(serde_json::json!({ "outbound": "direct" }))];
        let merged = interleave_route_slots(&slots, &order);
        assert_eq!(merged.len(), 2, "{merged:?}");
        assert_eq!(merged[0]["type"], Value::from("logical"));
        assert_eq!(merged[1]["outbound"], Value::from("direct"));

        // Interleaving the COMPRESSED list instead — the old behaviour — is
        // what put the block rule after the catch-all.
        let compressed: Vec<Option<Value>> = slots.iter().flatten().cloned().map(Some).collect();
        let broken = interleave_route_slots(&compressed, &order);
        assert_eq!(broken[0]["outbound"], Value::from("direct"));
        assert_eq!(broken[1]["type"], Value::from("logical"));
    }

    #[test]
    fn a_corrupt_rule_order_is_reported_rather_than_guessed() {
        let home = temp_home("rule-order-corrupt");
        std::fs::write(home.join(LOGICAL_RULES_FILE), r#"{"version":2,"entries":[]}"#).expect("write");
        assert!(
            load_rule_order(&home)
                .unwrap_err()
                .contains("unsupported rule order version")
        );
        std::fs::write(
            home.join(LOGICAL_RULES_FILE),
            r#"{"rules":[],"entries":[{"profile":0,"logical":0}]}"#,
        )
        .expect("write");
        assert!(load_rule_order(&home).unwrap_err().contains("exactly one"));
        std::fs::write(home.join(LOGICAL_RULES_FILE), r#"{"rules":[]}"#).expect("write");
        assert!(load_rule_order(&home).unwrap_err().contains("entries"));
        assert!(load_logical_rules(&home).is_err());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn dns_spec_round_trips_through_storage() {
        let home = temp_home("dns");
        assert_eq!(load_dns_spec(&home).unwrap(), DnsConfigSpec::default());

        let spec = DnsConfigSpec {
            servers: vec![DnsServerSpec {
                tag: "dns-local".into(),
                kind: DnsServerKind::Local,
                server: None,
                server_port: None,
                detour: None,
                tls_insecure: None,
                path: None,
                inet4_range: None,
                inet6_range: None,
            }],
            rules: Vec::new(),
            domain_resolver: None,
            final_server: None,
        };
        save_dns_spec(&home, &spec).expect("save");
        assert_eq!(load_dns_spec(&home).unwrap(), spec);
        let mut invalid = spec.clone();
        invalid.domain_resolver = Some("missing-server".into());
        assert!(
            save_dns_spec(&home, &invalid)
                .unwrap_err()
                .contains("unknown DNS bootstrap resolver")
        );
        assert_eq!(
            load_dns_spec(&home).unwrap(),
            spec,
            "invalid reference does not replace prior settings"
        );

        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn malformed_storage_is_reported_and_atomic_validation_preserves_previous_file() {
        let home = temp_home("malformed");
        std::fs::write(home.join(DNS_CONFIG_FILE), "{").expect("corrupt fixture");
        assert!(load_dns_spec(&home).unwrap_err().contains("parse"));
        std::fs::write(
            home.join(DNS_CONFIG_FILE),
            r#"{"servers":[{"tag":"upstream","kind":"udp"}]}"#,
        )
        .expect("semantically invalid fixture");
        let error = load_dns_spec(&home).unwrap_err();
        assert!(
            error.contains(DNS_CONFIG_FILE) && error.contains("requires an address"),
            "{error}"
        );
        let old = "[{}]";
        std::fs::write(home.join(RULE_SETS_FILE), old).expect("old data");
        assert!(save_rule_sets(&home, &[serde_json::json!({ "tag": "" })]).is_err());
        assert_eq!(std::fs::read_to_string(home.join(RULE_SETS_FILE)).unwrap(), old);
        assert!(
            !std::fs::read_dir(&home).unwrap().any(|entry| entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".tmp"))
        );
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn invalid_rule_set_reference_and_remote_schema_do_not_replace_valid_storage() {
        let home = temp_home("rule-set-validation");
        let valid = [serde_json::json!({"tag":"old","type":"local","path":"old.srs"})];
        save_rule_sets(&home, &valid).expect("initial save");
        assert!(save_rule_sets(&home, &[serde_json::json!({"tag":"remote","type":"remote"})]).is_err());
        assert_eq!(load_rule_sets(&home).unwrap(), valid);
        let dangling = vec![IRouteRule::Logical {
            op: LogicOp::And,
            rules: vec![IRouteRule::Simple {
                matches: vec![MatchField::RuleSet("missing".into())],
                target: RuleTarget::Direct,
            }],
            target: RuleTarget::Direct,
        }];
        assert!(
            save_logical_rules(&home, &dangling)
                .unwrap_err()
                .contains("unknown rule-set")
        );
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn atomic_rename_failure_keeps_previous_json_and_cleans_temporary_file() {
        let home = temp_home("rename-failure");
        let path = home.join(DNS_CONFIG_FILE);
        std::fs::write(&path, "{\"servers\": []}").expect("seed old config");
        let error = atomic_write_using(&home, DNS_CONFIG_FILE, b"{\"servers\":[{}]}", |_, _| {
            Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "injected rename failure",
            ))
        })
        .unwrap_err();
        assert!(error.to_string().contains("injected rename failure"));
        assert_eq!(std::fs::read_to_string(path).unwrap(), "{\"servers\": []}");
        assert!(
            std::fs::read_dir(&home).unwrap().all(|entry| !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".tmp"))
        );
        let _ = std::fs::remove_dir_all(home);
    }
}
