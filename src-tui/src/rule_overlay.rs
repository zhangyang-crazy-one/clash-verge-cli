//! Per-profile edits applied after the GUI-compatible enhancement chain.
//! Source rules use exact text plus occurrence identity; refreshes keep new
//! source rules rather than freezing the subscription at the time of editing.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RuleOverlay {
    version: u32,
    source: Vec<String>,
    entries: Vec<Entry>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case", deny_unknown_fields)]
enum Entry {
    Source(usize),
    Added(String),
}

pub(crate) fn path(profiles: &Path, item: &clash_verge_core::config::PrfItem) -> Result<PathBuf, String> {
    let uid = item
        .uid
        .as_deref()
        .filter(|uid| !uid.is_empty())
        .ok_or("profile has no UID for rule overlay")?;
    let digest = Sha256::digest(uid.as_bytes());
    Ok(profiles.join(format!("cli-rule-overlay-{digest:x}.yaml")))
}

fn strings(rules: &[crate::routing::IRouteRule]) -> Result<Vec<String>, String> {
    rules
        .iter()
        .map(|rule| {
            if matches!(rule, crate::routing::IRouteRule::Logical { .. }) {
                return Err("logical rules belong in the sing-box order sidecar".into());
            }
            let raw = match rule {
                crate::routing::IRouteRule::Raw { clash_raw } => clash_raw.clone(),
                other => crate::routing::to_clash_rule_str(other),
            };
            if matches!(rule, crate::routing::IRouteRule::Simple { .. })
                && crate::routing::from_clash_rule_str(&raw) != *rule
            {
                return Err(
                    "rule cannot round-trip through a Clash rule overlay without losing match conditions".into(),
                );
            }
            Ok(raw)
        })
        .collect()
}

impl RuleOverlay {
    pub(crate) fn from_edit(
        source: &[crate::routing::IRouteRule],
        edited: &[crate::routing::IRouteRule],
    ) -> Result<Self, String> {
        let source = strings(source)?;
        let mut occurrences: HashMap<&str, VecDeque<usize>> = HashMap::new();
        for (index, rule) in source.iter().enumerate() {
            occurrences.entry(rule).or_default().push_back(index);
        }
        let entries = strings(edited)?
            .into_iter()
            .map(
                |rule| match occurrences.get_mut(rule.as_str()).and_then(VecDeque::pop_front) {
                    Some(index) => Entry::Source(index),
                    None => Entry::Added(rule),
                },
            )
            .collect();
        Ok(Self {
            version: 1,
            source,
            entries,
        })
    }

    pub(crate) fn serialize(&self) -> Result<String, String> {
        serde_yaml_ng::to_string(self).map_err(|error| error.to_string())
    }

    fn validate(&self) -> Result<(), String> {
        if self.version != 1 {
            return Err(format!("unsupported rule overlay version {}", self.version));
        }
        let mut seen = HashSet::new();
        for entry in &self.entries {
            if let Entry::Source(index) = entry
                && (*index >= self.source.len() || !seen.insert(*index))
            {
                return Err(format!("invalid or repeated rule overlay source index {index}"));
            }
        }
        Ok(())
    }

    fn apply(&self, actual: &[String]) -> Result<Vec<String>, String> {
        self.validate()?;
        let mut source_slots: HashMap<&str, VecDeque<usize>> = HashMap::new();
        for (index, rule) in self.source.iter().enumerate() {
            source_slots.entry(rule).or_default().push_back(index);
        }
        let mut source_to_actual = HashMap::new();
        let mut new_rules = Vec::new();
        for (index, rule) in actual.iter().enumerate() {
            match source_slots.get_mut(rule.as_str()).and_then(VecDeque::pop_front) {
                Some(source) => {
                    source_to_actual.insert(source, index);
                }
                None => new_rules.push(index),
            }
        }
        let mut result: Vec<(Option<usize>, String)> = self
            .entries
            .iter()
            .filter_map(|entry| match entry {
                Entry::Source(source) => source_to_actual
                    .get(source)
                    .map(|index| (Some(*index), actual[*index].clone())),
                Entry::Added(rule) => Some((None, rule.clone())),
            })
            .collect();
        // Determine all insertion buckets against the unmodified edited list.
        // This keeps multiple newly added source rules in source order.
        let mut buckets: Vec<Vec<String>> = vec![Vec::new(); result.len() + 1];
        let positions: HashMap<usize, usize> = result
            .iter()
            .enumerate()
            .filter_map(|(slot, (source, _))| source.map(|source| (source, slot)))
            .collect();
        let mut anchors = vec![None; actual.len()];
        let mut next_anchor = None;
        for index in (0..actual.len()).rev() {
            anchors[index] = next_anchor;
            if let Some(slot) = positions.get(&index) {
                next_anchor = Some(*slot);
            }
        }
        let catch_all = result
            .iter()
            .position(|(_, rule)| is_catch_all(rule))
            .unwrap_or(result.len());
        let mut previous_slot = 0;
        for index in new_rules {
            // Reordered user anchors must not reverse newly supplied rules or
            // place them after a catch-all. Prefer their source order when
            // the two requirements cannot both follow the next anchor.
            let at = anchors[index].unwrap_or(catch_all).min(catch_all).max(previous_slot);
            buckets[at].push(actual[index].clone());
            previous_slot = at;
        }
        let mut output = Vec::new();
        for (index, (_, rule)) in result.drain(..).enumerate() {
            output.append(&mut buckets[index]);
            output.push(rule);
        }
        output.append(buckets.last_mut().expect("at least one bucket"));
        Ok(output)
    }
}

fn is_catch_all(rule: &str) -> bool {
    rule.split(',')
        .next()
        .is_some_and(|kind| matches!(kind.trim(), "MATCH" | "FINAL"))
}

