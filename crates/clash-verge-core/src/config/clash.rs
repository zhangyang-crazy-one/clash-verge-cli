use crate::utils::{dirs, help};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_yaml_ng::{Mapping, Value};
use std::{
    borrow::Cow,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    str::FromStr as _,
};

const DEFAULT_EXTERNAL_CONTROLLER: &str = "127.0.0.1:9097";
const DEFAULT_MIXED_PORT: u16 = 7897;
const DEFAULT_SOCKS_PORT: u16 = 7898;
const DEFAULT_HTTP_PORT: u16 = 7899;
const DEFAULT_REDIR_PORT: u16 = 7895;
const DEFAULT_TPROXY_PORT: u16 = 7896;
const DEFAULT_TUN_STACK: &str = "gvisor";
const DEFAULT_TUN_DNS_HIJACK: &[&str] = &["any:53"];

#[derive(Default, Debug, Clone)]
pub struct IClashTemp(pub Mapping);

impl IClashTemp {
    pub async fn new() -> Self {
        Self::try_read().await.unwrap_or_else(|_| Self::template())
    }

    /// Fallible read: unlike [`Self::new`], a missing/unreadable/malformed
    /// config file is an error instead of silently returning the template
    /// (whose default ports may not match the running core).
    pub async fn try_read() -> anyhow::Result<Self> {
        let path = dirs::clash_path()?;
        let mut map = help::read_mapping(&path).await?;
        let template_map = Self::template().0;
        for (key, value) in template_map.into_iter() {
            if !map.contains_key(&key) {
                map.insert(key, value);
            }
        }

        // Ensure secret field is present and not empty
        if let Some(val) = map.get_mut("secret")
            && let Value::String(s) = val
            && s.is_empty()
        {
            *s = "set-your-secret".into();
        }
        Ok(Self(Self::guard(map)))
    }

    pub fn template() -> Self {
        let mut map = Mapping::new();
        let mut tun_config = Mapping::new();
        let mut cors_map = Mapping::new();

        tun_config.insert("enable".into(), false.into());
        tun_config.insert("stack".into(), DEFAULT_TUN_STACK.into());
        tun_config.insert("auto-route".into(), true.into());
        tun_config.insert("strict-route".into(), false.into());
        tun_config.insert("auto-detect-interface".into(), true.into());
        tun_config.insert("dns-hijack".into(), DEFAULT_TUN_DNS_HIJACK.into());

        #[cfg(not(target_os = "windows"))]
        map.insert("redir-port".into(), DEFAULT_REDIR_PORT.into());
        #[cfg(target_os = "linux")]
        map.insert("tproxy-port".into(), DEFAULT_TPROXY_PORT.into());

        map.insert("mixed-port".into(), DEFAULT_MIXED_PORT.into());
        map.insert("socks-port".into(), DEFAULT_SOCKS_PORT.into());
        map.insert("port".into(), DEFAULT_HTTP_PORT.into());
        map.insert("log-level".into(), "info".into());
        map.insert("allow-lan".into(), false.into());
        map.insert("ipv6".into(), true.into());
        map.insert("mode".into(), "rule".into());
        map.insert("external-controller".into(), DEFAULT_EXTERNAL_CONTROLLER.into());
        map.insert(
            "external-controller-unix".into(),
            crate::utils::dirs::standalone_socket_path()
                .to_string_lossy()
                .into_owned()
                .into(),
        );
        map.insert("tun".into(), tun_config.into());
        cors_map.insert("allow-private-network".into(), true.into());
        cors_map.insert(
            "allow-origins".into(),
            vec![
                "tauri://localhost",
                "http://tauri.localhost",
                "https://yacd.metacubex.one",
                "https://metacubex.github.io",
                "https://board.zash.run.place",
            ]
            .into(),
        );
        map.insert("secret".into(), "set-your-secret".into());
        map.insert("external-controller-cors".into(), cors_map.into());
        map.insert("unified-delay".into(), true.into());
        Self(map)
    }

    fn guard(mut config: Mapping) -> Mapping {
        #[cfg(not(target_os = "windows"))]
        let redir_port = Self::guard_redir_port(&config);
        #[cfg(target_os = "linux")]
        let tproxy_port = Self::guard_tproxy_port(&config);
        let mixed_port = Self::guard_mixed_port(&config);
        let socks_port = Self::guard_socks_port(&config);
        let port = Self::guard_port(&config);
        let ctrl = Self::guard_external_controller(&config);

        #[cfg(not(target_os = "windows"))]
        config.insert("redir-port".into(), redir_port.into());
        #[cfg(target_os = "linux")]
        config.insert("tproxy-port".into(), tproxy_port.into());
        config.insert("mixed-port".into(), mixed_port.into());
        config.insert("socks-port".into(), socks_port.into());
        config.insert("port".into(), port.into());
        config.insert("external-controller".into(), ctrl.into());

        config
    }

