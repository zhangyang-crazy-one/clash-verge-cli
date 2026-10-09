// Chain proxy: dialer-proxy chain management.
// Builds a chain of nodes: entry → hop1 → hop2 → exit.
// Each node (except first) gets "dialer-proxy": <previous_node>.
// Applied via PUT /configs?force=true with the modified config.

use anyhow::{Context, anyhow};
use clash_verge_core::config::PrfItem;
use serde_yaml_ng::{Mapping, Sequence, Value};
use std::collections::HashSet;
use std::path::Path;

/// Supported chain types loaded from profile enhancement files.
pub enum ChainType {
    Merge(Mapping),
    Script { source: String, path: std::path::PathBuf },
    Rules(RulesFragment),
    Proxies(SequenceFragment),
    Groups(SequenceFragment),
}

/// Resolve the enhancement chain described by a local profile.
pub async fn resolve_chain(item: &PrfItem, profiles_dir: &Path) -> anyhow::Result<ChainType> {
    let itype = item.itype.as_deref().context("profile has no type")?;
    let file = item.file.as_deref().context("profile has no file")?;
    let path = profiles_dir.join(file);

    if !path.exists() {
        anyhow::bail!("profile file not found: {}", path.display());
    }

    let raw = tokio::fs::read_to_string(&path)
        .await
        .with_context(|| format!("failed to read {}", path.display()))?;

    match itype {
        "merge" => {
            let value: Value =
                serde_yaml_ng::from_str(&raw).with_context(|| format!("invalid YAML in {}", path.display()))?;
            let map = match value {
                Value::Null => Mapping::new(),
                Value::Mapping(map) => map,
                _ => anyhow::bail!("merge fragment must be a mapping in {}", path.display()),
            };
            Ok(ChainType::Merge(map))
        }
        "script" => Ok(ChainType::Script { source: raw, path }),
        "rules" | "proxies" | "groups" => match itype {
            "rules" => Ok(ChainType::Rules(parse_rules_fragment(&raw, &path)?)),
            "proxies" => Ok(ChainType::Proxies(parse_sequence_fragment(&raw, &path)?)),
            "groups" => Ok(ChainType::Groups(parse_sequence_fragment(&raw, &path)?)),
            _ => unreachable!("chain type was matched above"),
        },
        _ => anyhow::bail!("unsupported chain type: {itype}"),
    }
}

/// GUI sequence enhancements and the standalone legacy whole-list form.
pub enum SequenceFragment {
    Patch {
        prepend: Sequence,
        append: Sequence,
        delete: Vec<String>,
    },
    Replace(Sequence),
}

fn parse_sequence_fragment(raw: &str, path: &Path) -> anyhow::Result<SequenceFragment> {
    let value: Value = serde_yaml_ng::from_str(raw).with_context(|| format!("invalid YAML in {}", path.display()))?;
    match value {
        Value::Sequence(seq) => Ok(SequenceFragment::Replace(seq)),
        Value::Mapping(mut map) => {
            if map
                .keys()
                .any(|key| !key.as_str().is_some_and(|key| MAPPING_KEYS.contains(&key)))
            {
                anyhow::bail!("unsupported sequence fragment key in {}", path.display());
            }
            let mut sequence = |key: &str| -> anyhow::Result<Sequence> {
                match map.remove(key) {
                    None => Ok(Vec::new()),
                    Some(Value::Sequence(seq)) => Ok(seq),
                    _ => anyhow::bail!("{key} must be an array in {}", path.display()),
                }
            };
            let prepend = sequence("prepend")?;
            let append = sequence("append")?;
            let delete = sequence("delete")?;
            Ok(SequenceFragment::Patch {
                prepend,
                append,
                delete: parse_sequence_of_strings(delete, path)?,
            })
        }
        _ => anyhow::bail!("sequence fragment must be a mapping or sequence in {}", path.display()),
    }
}

