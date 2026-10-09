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
        [first, second, third, fourth] => *first == 127 && *second <= 0xff && *third <= 0xff && *fourth <= 0xff,
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

/// Apply the standalone CLI's own port settings from `verge.yaml` onto a
/// runtime config before it is written.
///
/// The CLI and the Clash Verge GUI share the same template defaults
/// (`mixed-port: 7897`, socks 7898, http 7899, redir 7895, tproxy 7896).
/// Running both, or leaving the GUI's root service enabled, makes them fight
/// over the same listeners and sends the losing side into a restart loop
/// (upstream #6741/#7861). Honouring the CLI's own `verge_*` port settings —
/// mirroring the GUI — lets the two coexist. Ports the user never set keep
/// whatever the runtime config already carries.
pub async fn apply_verge_ports(config: &mut Mapping) {
    let verge = clash_verge_core::config::IVerge::new().await;

    if let Some(port) = verge.verge_mixed_port {
        config.insert("mixed-port".into(), port.into());
    }

    if verge.verge_socks_enabled == Some(true)
        && let Some(port) = verge.verge_socks_port
    {
        config.insert("socks-port".into(), port.into());
    } else if verge.verge_socks_enabled == Some(false) {
        config.remove("socks-port");
    }

    if verge.verge_http_enabled == Some(true)
        && let Some(port) = verge.verge_port
    {
        config.insert("port".into(), port.into());
    } else if verge.verge_http_enabled == Some(false) {
        config.remove("port");
    }

    #[cfg(not(target_os = "windows"))]
    if verge.verge_redir_enabled == Some(true)
        && let Some(port) = verge.verge_redir_port
    {
        config.insert("redir-port".into(), port.into());
    } else if verge.verge_redir_enabled == Some(false) {
        config.remove("redir-port");
    }

    #[cfg(target_os = "linux")]
    if verge.verge_tproxy_enabled == Some(true)
        && let Some(port) = verge.verge_tproxy_port
    {
        config.insert("tproxy-port".into(), port.into());
    } else if verge.verge_tproxy_enabled == Some(false) {
        config.remove("tproxy-port");
    }
}

/// Effective mixed port the CLI will ask its core to bind: the CLI's own
/// `verge.yaml` override when set, else whatever the runtime config carries.
pub async fn effective_mixed_port() -> u16 {
    let verge = clash_verge_core::config::IVerge::new().await;
    if let Some(port) = verge.verge_mixed_port {
        return port;
    }
    clash_verge_core::config::IClashTemp::new().await.get_mixed_port()
}

/// The publicly-known secret shipped by the shared `config.yaml` template.
/// Anyone can read it, so it must never guard a live controller.
pub const PLACEHOLDER_CONTROLLER_SECRET: &str = "set-your-secret";

/// A fresh, unguessable controller secret (RFC 4122 v4 UUID).
pub fn generate_controller_secret() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Whether `secret` would leave the controller effectively unauthenticated.
pub fn is_placeholder_secret(secret: &str) -> bool {
    secret.trim().is_empty() || secret.trim() == PLACEHOLDER_CONTROLLER_SECRET
}

/// The last controller secret resolved in this process, shared with managers
/// that were built before the rotation happened.
static LAST_RESOLVED_SECRET: std::sync::OnceLock<std::sync::Mutex<Option<String>>> = std::sync::OnceLock::new();

fn last_resolved_slot() -> &'static std::sync::Mutex<Option<String>> {
    LAST_RESOLVED_SECRET.get_or_init(|| std::sync::Mutex::new(None))
}

/// Publish (and read back) the controller secret resolved in this process.
pub fn publish_resolved_secret(secret: &str) {
    if let Ok(mut slot) = last_resolved_slot().lock() {
        *slot = Some(secret.to_string());
    }
}

/// The controller secret resolved earlier in this process, if any.
pub fn last_resolved_secret() -> Option<String> {
    last_resolved_slot().lock().ok().and_then(|slot| slot.clone())
}

