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

#[cfg(test)]
#[allow(clippy::panic, clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;

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