fn apply_sequence_fragment(config: &mut Mapping, fragment: &SequenceFragment, field: &str) {
    let SequenceFragment::Patch {
        prepend,
        append,
        delete,
    } = fragment
    else {
        if let SequenceFragment::Replace(seq) = fragment {
            config.insert(field.into(), seq.clone().into());
        }
        return;
    };
    let name = |value: &Value| {
        value
            .as_str()
            .or_else(|| {
                value
                    .as_mapping()
                    .and_then(|map| map.get("name"))
                    .and_then(Value::as_str)
            })
            .map(str::to_owned)
    };
    let old = config
        .remove(field)
        .and_then(|value| value.as_sequence().cloned())
        .unwrap_or_default();
    let values: Sequence = prepend
        .iter()
        .cloned()
        .chain(
            old.into_iter()
                .filter(|value| !name(value).is_some_and(|name| delete.contains(&name))),
        )
        .chain(append.iter().cloned())
        .collect();
    config.insert(field.into(), values.into());
    if field != "proxies" {
        return;
    }
    let mut seen = HashSet::new();
    let added: Vec<String> = prepend
        .iter()
        .chain(append.iter())
        .filter_map(name)
        .filter(|name| seen.insert(name.clone()))
        .collect();
    let Some(Value::Sequence(groups)) = config.get_mut("proxy-groups") else {
        return;
    };
    let mut first_selector = true;
    for group in groups {
        let Some(group) = group.as_mapping_mut() else {
            continue;
        };
        if let Some(Value::Sequence(proxies)) = group.get_mut("proxies") {
            proxies.retain(|value| {
                !value
                    .as_str()
                    .is_some_and(|value| delete.iter().any(|name| name == value))
            });
        }
        if first_selector
            && !added.is_empty()
            && group
                .get("type")
                .and_then(Value::as_str)
                .is_some_and(|kind| kind.eq_ignore_ascii_case("select") || kind.eq_ignore_ascii_case("selector"))
        {
            let previous = group.remove("proxies");
            let mut names = HashSet::new();
            let values = added
                .iter()
                .cloned()
                .map(Value::from)
                .chain(
                    previous
                        .and_then(|value| value.as_sequence().cloned())
                        .unwrap_or_default(),
                )
                .filter(|value| value.as_str().is_none_or(|name| names.insert(name.to_owned())))
                .collect::<Sequence>();
            group.insert("proxies".into(), values.into());
            first_selector = false;
        }
    }
}

/// Copy a merge profile's section into the active Clash configuration.
pub fn is_noop(chain: &ChainType) -> bool {
    match chain {
        ChainType::Merge(map) => map.is_empty(),
        ChainType::Script { source, .. } => source.trim() == clash_verge_core::utils::tmpl::ITEM_SCRIPT.trim(),
        ChainType::Rules(RulesFragment::Mapping {
            prepend,
            append,
            delete,
        }) => prepend.is_empty() && append.is_empty() && delete.is_empty(),
        ChainType::Rules(RulesFragment::Sequence(seq)) => seq.is_empty(),
        ChainType::Proxies(fragment) | ChainType::Groups(fragment) => match fragment {
            SequenceFragment::Patch {
                prepend,
                append,
                delete,
            } => prepend.is_empty() && append.is_empty() && delete.is_empty(),
            SequenceFragment::Replace(seq) => seq.is_empty(),
        },
    }
}

/// Overlay the standalone `dns_config.yaml` section onto a profile's `dns`
/// mapping. Maps merge recursively so valid unknown nested fields survive;
/// scalar and sequence values replace the old value, including an explicit
/// empty sequence which clears an inherited list.
pub fn apply_dns_override(config: &mut Mapping, dns_override: &Mapping) -> anyhow::Result<()> {
    if dns_override.is_empty() {
        return Ok(());
    }

    let dns_key = Value::from("dns");
    let mut dns = match config.get(&dns_key) {
        Some(value) => value
            .as_mapping()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("profile dns section must be a mapping"))?,
        None => Mapping::new(),
    };
    merge_mapping_values(&mut dns, dns_override);
    config.insert(dns_key, Value::Mapping(dns));
    Ok(())
}