/// Atomically write `config.yaml` with owner-only permissions.
///
/// `help::save_yaml` mirrors the mode of a pre-existing file, so a config.yaml
/// created 0644 would keep a controller secret world-readable — and it
/// stages the content *before* the mode is known. This writer stages into a
/// unique `create_new` file that is already 0600, fsyncs it, and renames it
/// over the target, so the secret never exists on disk world-readable and
/// never exists half-written.
async fn save_clash_config_private(path: &std::path::Path, mapping: &Mapping) -> anyhow::Result<()> {
    use std::io::Write as _;

    let body = format!("# Generated by Clash Verge\n\n{}", serde_yaml_ng::to_string(mapping)?);
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("{} has no parent directory", path.display()))?;
    tokio::fs::create_dir_all(parent)
        .await
        .map_err(|error| anyhow::anyhow!("cannot create {}: {error}", parent.display()))?;
    let staging_id = uuid::Uuid::new_v4();
    let temporary = path.with_file_name(format!(
        "{}.{staging_id}.tmp",
        path.file_name().and_then(|name| name.to_str()).unwrap_or("config.yaml")
    ));

    let write = async {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut file = options
            .open(&temporary)
            .map_err(|error| anyhow::anyhow!("cannot stage {}: {error}", temporary.display()))?;
        file.write_all(body.as_bytes())
            .map_err(|error| anyhow::anyhow!("cannot write {}: {error}", temporary.display()))?;
        file.sync_all()
            .map_err(|error| anyhow::anyhow!("cannot sync {}: {error}", temporary.display()))?;
        drop(file);
        tokio::fs::rename(&temporary, path)
            .await
            .map_err(|error| anyhow::anyhow!("cannot replace {}: {error}", path.display()))
    }
    .await;
    if write.is_err() {
        let _ = tokio::fs::remove_file(&temporary).await;
    }
    write
}

/// Restrict an existing `config.yaml` to owner-only when it is not already.
///
/// Called even when the stored secret is strong: the file also carries the
/// controller address, and a world-readable one widens what a local user can
/// reach.
#[cfg(unix)]
fn tighten_clash_config_mode(path: &std::path::Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    let metadata = std::fs::metadata(path)?;
    if metadata.permissions().mode() & 0o077 == 0 {
        return Ok(());
    }
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(|error| {
        anyhow::anyhow!(
            "cannot restrict {} to mode 600: {error}; fix its permissions by hand",
            path.display()
        )
    })
}

#[cfg(not(unix))]
fn tighten_clash_config_mode(_path: &std::path::Path) -> anyhow::Result<()> {
    Ok(())
}

/// Resolve the effective controller secret, rotating a weak one and
/// persisting it back into `config.yaml` (mode 600) before returning.
///
/// Fail-closed contract:
/// - A missing `config.yaml` (fresh install) is composed from the template and
///   saved, so the CLI's own defaults exist before anything binds a port.
/// - A **malformed** `config.yaml` is an error: the file is never silently
///   overwritten by the template (which would discard the user's ports,
///   proxies and rules) and no core is started on top of a guess.
/// - An empty or `set-your-secret` secret is replaced with a random one and
///   persisted, so mihomo and the generated sing-box `clash_api` — which share
///   this secret — both stop accepting a publicly-known value.
/// - Persisting happens here (before any core starts), so a later CLI
///   invocation authenticates against the same value.
/// - A save failure is an error; we never silently fall back to the weak
///   secret.
pub async fn resolve_controller_secret() -> anyhow::Result<String> {
    let path = clash_verge_core::utils::dirs::clash_path()
        .map_err(|error| anyhow::anyhow!("cannot locate the clash config to store the controller secret: {error}"))?;
    let read = clash_verge_core::config::IClashTemp::try_read().await;
    let mut clash = match read {
        Ok(clash) => clash,
        Err(error) if is_missing_clash_config(&path, &error) => {
            // Fresh install only: nothing to lose, so compose the template.
            let mut clash = clash_verge_core::config::IClashTemp::template();
            let secret = generate_controller_secret();
            clash.0.insert(Value::from("secret"), Value::from(secret.clone()));
            save_clash_config_private(&path, &clash.0).await.map_err(|error| {
                anyhow::anyhow!(
                    "failed to compose {} with a generated controller secret: {error}",
                    path.display()
                )
            })?;
            publish_resolved_secret(&secret);
            return Ok(secret);
        }
        Err(error) => {
            return Err(anyhow::anyhow!(
                "cannot read {}: {error}. Fix or remove it — it was left untouched and no core was started.",
                path.display()
            ));
        }
    };

    let current = clash.get_client_info().secret.unwrap_or_default();
    if !is_placeholder_secret(&current) {
        // A strong secret still lives in a file that must stay private.
        tighten_clash_config_mode(&path)?;
        publish_resolved_secret(&current);
        return Ok(current);
    }

    let secret = generate_controller_secret();
    clash.0.insert(Value::from("secret"), Value::from(secret.clone()));
    save_clash_config_private(&path, &clash.0).await.map_err(|error| {
        anyhow::anyhow!(
            "failed to persist the generated controller secret to {}: {error}",
            path.display()
        )
    })?;
    publish_resolved_secret(&secret);
    Ok(secret)
}

