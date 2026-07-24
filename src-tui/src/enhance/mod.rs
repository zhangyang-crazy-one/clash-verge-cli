//! Thin enhance control-plane helpers ported from upstream clash-verge-rev.
//! Applied before writing / reloading the runtime clash config.

use serde_yaml_ng::{Mapping, Value};

/// App-owned top-level keys that must survive manual merge overrides.
const CONTROL_PLANE_KEYS: &[&str] = &[
    "external-controller",
    #[cfg(unix)]
    "external-controller-unix",
    #[cfg(windows)]
    "external-controller-pipe",
    "external-controller-cors",
    "secret",
    "mixed-port",
    "socks-port",
    "port",
    #[cfg(not(target_os = "windows"))]
    "redir-port",
    #[cfg(target_os = "linux")]
    "tproxy-port",
    "tun",
    "mode",
    "allow-lan",
    "log-level",
    "ipv6",
    "unified-delay",
];

/// Snapshot app-authoritative control-plane keys currently present in `config`.
pub fn snapshot_control_plane(config: &Mapping) -> Mapping {
    let mut snapshot = Mapping::new();
    for &key in CONTROL_PLANE_KEYS {
        let key = Value::from(key);
        if let Some(value) = config.get(&key) {
            snapshot.insert(key, value.clone());
        }
    }
    snapshot
}

/// Restore control-plane keys after a merge; missing snapshot keys are removed.
pub fn enforce_control_plane(mut config: Mapping, snapshot: Mapping) -> Mapping {
    for &key in CONTROL_PLANE_KEYS {
        let key = Value::from(key);
        if !snapshot.contains_key(&key) {
            config.remove(&key);
        }
    }
    config.extend(snapshot);
    config
}

fn is_loopback_bind_address(addr: &str) -> bool {
    let addr = addr.trim();
    let addr = addr
        .strip_prefix('[')
        .and_then(|addr| addr.strip_suffix(']'))
        .unwrap_or(addr);

    addr.eq_ignore_ascii_case("localhost")
        || addr.parse::<std::net::IpAddr>().is_ok_and(|addr| addr.is_loopback())
        || is_ipv4_shorthand_loopback(addr)
}

fn is_ipv4_shorthand_loopback(addr: &str) -> bool {
    let parts = addr.split('.').map(str::parse::<u32>).collect::<Result<Vec<_>, _>>();

    let Ok(parts) = parts else {
        return false;
    };

    match parts.as_slice() {
        [first, rest] => *first == 127 && *rest <= 0x00ff_ffff,
        [first, second, rest] => *first == 127 && *second <= 0xff && *rest <= 0xffff,
        [first, second, third, fourth] => {
            *first == 127 && *second <= 0xff && *third <= 0xff && *fourth <= 0xff
        }
        _ => false,
    }
}

/// When `allow-lan` is true and `bind-address` is loopback, widen to `*`.
pub fn ensure_lan_bind_address(mut config: Mapping) -> Mapping {
    let allow_lan = config.get("allow-lan").and_then(Value::as_bool).unwrap_or(false);

    if allow_lan
        && config
            .get("bind-address")
            .and_then(Value::as_str)
            .is_some_and(is_loopback_bind_address)
    {
        config.insert(Value::from("bind-address"), Value::from("*"));
    }

    config
}

/// Ensure `fake-ip-range6` when DNS is fake-ip + IPv6 enabled.
pub fn ensure_fake_ip_range6(dns: &mut Mapping) {
    let ipv6_enabled = dns.get("ipv6").and_then(|v| v.as_bool()).unwrap_or(false);
    let is_fake_ip = dns
        .get("enhanced-mode")
        .and_then(|v| v.as_str())
        .map(|m| m == "fake-ip")
        .unwrap_or(true);

    let range6_missing = dns
        .get("fake-ip-range6")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().is_empty())
        .unwrap_or(true);

    if ipv6_enabled && is_fake_ip && range6_missing {
        dns.insert(Value::from("fake-ip-range6"), Value::from("fdfe:dcba:9876::1/64"));
    }
}