fn merge_mapping_values(base: &mut Mapping, overlay: &Mapping) {
    for (key, overlay_value) in overlay {
        if let (Some(Value::Mapping(base_nested)), Value::Mapping(overlay_nested)) = (base.get_mut(key), overlay_value)
            && !overlay_nested.is_empty()
        {
            merge_mapping_values(base_nested, overlay_nested);
            continue;
        }
        base.insert(key.clone(), overlay_value.clone());
    }
}

fn merge_gui_values(base: &mut Mapping, overlay: &Mapping) {
    for (key, value) in overlay {
        if let (Some(Value::Mapping(existing)), Value::Mapping(nested)) = (base.get_mut(key), value) {
            merge_gui_values(existing, nested);
        } else {
            base.insert(key.clone(), value.clone());
        }
    }
}

/// Parsed rule-fragment shape. Either a GUI mapping with prepend/append/delete
/// string arrays, or a legacy YAML sequence of rule strings that fully
/// replaces the upstream rule list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RulesFragment {
    /// GUI mapping: `prepend` rules come first, then the upstream rules with
    /// any exact full-string match in `delete` removed (in their original
    /// positions), then `append` rules.
    Mapping {
        prepend: Vec<String>,
        append: Vec<String>,
        delete: Vec<String>,
    },
    /// Legacy sequence: the upstream rules are dropped wholesale; the
    /// fragment's strings become the final rule list.
    Sequence(Vec<String>),
}

/// Allowed keys in a [`RulesFragment::Mapping`] fragment. Extra keys are
/// rejected so a typo (e.g. `appends`) does not silently bypass the override.
const MAPPING_KEYS: &[&str] = &["prepend", "append", "delete"];

/// Parse a rules-fragment YAML document.
///
/// `source_path` is cited in error messages so the user can locate the bad
/// file. Accepts:
/// - A mapping with optional `prepend`, `append`, `delete` arrays of strings.
/// - A sequence of rule strings (legacy complete-list form).
///
/// Any other shape, non-string list item, unknown key, or malformed YAML is
/// rejected with a contextual error.
pub fn parse_rules_fragment(raw: &str, source_path: &Path) -> anyhow::Result<RulesFragment> {
    let value: Value =
        serde_yaml_ng::from_str(raw).with_context(|| format!("invalid rules YAML in {}", source_path.display()))?;

    match value {
        Value::Mapping(map) => {
            let mut prepend: Vec<String> = Vec::new();
            let mut append: Vec<String> = Vec::new();
            let mut delete: Vec<String> = Vec::new();
            for (key, val) in map {
                let Some(name) = key.as_str() else {
                    return Err(anyhow!(
                        "rules fragment in {} uses a non-string key",
                        source_path.display()
                    ));
                };
                match name {
                    "prepend" => prepend = parse_string_array(val, "prepend", source_path)?,
                    "append" => append = parse_string_array(val, "append", source_path)?,
                    "delete" => delete = parse_string_array(val, "delete", source_path)?,
                    other => {
                        return Err(anyhow!(
                            "unsupported key {other:?} in rules fragment {}; expected one of {:?}",
                            source_path.display(),
                            MAPPING_KEYS
                        ));
                    }
                }
            }
            Ok(RulesFragment::Mapping {
                prepend,
                append,
                delete,
            })
        }
        Value::Sequence(seq) => {
            let rules = parse_sequence_of_strings(seq, source_path)?;
            Ok(RulesFragment::Sequence(rules))
        }
        other => Err(anyhow!(
            "rules fragment in {} must be a mapping or a sequence, got a {} value",
            source_path.display(),
            value_kind(&other)
        )),
    }
}

fn parse_string_array(value: Value, key: &str, source_path: &Path) -> anyhow::Result<Vec<String>> {
    let Value::Sequence(seq) = value else {
        return Err(anyhow!(
            "rules fragment in {} has non-array value for {key:?}",
            source_path.display()
        ));
    };
    parse_sequence_of_strings(seq, source_path)
}