    pub fn patch_config(&mut self, patch: &Mapping) {
        for (key, value) in patch.iter() {
            self.0.insert(key.to_owned(), value.to_owned());
        }
    }

    pub async fn save_config(&self) -> Result<()> {
        help::save_yaml(&dirs::clash_path()?, &self.0, Some("# Generated by Clash Verge")).await
    }

    pub fn get_mixed_port(&self) -> u16 {
        Self::guard_mixed_port(&self.0)
    }

    #[allow(unused)]
    pub fn get_socks_port(&self) -> u16 {
        Self::guard_socks_port(&self.0)
    }

    #[allow(unused)]
    pub fn get_port(&self) -> u16 {
        Self::guard_port(&self.0)
    }

    pub fn get_client_info(&self) -> ClashInfo {
        let config = &self.0;

        ClashInfo {
            mixed_port: Self::guard_mixed_port(config),
            socks_port: Self::guard_socks_port(config),
            port: Self::guard_port(config),
            server: Self::guard_client_ctrl(config),
            secret: config.get("secret").and_then(|value| match value {
                Value::String(val_str) => Some(val_str.clone()),
                Value::Bool(val_bool) => Some(val_bool.to_string()),
                Value::Number(val_num) => Some(val_num.to_string()),
                _ => None,
            }),
        }
    }

    /// Tolerant read of the saved proxy mode (avoids strict BaseConfig parsing).
    pub fn get_mode(&self) -> Option<String> {
        self.0.get("mode").and_then(|value| match value {
            Value::String(val_str) => Some(val_str.clone()),
            _ => None,
        })
    }

    #[cfg(not(target_os = "windows"))]
    pub fn guard_redir_port(config: &Mapping) -> u16 {
        let mut port = config
            .get("redir-port")
            .and_then(|value| match value {
                Value::String(val_str) => val_str.parse().ok(),
                Value::Number(val_num) => val_num.as_u64().map(|u| u as u16),
                _ => None,
            })
            .unwrap_or(DEFAULT_REDIR_PORT);
        if port == 0 {
            port = DEFAULT_REDIR_PORT;
        }
        port
    }

    #[cfg(target_os = "linux")]
    pub fn guard_tproxy_port(config: &Mapping) -> u16 {
        let mut port = config
            .get("tproxy-port")
            .and_then(|value| match value {
                Value::String(val_str) => val_str.parse().ok(),
                Value::Number(val_num) => val_num.as_u64().map(|u| u as u16),
                _ => None,
            })
            .unwrap_or(DEFAULT_TPROXY_PORT);
        if port == 0 {
            port = DEFAULT_TPROXY_PORT;
        }
        port
    }

    pub fn guard_mixed_port(config: &Mapping) -> u16 {
        let raw_value = config.get("mixed-port");

        let mut port = raw_value
            .and_then(|value| match value {
                Value::String(val_str) => val_str.parse().ok(),
                Value::Number(val_num) => val_num.as_u64().map(|u| u as u16),
                _ => None,
            })
            .unwrap_or(DEFAULT_MIXED_PORT);

        if port == 0 {
            port = DEFAULT_MIXED_PORT;
        }

        port
    }

    pub fn guard_socks_port(config: &Mapping) -> u16 {
        let mut port = config
            .get("socks-port")
            .and_then(|value| match value {
                Value::String(val_str) => val_str.parse().ok(),
                Value::Number(val_num) => val_num.as_u64().map(|u| u as u16),
                _ => None,
            })
            .unwrap_or(DEFAULT_SOCKS_PORT);
        if port == 0 {
            port = DEFAULT_SOCKS_PORT;
        }
        port
    }

    pub fn guard_port(config: &Mapping) -> u16 {
        let mut port = config
            .get("port")
            .and_then(|value| match value {
                Value::String(val_str) => val_str.parse().ok(),
                Value::Number(val_num) => val_num.as_u64().map(|u| u as u16),
                _ => None,
            })
            .unwrap_or(DEFAULT_HTTP_PORT);
        if port == 0 {
            port = DEFAULT_HTTP_PORT;
        }
        port
    }

    pub fn guard_server_ctrl(config: &Mapping) -> String {
        config
            .get("external-controller")
            .and_then(|value| match value.as_str() {
                Some(val_str) => {
                    let val_str = val_str.trim();

                    let val = match val_str.starts_with(':') {
                        true => Cow::Owned(format!("127.0.0.1{val_str}")),
                        false => Cow::Borrowed(val_str),
                    };

                    SocketAddr::from_str(&val).ok().map(|s| s.to_string())
                }
                None => None,
            })
            .unwrap_or_else(|| DEFAULT_EXTERNAL_CONTROLLER.into())
    }