/// Apply GUI TUN enable flag and ensure DNS fake-ip helpers when enabling.
pub fn use_tun(mut config: Mapping, enable: bool) -> Mapping {
    let tun_key = Value::from("tun");
    let mut tun_val = config
        .get(&tun_key)
        .and_then(Value::as_mapping)
        .cloned()
        .unwrap_or_default();

    tun_val.insert(Value::from("enable"), Value::from(enable));
    config.insert(tun_key, Value::Mapping(tun_val));

    if enable {
        let dns_key = Value::from("dns");
        let mut dns_val = config
            .get(&dns_key)
            .and_then(Value::as_mapping)
            .cloned()
            .unwrap_or_default();
        let ipv6_val = config.get("ipv6").and_then(|v| v.as_bool()).unwrap_or(false);

        let current_mode = dns_val
            .get(Value::from("enhanced-mode"))
            .and_then(|v| v.as_str())
            .unwrap_or("fake-ip");

        if current_mode == "fake-ip" || !dns_val.contains_key(Value::from("enhanced-mode")) {
            dns_val.insert(Value::from("enable"), Value::from(true));
            dns_val.insert(Value::from("ipv6"), Value::from(ipv6_val));
            if !dns_val.contains_key(Value::from("enhanced-mode")) {
                dns_val.insert(Value::from("enhanced-mode"), Value::from("fake-ip"));
            }
            if !dns_val.contains_key(Value::from("fake-ip-range")) {
                dns_val.insert(Value::from("fake-ip-range"), Value::from("198.18.0.1/16"));
            }
            ensure_fake_ip_range6(&mut dns_val);
            config.insert(dns_key, Value::Mapping(dns_val));
        }
    } else if let Some(Value::Mapping(dns)) = config.get_mut("dns") {
        ensure_fake_ip_range6(dns);
    }

    config
}

/// Apply thin enhance guards before writing runtime config.
///
/// Order mirrors upstream: TUN from GUI → snapshot control plane → restore after
/// optional merge (caller may merge between snapshot/enforce) → LAN bind → DNS v6.
pub fn prepare_runtime_config(mut config: Mapping, enable_tun: bool) -> Mapping {
    config = use_tun(config, enable_tun);
    let control_plane = snapshot_control_plane(&config);
    config = enforce_control_plane(config, control_plane);
    config = ensure_lan_bind_address(config);
    if let Some(Value::Mapping(dns)) = config.get_mut("dns") {
        ensure_fake_ip_range6(dns);
    }
    config
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mapping(yaml: &str) -> Mapping {
        serde_yaml_ng::from_str(yaml).expect("yaml")
    }

    #[test]
    fn lan_bind_address_loopback_is_widened() {
        for bind_address in [
            "localhost",
            "127.0.0.1",
            "127.0.0.2",
            "127.1",
            "::1",
            "[::1]",
            "0:0:0:0:0:0:0:1",
        ] {
            let result = ensure_lan_bind_address(mapping(&format!(
                r#"{{allow-lan: true, bind-address: "{bind_address}"}}"#
            )));
            assert_eq!(
                result.get("bind-address").and_then(Value::as_str),
                Some("*"),
                "bind-address {bind_address} should be widened"
            );
        }
    }

    #[test]
    fn lan_bind_address_preserves_custom_or_disabled() {
        let custom = ensure_lan_bind_address(mapping(r#"{allow-lan: true, bind-address: "192.168.1.2"}"#));
        assert_eq!(
            custom.get("bind-address").and_then(Value::as_str),
            Some("192.168.1.2")
        );

        let disabled = ensure_lan_bind_address(mapping(r#"{allow-lan: false, bind-address: "127.0.0.1"}"#));
        assert_eq!(
            disabled.get("bind-address").and_then(Value::as_str),
            Some("127.0.0.1")
        );
    }

    #[test]
    fn control_plane_survives_manual_overrides() {
        let app = mapping(
            r#"{mixed-port: 7897, secret: "s", tun: {enable: true}, mode: rule, allow-lan: false}"#,
        );
        let snapshot = snapshot_control_plane(&app);
        let mut hijacked = app;
        hijacked.insert(Value::from("mixed-port"), Value::from(1));
        hijacked.insert(Value::from("secret"), Value::from("hacked"));
        hijacked.insert(Value::from("extra"), Value::from("keep"));
        let result = enforce_control_plane(hijacked, snapshot);
        assert_eq!(result.get("mixed-port").and_then(Value::as_u64), Some(7897));
        assert_eq!(result.get("secret").and_then(Value::as_str), Some("s"));
        assert_eq!(result.get("extra").and_then(Value::as_str), Some("keep"));
        assert_eq!(
            result
                .get("tun")
                .and_then(Value::as_mapping)
                .and_then(|m| m.get("enable"))
                .and_then(Value::as_bool),
            Some(true)
        );
    }

    #[test]
    fn fake_ip_range6_added_when_needed() {
        let mut dns = mapping(r#"{ipv6: true, enhanced-mode: fake-ip}"#);
        ensure_fake_ip_range6(&mut dns);
        assert_eq!(
            dns.get("fake-ip-range6").and_then(Value::as_str),
            Some("fdfe:dcba:9876::1/64")
        );
    }

    #[test]
    fn use_tun_sets_enable_flag() {
        let config = use_tun(mapping(r#"{tun: {enable: false}, ipv6: true}"#), true);
        assert_eq!(
            config
                .get("tun")
                .and_then(Value::as_mapping)
                .and_then(|m| m.get("enable"))
                .and_then(Value::as_bool),
            Some(true)
        );
        assert!(
            config
                .get("dns")
                .and_then(Value::as_mapping)
                .and_then(|m| m.get("fake-ip-range6"))
                .is_some()
        );
    }
}
