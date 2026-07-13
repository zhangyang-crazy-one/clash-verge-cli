// Chain proxy: dialer-proxy chain management.
// Builds a chain of nodes: entry → hop1 → hop2 → exit.
// Each node (except first) gets "dialer-proxy": <previous_node>.
// Applied via PUT /configs?force=true with the modified config.

use anyhow::Context;
use clash_verge_core::config::PrfItem;
use serde_yaml_ng::{Mapping, Value};
use std::path::Path;

/// Supported chain types loaded from profile enhancement files.
pub enum ChainType {
    Merge(Mapping),
    Script,
    Rules(Vec<Mapping>),
    Proxies(Vec<Mapping>),
    Groups(Vec<Mapping>),
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
            let map: Mapping =
                serde_yaml_ng::from_str(&raw).with_context(|| format!("invalid YAML in {}", path.display()))?;
            Ok(ChainType::Merge(map))
        }
        "script" => Ok(ChainType::Script),
        "rules" | "proxies" | "groups" => {
            let seq: Vec<Mapping> =
                serde_yaml_ng::from_str(&raw).with_context(|| format!("invalid YAML in {}", path.display()))?;

            match itype {
                "rules" => Ok(ChainType::Rules(seq)),
                "proxies" => Ok(ChainType::Proxies(seq)),
                "groups" => Ok(ChainType::Groups(seq)),
                _ => unreachable!("chain type was matched above"),
            }
        }
        _ => anyhow::bail!("unsupported chain type: {itype}"),
    }
}

/// Copy a merge profile's section into the active Clash configuration.
pub fn apply_merge(base: &mut Mapping, merge: &Mapping, key: &str) {
    if let Some(value) = merge.get(key) {
        base.insert(key.into(), value.clone());
    }
}

/// Apply a resolved profile chain without discarding unrelated configuration.
pub fn apply_chain_to_config(config: &mut Mapping, chain: &ChainType) {
    match chain {
        ChainType::Merge(merge) => {
            for key in ["proxies", "proxy-groups", "rules", "rule-providers", "proxy-providers"] {
                apply_merge(config, merge, key);
            }
        }
        ChainType::Rules(seq) => {
            config.insert("rules".into(), seq.clone().into());
        }
        ChainType::Proxies(seq) => {
            config.insert("proxies".into(), seq.clone().into());
        }
        ChainType::Groups(seq) => {
            config.insert("proxy-groups".into(), seq.clone().into());
        }
        ChainType::Script => {
            // Script execution has a separate runtime and is intentionally not implicit here.
        }
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
}