pub(crate) fn validate_document(body: &[u8]) -> Result<(), String> {
    let overlay: RuleOverlay = serde_yaml_ng::from_slice(body).map_err(|error| error.to_string())?;
    overlay.validate()
}

pub(crate) fn apply(
    profiles: &Path,
    item: &clash_verge_core::config::PrfItem,
    profile: &mut serde_yaml_ng::Mapping,
) -> Result<(), String> {
    // Legacy fragments without a UID have no stable overlay identity.
    if item.uid.as_deref().is_none_or(str::is_empty) {
        return Ok(());
    }
    let path = path(profiles, item)?;
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(format!("read {}: {error}", path.display())),
    };
    let overlay: RuleOverlay =
        serde_yaml_ng::from_str(&raw).map_err(|error| format!("parse {}: {error}", path.display()))?;
    let actual = match profile.get("rules") {
        None => Vec::new(),
        Some(serde_yaml_ng::Value::Sequence(rules)) => rules
            .iter()
            .map(|rule| {
                rule.as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| "rule overlay requires string-form source rules".to_string())
            })
            .collect::<Result<Vec<_>, _>>()?,
        Some(_) => return Err("rule overlay source rules must be an array".into()),
    };
    let canonical = strings(
        &actual
            .iter()
            .map(|rule| crate::routing::from_clash_rule_str(rule))
            .collect::<Vec<_>>(),
    )?;
    let rules = overlay
        .apply(&canonical)
        .map_err(|error| format!("{}: {error}", path.display()))?;
    profile.insert(
        "rules".into(),
        serde_yaml_ng::Value::Sequence(rules.into_iter().map(serde_yaml_ng::Value::String).collect()),
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn rules(raw: &[&str]) -> Vec<crate::routing::IRouteRule> {
        raw.iter()
            .map(|raw| crate::routing::IRouteRule::Raw {
                clash_raw: (*raw).into(),
            })
            .collect()
    }
    fn text(raw: &[&str]) -> Vec<String> {
        raw.iter().map(|raw| (*raw).into()).collect()
    }

    #[test]
    fn refresh_keeps_edits_and_new_source_before_match() {
        let overlay = RuleOverlay::from_edit(
            &rules(&["DOMAIN,old,DIRECT", "MATCH,DIRECT"]),
            &rules(&["DOMAIN,local,REJECT", "MATCH,DIRECT"]),
        )
        .unwrap();
        assert_eq!(
            overlay
                .apply(&text(&["DOMAIN,new,DIRECT", "DOMAIN,old,DIRECT", "MATCH,DIRECT"]))
                .unwrap(),
            text(&["DOMAIN,local,REJECT", "DOMAIN,new,DIRECT", "MATCH,DIRECT"])
        );
        assert_eq!(
            overlay.apply(&text(&["DOMAIN,next,DIRECT", "MATCH,DIRECT"])).unwrap(),
            text(&["DOMAIN,local,REJECT", "DOMAIN,next,DIRECT", "MATCH,DIRECT"])
        );
    }

    #[test]
    fn duplicate_occurrences_and_reordering_are_stable() {
        let overlay = RuleOverlay::from_edit(
            &rules(&["A", "B", "A", "MATCH,DIRECT"]),
            &rules(&["B", "A", "MATCH,DIRECT"]),
        )
        .unwrap();
        assert_eq!(
            overlay
                .apply(&text(&["A", "B", "A", "C", "D", "MATCH,DIRECT"]))
                .unwrap(),
            text(&["B", "A", "C", "D", "MATCH,DIRECT"])
        );
        assert_eq!(
            overlay.apply(&text(&["B", "MATCH,DIRECT"])).unwrap(),
            text(&["B", "MATCH,DIRECT"])
        );
    }

    #[test]
    fn new_source_rules_keep_order_across_reordered_anchors() {
        let overlay =
            RuleOverlay::from_edit(&rules(&["A", "B", "MATCH,DIRECT"]), &rules(&["B", "A", "MATCH,DIRECT"])).unwrap();
        assert_eq!(
            overlay.apply(&text(&["X", "A", "Y", "B", "MATCH,DIRECT"])).unwrap(),
            text(&["B", "X", "Y", "A", "MATCH,DIRECT"])
        );
    }

    #[test]
    fn malformed_indices_and_versions_are_rejected() {
        for raw in [
            "version: 2\nsource: []\nentries: []\n",
            "version: 1\nsource: [A]\nentries: [{kind: source, value: 2}]\n",
            "version: 1\nsource: [A]\nentries: [{kind: source, value: 0}, {kind: source, value: 0}]\n",
        ] {
            let overlay: RuleOverlay = serde_yaml_ng::from_str(raw).unwrap();
            assert!(overlay.apply(&text(&["A"])).is_err());
        }
    }

    #[test]
    fn profile_identity_cannot_escape_directory_or_collide() {
        let item = |uid: &str| clash_verge_core::config::PrfItem {
            uid: Some(uid.into()),
            ..Default::default()
        };
        let one = path(Path::new("profiles"), &item("../../elsewhere")).unwrap();
        assert_eq!(one.parent().unwrap(), Path::new("profiles"));
        assert_ne!(one, path(Path::new("profiles"), &item("another")).unwrap());
    }

    #[test]
    fn multiple_match_conditions_are_not_silently_lost() {
        let rule = crate::routing::IRouteRule::Simple {
            matches: vec![
                crate::routing::MatchField::Domain("one.example".into()),
                crate::routing::MatchField::Port(443),
            ],
            target: crate::routing::RuleTarget::Direct,
        };
        let error = RuleOverlay::from_edit(&[], &[rule]).unwrap_err();
        assert!(error.contains("without losing match conditions"));
    }
}