fn parse_sequence_of_strings(seq: Sequence, source_path: &Path) -> anyhow::Result<Vec<String>> {
    let mut out = Vec::with_capacity(seq.len());
    for (index, entry) in seq.into_iter().enumerate() {
        let Value::String(rule) = entry else {
            return Err(anyhow!(
                "rules fragment in {} has non-string rule at index {index}",
                source_path.display()
            ));
        };
        out.push(rule);
    }
    Ok(out)
}

fn value_kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Sequence(_) => "sequence",
        Value::Mapping(_) => "mapping",
        Value::Tagged(_) => "tagged",
    }
}

/// Apply a parsed fragment to a clash `Mapping`, replacing the `rules` key.
///
/// For [`RulesFragment::Mapping`]: prepend rules come first, then every
/// upstream entry whose exact full-string text is in `delete` is removed
/// (non-string upstream entries — e.g. mapping-form rules from a provider —
/// are preserved in their original positions), then the `append` rules.
///
/// For [`RulesFragment::Sequence`]: the upstream rules are dropped wholesale
/// and replaced by the fragment's strings.
pub fn apply_rules_fragment(config: &mut Mapping, fragment: &RulesFragment) {
    let upstream_array: Sequence = match config.get("rules") {
        Some(Value::Sequence(seq)) => seq.clone(),
        _ => Sequence::new(),
    };

    let new_array: Sequence = match fragment {
        RulesFragment::Sequence(rules) => rules.iter().cloned().map(Value::String).collect(),
        RulesFragment::Mapping {
            prepend,
            append,
            delete,
        } => {
            let delete_set: HashSet<&String> = delete.iter().collect();
            let mut result: Vec<Value> = Vec::with_capacity(prepend.len() + upstream_array.len() + append.len());
            for rule in prepend {
                result.push(Value::String(rule.clone()));
            }
            for entry in &upstream_array {
                let drop = matches!(entry, Value::String(rule) if delete_set.contains(rule));
                if !drop {
                    result.push(entry.clone());
                }
            }
            for rule in append {
                result.push(Value::String(rule.clone()));
            }
            result
        }
    };

    config.insert("rules".into(), Value::Sequence(new_array));
}

/// Apply a resolved profile chain without discarding unrelated configuration.
#[cfg(test)]
pub fn apply_chain_to_config(config: &mut Mapping, chain: &ChainType) -> anyhow::Result<()> {
    apply_chain_to_profile(config, chain, "")
}

pub fn apply_chain_to_profile(config: &mut Mapping, chain: &ChainType, profile_name: &str) -> anyhow::Result<()> {
    match chain {
        ChainType::Merge(merge) => {
            // GUI v2.5.7: lowercase top-level keys; DNS overlays its child
            // keys, hosts replaces, and other mappings merge recursively.
            for (key, value) in merge {
                let key = key
                    .as_str()
                    .map(|key| Value::from(key.to_ascii_lowercase()))
                    .unwrap_or_else(|| key.clone());
                if key.as_str() == Some("dns") {
                    let dns = value
                        .as_mapping()
                        .context("merge profile dns section must be a mapping")?;
                    match config.get_mut(&key) {
                        Some(Value::Mapping(existing)) => existing.extend(dns.clone()),
                        Some(_) => anyhow::bail!("profile dns section must be a mapping"),
                        None => {
                            config.insert(key, value.clone());
                        }
                    }
                } else if key.as_str() != Some("hosts")
                    && let (Some(Value::Mapping(existing)), Value::Mapping(overlay)) = (config.get_mut(&key), value)
                {
                    merge_gui_values(existing, overlay);
                } else {
                    config.insert(key, value.clone());
                }
            }
        }
        ChainType::Rules(fragment) => apply_rules_fragment(config, fragment),
        ChainType::Proxies(fragment) => apply_sequence_fragment(config, fragment, "proxies"),
        ChainType::Groups(fragment) => apply_sequence_fragment(config, fragment, "proxy-groups"),
        ChainType::Script { source, path } => {
            let enhanced = crate::profile_script::evaluate(config, source, profile_name, path)?;
            *config = enhanced;
        }
    }
    Ok(())
}

