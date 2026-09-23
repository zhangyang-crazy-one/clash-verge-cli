use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MihomoVersion {
    pub version: String,
}

/// Top-level mihomo proxies response: GET /proxies
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyData {
    pub proxies: HashMap<String, ProxyGroup>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyGroup {
    #[serde(rename = "type")]
    pub group_type: String,
    pub now: Option<String>,
    pub all: Option<Vec<String>>,
    pub history: Option<Vec<DelayHistory>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DelayHistory {
    pub time: String,
    pub delay: u64,
}

/// Delay test result for a single node.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyDelay {
    pub delay: u64,
}

/// Select proxy request body: PUT /proxies/:group
#[derive(Debug, Clone, Serialize)]
pub struct SelectProxyRequest {
    pub name: String,
}

/// Traffic data: GET /traffic
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrafficData {
    pub up: u64,
    pub down: u64,
}

/// Connections data: GET /connections
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectionsData {
    /// mihomo sends `null` rather than `[]` when nothing is connected.
    #[serde(default, deserialize_with = "null_as_empty")]
    pub connections: Vec<ConnectionInfo>,
}

fn null_as_empty<'de, D, T>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Ok(Option::<Vec<T>>::deserialize(deserializer)?.unwrap_or_default())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectionInfo {
    pub id: String,
    pub metadata: Option<ConnectionMeta>,
    pub upload: u64,
    pub download: u64,
    pub start: String,
    pub rule: Option<String>,
    pub chains: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectionMeta {
    pub host: Option<String>,
    pub network: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogEntry {
    #[serde(rename = "type")]
    pub level: String,
    pub payload: String,
}

/// A single rule from `GET /rules`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Rule {
    #[serde(rename = "type")]
    pub rule_type: String,
    pub payload: String,
    pub proxy: String,
    #[serde(default)]
    pub size: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RulesResponse {
    pub rules: Vec<Rule>,
}

/// A rule provider from `GET /providers/rules`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuleProvider {
    pub name: String,
    pub behavior: String,
    #[serde(rename = "ruleCount")]
    pub rule_count: u64,
    #[serde(rename = "vehicleType")]
    pub vehicle_type: String,
    #[serde(default)]
    pub updated_at: Option<String>,
    #[serde(rename = "type")]
    pub provider_type: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuleProvidersResponse {
    pub providers: std::collections::HashMap<String, RuleProvider>,
}

#[cfg(test)]
#[allow(clippy::panic, clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn connections_accept_null_and_missing_lists() {
        for body in [
            r#"{"downloadTotal":0,"uploadTotal":0,"connections":null}"#,
            r#"{"downloadTotal":0,"uploadTotal":0}"#,
        ] {
            let data: ConnectionsData = serde_json::from_str(body).unwrap();
            assert!(data.connections.is_empty(), "{body}");
        }
    }

    #[test]
    fn test_version_deserialize() {
        let raw = r#"{"version":"Mihomo Meta v1.19.29"}"#;
        let v: MihomoVersion = match serde_json::from_str(raw) {
            Ok(v) => v,
            Err(e) => panic!("parse failed: {e}"),
        };
        assert_eq!(v.version, "Mihomo Meta v1.19.29");
    }
}