/// Whether a `config.yaml` read failed because the file is absent (fresh
/// install) rather than because it is unreadable or malformed.
fn is_missing_clash_config(path: &std::path::Path, error: &anyhow::Error) -> bool {
    std::fs::metadata(path).is_err() || error.to_string().contains("file not found")
}

/// Fail fast when the mixed port is already bound by another process.
///
/// The CLI and the Clash Verge GUI share the same template defaults, and the
/// GUI's root service retries forever when it loses the race (upstream
/// #6741/#7861). Spawning a core that cannot bind makes both sides flap and
/// briefly hijacks the system proxy, so report the conflict with the exact
/// knob to change instead. Calls after the CLI has stopped its own tracked
/// core, so the only remaining holder is a foreign process.
pub async fn ensure_mixed_port_available() -> Result<(), String> {
    let port = effective_mixed_port().await;
    if port == 0 {
        return Ok(());
    }
    match std::net::TcpListener::bind(("127.0.0.1", port)) {
        Ok(listener) => {
            drop(listener);
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => {
            let verge_path = clash_verge_core::utils::dirs::verge_path()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|_| "verge.yaml".into());
            Err(format!(
                "mixed-port {port} is already in use by another process — often the Clash Verge \
                 GUI's service (its core also defaults to 7897). Stop the other instance, or set \
                 `verge_mixed_port` to a free port in {verge_path}."
            ))
        }
        Err(error) => Err(format!("cannot probe mixed-port {port}: {error}")),
    }
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
        assert_eq!(custom.get("bind-address").and_then(Value::as_str), Some("192.168.1.2"));

        let disabled = ensure_lan_bind_address(mapping(r#"{allow-lan: false, bind-address: "127.0.0.1"}"#));
        assert_eq!(disabled.get("bind-address").and_then(Value::as_str), Some("127.0.0.1"));
    }

    #[test]
    fn control_plane_survives_manual_overrides() {
        let app = mapping(r#"{mixed-port: 7897, secret: "s", tun: {enable: true}, mode: rule, allow-lan: false}"#);
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
    fn profile_reload_drops_stale_non_control_plane_keys() {
        // Simulate rebuild: start from a providers-only profile, keep only control plane
        // from a previous proxies-based runtime.
        let previous_runtime = mapping(
            r#"{
mixed-port: 7897,
secret: "s",
mode: rule,
proxies: [{name: old, type: ss}],
proxy-groups: [{name: PROXY, type: select, proxies: [old]}],
rules: [MATCH,PROXY]
}"#,
        );
        let new_profile = mapping(
            r#"{
proxy-providers: {sub: {type: http, url: https://example.com/p.yaml}},
proxy-groups: [{name: PROXY, type: select, use: [sub]}],
rules: [MATCH,PROXY]
}"#,
        );
        let snapshot = snapshot_control_plane(&previous_runtime);
        let result = enforce_control_plane(new_profile, snapshot);
        assert!(result.get("proxies").is_none(), "stale proxies must not linger");
        assert!(result.get("proxy-providers").is_some());
        assert_eq!(result.get("mixed-port").and_then(Value::as_u64), Some(7897));
        assert_eq!(result.get("secret").and_then(Value::as_str), Some("s"));
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
    fn use_tun_preserves_an_explicit_fake_ip_range6() {
        let config = use_tun(
            mapping(
                r#"{ipv6: true, tun: {enable: false}, dns: {ipv6: true, enhanced-mode: fake-ip, fake-ip-range6: "fd12:3456:789a::1/64"}}"#,
            ),
            true,
        );
        assert_eq!(
            config
                .get("dns")
                .and_then(Value::as_mapping)
                .and_then(|dns| dns.get("fake-ip-range6"))
                .and_then(Value::as_str),
            Some("fd12:3456:789a::1/64"),
            "an explicit profile IPv6 fake-IP range must survive TUN enhancement"
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

    #[tokio::test]
    async fn apply_verge_ports_modifies_configured_ports() {
        let mut cfg = mapping("mixed-port: 7890\n");
        apply_verge_ports(&mut cfg).await;
        // If verge.yaml has a verge_mixed_port, it overrides; otherwise it retains 7890.
        assert!(cfg.contains_key("mixed-port"));
    }
    /// P1-E: a present-but-malformed `config.yaml` must be reported, never
    /// silently replaced by the template (which would discard the user's
    /// ports, proxies and rules behind a rotated secret).
    #[tokio::test]
    async fn a_malformed_config_yaml_is_an_error_and_is_never_overwritten() {
        let home = tempfile::tempdir().expect("tempdir");
        let _guard = crate::profile_store::store::tests::claim_test_app_home(home.path().to_path_buf()).await;
        let path = home.path().join("config.yaml");
        let broken = "mixed-port: [7897\nsecret: mine\n";
        std::fs::write(&path, broken).expect("seed");

        let error = resolve_controller_secret()
            .await
            .expect_err("a malformed config must not be replaced by the template");
        assert!(error.to_string().contains("config.yaml"), "{error}");
        assert_eq!(
            std::fs::read_to_string(&path).expect("reread"),
            broken,
            "the user's file must be left exactly as it was"
        );
    }

    /// P1-E/P2: the rotated secret lands in a 0600 file even when the
    /// pre-existing config.yaml was world-readable — the staging file is
    /// already private, so the secret never exists world-readable.
    #[tokio::test]
    async fn a_rotated_secret_lands_in_a_private_file_and_tightens_a_loose_one() {
        let home = tempfile::tempdir().expect("tempdir");
        let _guard = crate::profile_store::store::tests::claim_test_app_home(home.path().to_path_buf()).await;
        let path = home.path().join("config.yaml");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::write(&path, "mixed-port: 35123\nsecret: set-your-secret\n").expect("seed");
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).expect("loosen");
        }
        #[cfg(not(unix))]
        std::fs::write(&path, "mixed-port: 35123\nsecret: set-your-secret\n").expect("seed");

        let secret = resolve_controller_secret().await.expect("rotate");
        assert_ne!(secret, PLACEHOLDER_CONTROLLER_SECRET);
        assert!(std::fs::read_to_string(&path).expect("reread").contains(&secret));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&path).expect("stat").permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "a rotated secret must not be world-readable: {mode:o}");
            // No staging file survives.
            let leftovers: Vec<_> = std::fs::read_dir(home.path())
                .expect("list")
                .filter_map(|entry| entry.ok())
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .filter(|name| name.starts_with('.') && name.ends_with(".tmp"))
                .collect();
            assert!(leftovers.is_empty(), "staging files left behind: {leftovers:?}");
        }

        // A strong secret is still corrected when the file is loose.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).expect("loosen again");
            assert_eq!(resolve_controller_secret().await.expect("strong"), secret);
            let mode = std::fs::metadata(&path).expect("stat").permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "a stored secret must not be left world-readable: {mode:o}");
        }
    }
}