#[cfg(test)]
mod dns_merge_tests {
    use super::{ChainType, apply_chain_to_config, apply_dns_override};
    use serde_yaml_ng::{Mapping, Value};

    fn parse(yaml: &str) -> Mapping {
        serde_yaml_ng::from_str(yaml).expect("valid fixture")
    }

    #[test]
    fn global_dns_override_is_recursive_and_empty_lists_clear() {
        let mut profile = parse(
            "dns:\n  nameserver: [profile-dns]\n  future: {keep: true, replace: old}\n  nameserver-policy: {a.example: [1.1.1.1]}\n",
        );
        let global = parse("nameserver: []\nfuture: {replace: new, nested: [preserved]}\nnameserver-policy: {}\n");
        apply_dns_override(&mut profile, &global).expect("valid profile DNS section");

        assert_eq!(profile["dns"]["nameserver"], Value::Sequence(Vec::new()));
        assert_eq!(profile["dns"]["future"]["keep"], Value::from(true));
        assert_eq!(profile["dns"]["future"]["replace"], Value::from("new"));
        assert_eq!(profile["dns"]["future"]["nested"][0], Value::from("preserved"));
        assert_eq!(profile["dns"]["nameserver-policy"], Value::Mapping(Mapping::new()));
    }

    #[test]
    fn malformed_profile_dns_is_reported_without_defaulting_or_replacing_it() {
        let mut profile = parse("dns: invalid-scalar\n");
        let before = profile.clone();
        let global = parse("nameserver: [global]\n");
        assert!(apply_dns_override(&mut profile, &global).is_err());
        assert_eq!(profile, before);
    }

    #[test]
    fn merge_profile_overlays_dns_children_and_unset_fields_inherit() {
        let mut config = parse("dns: {nameserver: [profile], future: {keep: true, change: old}}\n");
        let merge = parse("dns: {future: {change: new}, fallback: [8.8.8.8]}\n");
        apply_chain_to_config(&mut config, &ChainType::Merge(merge)).expect("valid DNS merge");

        assert_eq!(config["dns"]["nameserver"][0], Value::from("profile"));
        assert!(config["dns"]["future"].as_mapping().unwrap().get("keep").is_none());
        assert_eq!(config["dns"]["future"]["change"], Value::from("new"));
        assert_eq!(config["dns"]["fallback"][0], Value::from("8.8.8.8"));
    }

    #[test]
    fn empty_merge_dns_inherits_existing_children_like_gui() {
        let mut config = parse("dns: {nameserver: [profile]}\n");
        let merge = parse("dns: {}\n");
        apply_chain_to_config(&mut config, &ChainType::Merge(merge)).expect("valid empty DNS merge");
        assert_eq!(config["dns"]["nameserver"][0], Value::from("profile"));
    }

    #[test]
    fn gui_merge_preserves_empty_deep_maps_and_replaces_dns_children_and_hosts() {
        let mut config = parse(
            "future: {Nested: {keep: true}}\ndns: {nameserver: [old], nested: {keep: true}}\nhosts: {old: 192.0.2.1}\n",
        );
        let merge = parse("FUTURE: {Nested: {}}\nDNS: {nested: {new: true}}\nHOSTS: {}\n");
        apply_chain_to_config(&mut config, &ChainType::Merge(merge)).unwrap();
        assert_eq!(config["future"]["Nested"]["keep"], Value::from(true));
        assert_eq!(config["dns"]["nameserver"][0], Value::from("old"));
        assert!(config["dns"]["nested"].as_mapping().unwrap().get("keep").is_none());
        assert_eq!(config["hosts"], Value::Mapping(Mapping::new()));
    }

