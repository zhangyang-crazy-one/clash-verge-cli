use serde::{Deserialize, Deserializer, Serialize};
use std::collections::HashMap;

/// Deserialize an explicitly-`null` JSON value as the type's `Default`.
///
/// mihomo returns `"connections": null` / `"rules": null` (and may omit the
/// keys entirely) when the lists are empty, which a plain `Vec<T>` field
/// rejects with `invalid type: null, expected a sequence`. Wrapping the target
/// type in `Option` first lets serde accept `null`, then we fall back to the
/// default (an empty `Vec`). Pair with `#[serde(default)]` so a missing key
/// also yields the default; the public field stays `Vec<T>`, so consumers are
/// unchanged.
fn null_to_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

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
    #[serde(default, deserialize_with = "null_to_default")]
    pub connections: Vec<ConnectionInfo>,
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
    #[serde(default, deserialize_with = "null_to_default")]
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

    fn parse_connections(raw: &str) -> ConnectionsData {
        match serde_json::from_str(raw) {
            Ok(d) => d,
            Err(e) => panic!("connections parse failed: {e}"),
        }
    }

    fn parse_rules(raw: &str) -> RulesResponse {
        match serde_json::from_str(raw) {
            Ok(d) => d,
            Err(e) => panic!("rules parse failed: {e}"),
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

    #[test]
    fn test_connections_null_parses_to_empty() {
        let data = parse_connections(r#"{"connections": null}"#);
        assert!(data.connections.is_empty());
    }

    #[test]
    fn test_connections_missing_key_parses_to_empty() {
        let data = parse_connections(r#"{}"#);
        assert!(data.connections.is_empty());
    }

    #[test]
    fn test_connections_normal_payload_parses() {
        let raw = r#"{"connections":[{"id":"1","metadata":null,"upload":100,"download":200,"start":"2025-01-01T00:00:00Z","rule":null,"chains":null}]}"#;
        let data = parse_connections(raw);
        assert_eq!(data.connections.len(), 1);
        assert_eq!(data.connections[0].id, "1");
        assert_eq!(data.connections[0].upload, 100);
        assert_eq!(data.connections[0].download, 200);
    }

    #[test]
    fn test_rules_null_parses_to_empty() {
        let resp = parse_rules(r#"{"rules": null}"#);
        assert!(resp.rules.is_empty());
    }

    #[test]
    fn test_rules_missing_key_parses_to_empty() {
        let resp = parse_rules(r#"{}"#);
        assert!(resp.rules.is_empty());
    }

    #[test]
    fn test_rules_normal_payload_parses() {
        let raw = r#"{"rules":[{"type":"DOMAIN-SUFFIX","payload":"example.com","proxy":"DIRECT","size":0}]}"#;
        let resp = parse_rules(raw);
        assert_eq!(resp.rules.len(), 1);
        assert_eq!(resp.rules[0].rule_type, "DOMAIN-SUFFIX");
        assert_eq!(resp.rules[0].payload, "example.com");
        assert_eq!(resp.rules[0].proxy, "DIRECT");
    }
}
