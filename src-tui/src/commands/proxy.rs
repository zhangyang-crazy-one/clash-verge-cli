//! `proxy list|select|delay`.

use std::collections::HashMap;
use std::sync::Arc;

use serde::Serialize;

use crate::mihomo_api::types::ProxyGroup;
use crate::mihomo_manager::manager::MihomoManager;
use crate::services::proxy::{delay_many, is_group, last_delay, leaf_targets};

#[derive(Serialize)]
struct GroupSummary<'a> {
    name: &'a str,
    #[serde(rename = "type")]
    group_type: &'a str,
    now: Option<&'a str>,
    members: usize,
}

#[derive(Serialize)]
struct Member<'a> {
    name: &'a str,
    selected: bool,
    /// Last recorded delay in ms; 0 means the last test failed.
    delay: Option<u64>,
}

pub async fn list(manager: &MihomoManager, group: Option<&str>, json: bool) -> anyhow::Result<()> {
    let api = super::running_api(manager).await?;
    let groups = api.get_proxies().await?.proxies;
    match group {
        None => {
            let summaries = group_summaries(&groups);
            if json {
                println!("{}", serde_json::to_string_pretty(&summaries)?);
            } else if summaries.is_empty() {
                println!("(no proxy groups)");
            } else {
                for summary in summaries {
                    println!(
                        "{}\t{}\t{}\t{} members",
                        summary.name,
                        summary.group_type,
                        summary.now.unwrap_or("-"),
                        summary.members
                    );
                }
            }
        }
        Some(name) => {
            let members = group_members(&groups, name)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&members)?);
            } else {
                for member in members {
                    println!(
                        "{} {}\t{}",
                        if member.selected { "*" } else { " " },
                        member.name,
                        format_delay(member.delay)
                    );
                }
            }
        }
    }
    Ok(())
}

pub async fn select(manager: &MihomoManager, group: &str, node: &str) -> anyhow::Result<()> {
    let api = super::running_api(manager).await?;
    let groups = api.get_proxies().await?.proxies;
    let members = groups
        .get(group)
        .and_then(|g| g.all.as_ref())
        .ok_or_else(|| anyhow::anyhow!("no proxy group named '{group}' (see `clash-verge-cli proxy list`)"))?;
    if !members.iter().any(|member| member == node) {
        anyhow::bail!("'{node}' is not a member of '{group}' (see `clash-verge-cli proxy list '{group}'`)");
    }
    api.select_proxy(group, node).await?;
    println!("{group} → {node}");
    Ok(())
}

pub async fn delay(manager: &MihomoManager, target: &str, url: &str, timeout_ms: u64) -> anyhow::Result<()> {
    let api = super::running_api(manager).await?;
    let groups = api.get_proxies().await?.proxies;
    let targets = if is_group(&groups, target) {
        let targets = leaf_targets(&groups, Some(target));
        if targets.is_empty() {
            anyhow::bail!("group '{target}' has no testable proxies");
        }
        targets
    } else if groups.contains_key(target) {
        vec![target.to_string()]
    } else {
        anyhow::bail!("no proxy or group named '{target}' (see `clash-verge-cli proxy list`)");
    };

    let results = delay_many(Arc::new(api), targets, url.to_string(), timeout_ms).await;
    let mut succeeded = 0;
    for (name, result) in &results {
        match result {
            Ok(delay) => {
                succeeded += 1;
                println!("{name}\t{delay} ms");
            }
            Err(error) => println!("{name}\tfailed: {error}"),
        }
    }
    if succeeded == 0 {
        anyhow::bail!("every delay test failed");
    }
    Ok(())
}

fn group_summaries(groups: &HashMap<String, ProxyGroup>) -> Vec<GroupSummary<'_>> {
    let mut summaries: Vec<GroupSummary<'_>> = groups
        .iter()
        .filter_map(|(name, group)| {
            group.all.as_ref().map(|members| GroupSummary {
                name,
                group_type: &group.group_type,
                now: group.now.as_deref(),
                members: members.len(),
            })
        })
        .collect();
    summaries.sort_by(|a, b| a.name.cmp(b.name));
    summaries
}

fn group_members<'a>(groups: &'a HashMap<String, ProxyGroup>, name: &str) -> anyhow::Result<Vec<Member<'a>>> {
    let group = groups
        .get(name)
        .filter(|group| group.all.is_some())
        .ok_or_else(|| anyhow::anyhow!("no proxy group named '{name}' (see `clash-verge-cli proxy list`)"))?;
    Ok(group
        .all
        .iter()
        .flatten()
        .map(|member| Member {
            name: member,
            selected: group.now.as_deref() == Some(member.as_str()),
            delay: last_delay(groups, member),
        })
        .collect())
}

fn format_delay(delay: Option<u64>) -> String {
    match delay {
        None => "-".into(),
        Some(0) => "timeout".into(),
        Some(ms) => format!("{ms} ms"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mihomo_api::types::DelayHistory;

    fn proxy(all: Option<&[&str]>, now: Option<&str>, delay: Option<u64>) -> ProxyGroup {
        ProxyGroup {
            group_type: if all.is_some() { "Selector" } else { "Shadowsocks" }.into(),
            now: now.map(str::to_string),
            all: all.map(|nodes| nodes.iter().map(|n| n.to_string()).collect()),
            history: delay.map(|delay| {
                vec![DelayHistory {
                    time: String::new(),
                    delay,
                }]
            }),
        }
    }

    fn groups() -> HashMap<String, ProxyGroup> {
        HashMap::from([
            ("Proxy".into(), proxy(Some(&["Tokyo", "Osaka"]), Some("Tokyo"), None)),
            ("Tokyo".into(), proxy(None, None, Some(42))),
            ("Osaka".into(), proxy(None, None, Some(0))),
        ])
    }

    #[test]
    fn summaries_list_only_groups_sorted_by_name() {
        let groups = groups();
        let summaries = group_summaries(&groups);
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].name, "Proxy");
        assert_eq!(summaries[0].now, Some("Tokyo"));
        assert_eq!(summaries[0].members, 2);
    }

    #[test]
    fn members_mark_the_selection_and_carry_the_last_delay() {
        let groups = groups();
        let members = group_members(&groups, "Proxy").unwrap();
        assert_eq!(members.len(), 2);
        assert!(members[0].selected && members[0].name == "Tokyo" && members[0].delay == Some(42));
        assert!(!members[1].selected && members[1].delay == Some(0));
        assert!(group_members(&groups, "Tokyo").is_err(), "a leaf is not a group");
        assert!(group_members(&groups, "Missing").is_err());
    }

    #[test]
    fn delays_format_timeouts_and_unknowns() {
        assert_eq!(format_delay(Some(42)), "42 ms");
        assert_eq!(format_delay(Some(0)), "timeout");
        assert_eq!(format_delay(None), "-");
    }
}