    #[test]
    fn script_chain_is_rejected_before_any_config_mutation() {
        let mut config = parse("dns: {nameserver: [profile]}\n");
        let before = config.clone();
        assert!(
            apply_chain_to_config(
                &mut config,
                &ChainType::Script {
                    source: "function main(c) { throw new Error('fixture'); }".into(),
                    path: "fixture.js".into()
                }
            )
            .is_err()
        );
        assert_eq!(config, before);
    }
}

/// Configure an ordered entry -> hop -> exit chain in the YAML `proxies` list.
/// Each hop after the entry dials through its preceding proxy.
pub fn build_chain_config(chain_nodes: &[String], proxies: &mut [Mapping]) -> anyhow::Result<()> {
    if chain_nodes.len() < 2 {
        anyhow::bail!("chain requires at least 2 nodes (entry + exit)");
    }

    for node in chain_nodes {
        let found = proxies
            .iter()
            .any(|proxy| proxy.get("name").and_then(Value::as_str) == Some(node));
        if !found {
            anyhow::bail!("chain node is not a configured outbound proxy: {node}");
        }
    }

    for proxy in proxies {
        let Some(name) = proxy.get("name").and_then(Value::as_str) else {
            continue;
        };
        let Some(position) = chain_nodes.iter().position(|node| node == name) else {
            continue;
        };

        if position == 0 {
            proxy.remove("dialer-proxy");
        } else {
            proxy.insert("dialer-proxy".into(), Value::String(chain_nodes[position - 1].clone()));
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proxy(name: &str) -> Mapping {
        let mut proxy = Mapping::new();
        proxy.insert("name".into(), Value::String(name.into()));
        proxy.insert("type".into(), Value::String("ss".into()));
        proxy
    }

    fn dialer(proxy: &Mapping) -> Option<&str> {
        proxy.get("dialer-proxy").and_then(Value::as_str)
    }

    #[test]
    fn builds_entry_to_exit_dialer_chain_in_a_proxy_list() {
        let mut entry = proxy("entry");
        entry.insert("dialer-proxy".into(), Value::String("stale".into()));
        let mut proxies = vec![entry, proxy("exit"), proxy("unchanged")];

        build_chain_config(&["entry".into(), "exit".into()], &mut proxies).expect("chain config");

        assert_eq!(dialer(&proxies[0]), None);
        assert_eq!(dialer(&proxies[1]), Some("entry"));
        assert_eq!(dialer(&proxies[2]), None);
    }

    #[test]
    fn missing_chain_node_rejects_without_mutating_proxy_list() {
        let mut proxies = vec![proxy("entry"), proxy("exit")];
        let before = proxies.clone();

        assert!(build_chain_config(&["entry".into(), "missing".into()], &mut proxies).is_err());
        assert_eq!(proxies, before);
    }

    #[test]
    fn parse_rules_fragment_accepts_gui_mapping_with_all_three_arrays() {
        let raw = "\
prepend:
  - DOMAIN,foo.com,DIRECT
append:
  - MATCH,PROXY
delete:
  - DOMAIN-KEYWORD,ads
";
        let path = std::path::Path::new("/tmp/rules-demo.yaml");
        let fragment = parse_rules_fragment(raw, path).expect("parse");
        assert_eq!(
            fragment,
            RulesFragment::Mapping {
                prepend: vec!["DOMAIN,foo.com,DIRECT".into()],
                append: vec!["MATCH,PROXY".into()],
                delete: vec!["DOMAIN-KEYWORD,ads".into()],
            }
        );
    }

    #[test]
    fn parse_rules_fragment_accepts_empty_gui_mapping() {
        let raw = "prepend: []\nappend: []\ndelete: []\n";
        let path = std::path::Path::new("/tmp/rules-empty.yaml");
        assert_eq!(
            parse_rules_fragment(raw, path).expect("parse"),
            RulesFragment::Mapping {
                prepend: vec![],
                append: vec![],
                delete: vec![]
            }
        );
    }

    #[test]
    fn parse_rules_fragment_accepts_legacy_sequence_of_strings() {
        let raw = "- DOMAIN,a.com,DIRECT\n- IP-CIDR,1.0.0.0/8,DIRECT\n- MATCH,PROXY\n";
        let path = std::path::Path::new("/tmp/rules-legacy.yaml");
        assert_eq!(
            parse_rules_fragment(raw, path).expect("parse"),
            RulesFragment::Sequence(vec![
                "DOMAIN,a.com,DIRECT".into(),
                "IP-CIDR,1.0.0.0/8,DIRECT".into(),
                "MATCH,PROXY".into(),
            ])
        );
    }

    #[test]
    fn parse_rules_fragment_rejects_unknown_mapping_key() {
        let raw = "prepend: []\nappends: []\n";
        let path = std::path::Path::new("/tmp/rules-bad-key.yaml");
        let error = parse_rules_fragment(raw, path).expect_err("unknown key must reject");
        assert!(
            error.to_string().contains("appends"),
            "error cites the bad key: {error}"
        );
        assert!(
            error.to_string().contains("/tmp/rules-bad-key.yaml"),
            "error cites the file: {error}"
        );
    }

    #[test]
    fn parse_rules_fragment_rejects_non_string_rule_in_sequence() {
        let raw = "- DOMAIN,a.com,DIRECT\n- {IP-CIDR: 1.0.0.0/8, POLICY: DIRECT}\n";
        let path = std::path::Path::new("/tmp/rules-mixed.yaml");
        let error = parse_rules_fragment(raw, path).expect_err("non-string rule must reject");
        assert!(
            error.to_string().contains("non-string"),
            "error explains the failure: {error}"
        );
    }

    #[test]
    fn parse_rules_fragment_rejects_scalar_root() {
        let raw = "just-a-string\n";
        let path = std::path::Path::new("/tmp/rules-scalar.yaml");
        assert!(parse_rules_fragment(raw, path).is_err(), "scalar root must reject");
    }

    #[test]
    fn parse_rules_fragment_rejects_non_array_value_for_prepend() {
        let raw = "prepend: DOMAIN,foo.com,DIRECT\n";
        let path = std::path::Path::new("/tmp/rules-prepend-not-array.yaml");
        assert!(parse_rules_fragment(raw, path).is_err());
    }

    /// Apply a fragment to an upstream string list via the production code
    /// path (`apply_rules_fragment`) and read the resulting rules back.
    fn apply_to_rules(upstream: &[&str], fragment: &RulesFragment) -> Vec<String> {
        let mut config = Mapping::new();
        config.insert(
            "rules".into(),
            Value::Sequence(upstream.iter().map(|rule| Value::String((*rule).into())).collect()),
        );
        apply_rules_fragment(&mut config, fragment);
        config
            .get("rules")
            .and_then(Value::as_sequence)
            .expect("rules sequence")
            .iter()
            .filter_map(|value| value.as_str().map(str::to_owned))
            .collect()
    }

    #[test]
    fn compose_rules_mapping_prepends_appends_and_drops_exact_matches() {
        let fragment = RulesFragment::Mapping {
            prepend: vec!["P".into()],
            append: vec!["Q".into()],
            delete: vec!["B".into()],
        };
        assert_eq!(apply_to_rules(&["A", "B", "C"], &fragment), vec!["P", "A", "C", "Q"]);
    }

    #[test]
    fn compose_rules_empty_mapping_preserves_upstream_in_order() {
        let fragment = RulesFragment::Mapping {
            prepend: vec![],
            append: vec![],
            delete: vec![],
        };
        assert_eq!(apply_to_rules(&["A", "B", "C"], &fragment), vec!["A", "B", "C"]);
    }

    #[test]
    fn compose_rules_sequence_replaces_upstream_wholesale() {
        let fragment = RulesFragment::Sequence(vec!["X".into(), "Y".into()]);
        assert_eq!(apply_to_rules(&["A", "B", "C"], &fragment), vec!["X", "Y"]);
    }

    #[test]
    fn compose_rules_mapping_removes_every_exact_string_delete_match() {
        let fragment = RulesFragment::Mapping {
            prepend: vec![],
            append: vec![],
            delete: vec!["B".into()],
        };
        assert_eq!(apply_to_rules(&["A", "B", "A", "B"], &fragment), vec!["A", "A"]);
    }

    #[test]
    fn apply_rules_fragment_mapping_replaces_rules_key_with_composed_value() {
        let mut config: Mapping = serde_yaml_ng::from_str(r#"{rules: [A, B, C]}"#).expect("upstream yaml");
        let fragment = RulesFragment::Mapping {
            prepend: vec!["P".into()],
            append: vec!["Q".into()],
            delete: vec!["B".into()],
        };
        apply_rules_fragment(&mut config, &fragment);
        let rules: Vec<String> = config
            .get("rules")
            .and_then(Value::as_sequence)
            .expect("rules sequence")
            .iter()
            .filter_map(|v| v.as_str().map(str::to_owned))
            .collect();
        assert_eq!(rules, vec!["P", "A", "C", "Q"]);
    }

    #[test]
    fn apply_rules_fragment_sequence_replaces_rules_key_wholesale() {
        let mut config: Mapping = serde_yaml_ng::from_str(r#"{rules: [A, B, C]}"#).expect("upstream yaml");
        let fragment = RulesFragment::Sequence(vec!["X".into(), "Y".into()]);
        apply_rules_fragment(&mut config, &fragment);
        let rules: Vec<String> = config
            .get("rules")
            .and_then(Value::as_sequence)
            .expect("rules sequence")
            .iter()
            .filter_map(|v| v.as_str().map(str::to_owned))
            .collect();
        assert_eq!(rules, vec!["X", "Y"]);
    }

    #[test]
    fn apply_rules_fragment_mapping_preserves_non_string_upstream_entries() {
        // A mapping-form rule (provider output) sits between string rules and
        // must remain at its original position relative to surviving strings.
        let mut config: Mapping = serde_yaml_ng::from_str(
            "rules:\n  - A\n  - type: IP-CIDR\n    payload: 1.0.0.0/8\n    policy: DIRECT\n  - C\n",
        )
        .expect("upstream yaml");
        let fragment = RulesFragment::Mapping {
            prepend: vec!["P".into()],
            append: vec!["Q".into()],
            delete: vec![],
        };
        apply_rules_fragment(&mut config, &fragment);
        let sequence = config
            .get("rules")
            .and_then(Value::as_sequence)
            .expect("rules sequence");
        assert_eq!(sequence.len(), 5, "prepend + 3 upstream + append = 5");
        assert_eq!(sequence[0].as_str(), Some("P"));
        assert_eq!(sequence[1].as_str(), Some("A"));
        assert!(
            sequence[2].as_mapping().is_some(),
            "non-string rule kept at index 2 (its original position)"
        );
        assert_eq!(sequence[3].as_str(), Some("C"));
        assert_eq!(sequence[4].as_str(), Some("Q"));
    }

    #[test]
    fn apply_rules_fragment_mapping_keeps_non_string_entry_after_deletion_of_strings() {
        // Non-string entries are not subject to exact-string deletion, so the
        // mapping must stay even when its neighboring string is removed.
        let mut config: Mapping = serde_yaml_ng::from_str(
            "rules:\n  - A\n  - B\n  - type: IP-CIDR\n    payload: 1.0.0.0/8\n    policy: DIRECT\n",
        )
        .expect("upstream yaml");
        let fragment = RulesFragment::Mapping {
            prepend: vec![],
            append: vec![],
            delete: vec!["B".into()],
        };
        apply_rules_fragment(&mut config, &fragment);
        let sequence = config
            .get("rules")
            .and_then(Value::as_sequence)
            .expect("rules sequence");
        assert_eq!(sequence.len(), 2);
        assert_eq!(sequence[0].as_str(), Some("A"));
        assert!(sequence[1].as_mapping().is_some(), "non-string rule kept");
    }
}