    pub fn guard_external_controller(config: &Mapping) -> String {
        Self::guard_server_ctrl(config)
    }

    pub fn guard_client_ctrl(config: &Mapping) -> String {
        let value = Self::guard_server_ctrl(config);
        match SocketAddr::from_str(value.as_str()) {
            Ok(mut socket) => {
                if socket.ip().is_unspecified() {
                    socket.set_ip(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)));
                }
                socket.to_string()
            }
            Err(_) => DEFAULT_EXTERNAL_CONTROLLER.into(),
        }
    }
}

#[derive(Default, Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct ClashInfo {
    /// clash core port
    pub mixed_port: u16,
    pub socks_port: u16,
    pub port: u16,
    /// same as `external-controller`
    pub server: String,
    /// clash secret
    pub secret: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_clash_info() {
        fn get_case<T: Into<Value>, D: Into<Value>>(mp: T, ec: D) -> ClashInfo {
            let mut map = Mapping::new();
            map.insert("mixed-port".into(), mp.into());
            map.insert("external-controller".into(), ec.into());

            IClashTemp(IClashTemp::guard(map)).get_client_info()
        }

        fn get_result<S: Into<String>>(port: u16, server: S) -> ClashInfo {
            ClashInfo {
                mixed_port: port,
                socks_port: DEFAULT_SOCKS_PORT,
                port: DEFAULT_HTTP_PORT,
                server: server.into(),
                secret: None,
            }
        }

        assert_eq!(
            IClashTemp(IClashTemp::guard(Mapping::new())).get_client_info(),
            get_result(DEFAULT_MIXED_PORT, DEFAULT_EXTERNAL_CONTROLLER)
        );

        assert_eq!(
            get_case("", ""),
            get_result(DEFAULT_MIXED_PORT, DEFAULT_EXTERNAL_CONTROLLER)
        );
        assert_eq!(get_case(65537, ""), get_result(1, DEFAULT_EXTERNAL_CONTROLLER));
        assert_eq!(get_case(8888, "127.0.0.1:8888"), get_result(8888, "127.0.0.1:8888"));
        assert_eq!(
            get_case(8888, "   :98888 "),
            get_result(8888, DEFAULT_EXTERNAL_CONTROLLER)
        );
        assert_eq!(get_case(8888, "0.0.0.0:8080  "), get_result(8888, "127.0.0.1:8080"));
        assert_eq!(get_case(8888, "0.0.0.0:8080"), get_result(8888, "127.0.0.1:8080"));
        assert_eq!(get_case(8888, "[::]:8080"), get_result(8888, "127.0.0.1:8080"));
        assert_eq!(get_case(8888, "192.168.1.1:8080"), get_result(8888, "192.168.1.1:8080"));
        assert_eq!(
            get_case(8888, "192.168.1.1:80800"),
            get_result(8888, DEFAULT_EXTERNAL_CONTROLLER)
        );
    }
}

#[derive(Default, Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub struct IClashExternalControllerCors {
    pub allow_origins: Option<Vec<String>>,
    pub allow_private_network: Option<bool>,
}

#[derive(Default, Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub struct IClash {
    pub mixed_port: Option<u16>,
    pub allow_lan: Option<bool>,
    pub log_level: Option<String>,
    pub ipv6: Option<bool>,
    pub mode: Option<String>,
    pub external_controller: Option<String>,
    pub secret: Option<String>,
    pub dns: Option<IClashDNS>,
    pub tun: Option<IClashTUN>,
    pub interface_name: Option<String>,
    pub external_controller_cors: Option<IClashExternalControllerCors>,
}

#[derive(Default, Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub struct IClashTUN {
    pub enable: Option<bool>,
    pub stack: Option<String>,
    pub auto_route: Option<bool>,
    pub auto_detect_interface: Option<bool>,
    pub dns_hijack: Option<Vec<String>>,
}

#[derive(Default, Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub struct IClashDNS {
    pub enable: Option<bool>,
    pub listen: Option<String>,
    pub default_nameserver: Option<Vec<String>>,
    pub enhanced_mode: Option<String>,
    pub fake_ip_range: Option<String>,
    pub fake_ip_range6: Option<String>,
    pub use_hosts: Option<bool>,
    pub fake_ip_filter: Option<Vec<String>>,
    pub nameserver: Option<Vec<String>>,
    pub fallback: Option<Vec<String>>,
    pub fallback_filter: Option<IClashFallbackFilter>,
    pub nameserver_policy: Option<Vec<String>>,
}

#[derive(Default, Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub struct IClashFallbackFilter {
    pub geoip: Option<bool>,
    pub geoip_code: Option<String>,
    pub ipcidr: Option<Vec<String>>,
    pub domain: Option<Vec<String>>,
}
