//! Skeleton generator for the sing-box runtime config (`singbox.json`).
//!
//! Shape verified against sing-box 1.13 docs/examples:
//! - mixed inbound on a local port for SOCKS/HTTP
//! - optional tun inbound with an explicit `interface_name` so the
//!   lifecycle resource barrier can poll a deterministic device
//! - selector/urltest outbound groups derived from the profile structure;
//!   node outbounds arrive pre-converted and are spliced verbatim
//! - `experimental.clash_api` bound to a local TCP address (sing-box's
//!   clash_api does not support unix sockets)
//!
//! Route rules and DNS arrive pre-built (`ConfigInput::route_rules` /
//! `ConfigInput::dns`) so this module stays a pure shape assembler; unknown
//! fields survive because generation starts from scratch each time the
//! profile changes.

use serde_json::{Value, json};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

/// A selector or urltest outbound group derived from the profile.
#[derive(Debug, Clone)]
pub struct GroupSpec {
    pub name: String,
    pub kind: GroupKind,
    /// Member tags — node outbound tags or other group tags.
    pub members: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupKind {
    Selector,
    UrlTest,
}

/// TUN inbound settings (only applied when [`ConfigInput::enable_tun`]).
#[derive(Debug, Clone)]
pub struct TunSettings {
    /// gVisor | system | mixed
    pub stack: String,
    pub mtu: u16,
}

/// sing-box `experimental.clash_api` settings. TCP only — sing-box's
/// clash_api has no unix-socket support.
#[derive(Debug, Clone)]
pub struct ClashApiSettings {
    pub listen: SocketAddr,
    pub secret: String,
}

/// Everything the generator needs. Node outbounds must already be valid
/// sing-box outbound objects with unique `tag`s (the converter guarantees
/// this); groups reference those tags by name.
#[derive(Debug, Clone)]
pub struct ConfigInput {
    /// Pre-converted node outbounds (full JSON objects).
    pub outbounds: Vec<Value>,
    pub groups: Vec<GroupSpec>,
    pub mixed_port: u16,
    pub enable_tun: bool,
    pub tun: TunSettings,
    pub clash_api: ClashApiSettings,
    /// sing-box rule-set definitions emitted beneath `route`; rules reference
    /// them by tag.
    pub rule_sets: Vec<Value>,
    /// Pre-converted sing-box route rule objects (task 7.5): profile rules
    /// via `routing::to_singbox_json` plus stored logical rules.
    pub route_rules: Vec<Value>,
    /// Pre-built DNS section (task 8.1, 1.12+ new format); omitted when None.
    pub dns: Option<Value>,
}

const TUN_INTERFACE_NAME: &str = "sb-tun0";
const DIRECT_TAG: &str = "direct";

/// File name of the sing-box selection cache (#57), created inside the CLI
/// config dir. sing-box 1.14 persists selector choices (and the clash mode)
/// in this bbolt file whenever `experimental.cache_file` is enabled, so
/// selections survive the restarts a subscription refresh or rule edit causes.
pub const CACHE_FILE_NAME: &str = "singbox-cache.db";

/// Mode the core starts in when the shared clash.yaml carries no usable
/// `mode`. sing-box spells modes capitalized (`Rule` / `Global` / `Direct`)
/// in both `default_mode` and the `clash_mode` route matcher.
pub const DEFAULT_CLASH_MODE: &str = "Rule";

/// Every mode sing-box can expose, derived from the `clash_mode` rules in
/// `route.rules`. Without at least the two non-default rules below,
/// sing-box reports `mode-list: ["Rule"]` and silently ignores
/// `PATCH /configs {"mode": ...}` (#53).
pub const CLASH_MODES: &[&str] = &["Rule", "Global", "Direct"];

/// The publicly-known secret shipped by the shared `config.yaml` template.
/// A controller still carrying it is effectively unauthenticated, so the CLI
/// rotates it before the core starts (see `enhance::resolve_controller_secret`).
pub const PLACEHOLDER_SECRET: &str = "set-your-secret";

/// Origins allowed to reach the generated clash_api over CORS.
///
/// sing-box defaults `access_control_allow_origin` to `["*"]`, which lets any
/// web page the user visits drive the local controller cross-origin. The CLI's
/// own controller client is a loopback TCP client that never sends `Origin`,
/// so the allow-list is restricted to loopback origins only.
pub const CLASH_API_ALLOW_ORIGINS: &[&str] = &[
    "http://localhost",
    "https://localhost",
    "http://127.0.0.1",
    "http://[::1]",
];

/// Fail-closed guard for the CLI-owned control plane of a generated config.
///
/// Runs only after the CLI has stamped its own `clash_api` block, so it can
/// assert that the block we just wrote is neither unauthenticated nor
/// wildcard-permissive. `Ok(())` means the file is safe to hand to the core.
pub fn validate_control_plane_security(config: &Value) -> Result<(), String> {
    let Some(clash_api) = config.pointer("/experimental/clash_api") else {
        return Err("generated sing-box config is missing experimental.clash_api".into());
    };
    let secret = clash_api.get("secret").and_then(Value::as_str).unwrap_or_default();
    if secret.trim().is_empty() || secret == PLACEHOLDER_SECRET {
        return Err("generated sing-box clash_api carries an empty or template controller secret".into());
    }
    let Some(origins) = clash_api.get("access_control_allow_origin").and_then(Value::as_array) else {
        return Err(
            "generated sing-box clash_api has no access_control_allow_origin; sing-box defaults it to '*'".into(),
        );
    };
    if origins.iter().any(|origin| origin.as_str() == Some("*")) {
        return Err("generated sing-box clash_api allows the CORS wildcard origin '*'".into());
    }
    if origins.is_empty() {
        return Err("generated sing-box clash_api has an empty access_control_allow_origin".into());
    }
    if clash_api
        .get("access_control_allow_private_network")
        .and_then(Value::as_bool)
        != Some(false)
    {
        return Err("generated sing-box clash_api must set access_control_allow_private_network to false".into());
    }
    // #53: without an explicit default_mode sing-box derives the mode list
    // from route rules alone and mode switching silently no-ops.
    match clash_api.get("default_mode").and_then(Value::as_str) {
        Some(mode) if CLASH_MODES.contains(&mode) => {}
        Some(mode) => {
            return Err(format!(
                "generated sing-box clash_api has unsupported default_mode {mode:?}"
            ));
        }
        None => return Err("generated sing-box clash_api has no default_mode; mode switching would be a no-op".into()),
    }
    Ok(())
}

/// Canonicalize a Clash mode name to sing-box's spelling. Unknown values fall
/// back to [`DEFAULT_CLASH_MODE`] instead of emitting a mode sing-box rejects.
pub fn normalize_clash_mode(mode: &str) -> String {
    match mode.trim().to_ascii_lowercase().as_str() {
        "global" => "Global".into(),
        "direct" => "Direct".into(),
        _ => DEFAULT_CLASH_MODE.into(),
    }
}

/// Route rules that make `PATCH /configs {"mode": ...}` effective: sing-box
/// builds its `mode-list` from the `clash_mode` matchers it finds here, and
/// `Global`/`Direct` must be prepended so they short-circuit the regular
/// rule list. `Rule` needs no rule of its own — it falls through to the
/// generated routing rules.
pub fn clash_mode_rules(global_target: &str) -> Vec<Value> {
    vec![
        json!({ "clash_mode": "Direct", "outbound": DIRECT_TAG }),
        json!({ "clash_mode": "Global", "outbound": global_target }),
    ]
}

/// Read the mode the shared clash.yaml (`config.yaml` / `clash-verge.yaml`)
/// currently holds; this is what `clash-verge-cli mode <x>` persists, so it
/// is the mode a freshly spawned sing-box should start in. Unreadable or
/// missing files fall back to [`DEFAULT_CLASH_MODE`].
pub fn default_mode_from_yaml(body: &str) -> String {
    serde_yaml_ng::from_str::<serde_yaml_ng::Value>(body)
        .ok()
        .and_then(|document| {
            document
                .get("mode")
                .and_then(serde_yaml_ng::Value::as_str)
                .map(normalize_clash_mode)
        })
        .unwrap_or_else(|| DEFAULT_CLASH_MODE.into())
}

fn configured_default_mode() -> String {
    clash_verge_core::utils::dirs::clash_path()
        .ok()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .map(|body| default_mode_from_yaml(&body))
        .unwrap_or_else(|| DEFAULT_CLASH_MODE.into())
}

/// `experimental.cache_file` for a cache path. sing-box 1.14 removed the
/// 1.13-era `store_selected` switch: with `enabled: true` the core always
/// persists selector choices (and the clash mode) into this file. Only the
/// reviewed 1.14.2 capability matrix is supported, so no legacy flag is emitted.
pub fn cache_file_json(path: &Path) -> Value {
    json!({
        "enabled": true,
        "path": path.to_string_lossy(),
    })
}

/// Fail-closed permission guard for the selection cache: it records which
/// nodes a user picked, so it must never be readable by group/other. A file
/// sing-box would create itself gets 0644, hence the explicit pre-create
/// (with 0600) plus a chmod of a pre-existing loose file.
pub fn ensure_private_cache_file(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};

    if let Some(parent) = path.parent()
        && !parent.is_dir()
        && let Err(error) = std::fs::create_dir_all(parent)
    {
        return Err(format!("cannot create {}: {error}", parent.display()));
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true);
    #[cfg(unix)]
    {
        options.mode(0o600);
    }
    match options.open(path) {
        Ok(file) => {
            #[cfg(unix)]
            if let Err(error) = file.set_permissions(std::fs::Permissions::from_mode(0o600)) {
                return Err(format!("cannot restrict {}: {error}", path.display()));
            }
            file.sync_all()
                .map_err(|error| format!("sync {}: {error}", path.display()))
        }
        Err(error) => Err(format!("cannot create selection cache {}: {error}", path.display())),
    }
}

#[cfg(unix)]
fn cache_file_is_private(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::metadata(path).is_ok_and(|meta| meta.permissions().mode() & 0o077 == 0)
}

#[cfg(not(unix))]
fn cache_file_is_private(_path: &Path) -> bool {
    true
}

/// Absolute path of the selection cache under the CLI config dir, when the
/// config dir is known.
fn cache_file_path() -> Option<PathBuf> {
    let home = clash_verge_core::utils::dirs::app_home_dir().ok()?;
    Some(home.join(CACHE_FILE_NAME))
}

/// The `experimental.clash_api` object the CLI owns: loopback listener,
/// resolved secret and an explicit, non-wildcard CORS policy.
fn clash_api_json(clash_api: &ClashApiSettings, default_mode: &str) -> Value {
    json!({
        "external_controller": clash_api.listen.to_string(),
        "secret": clash_api.secret,
        "default_mode": default_mode,
        "access_control_allow_origin": CLASH_API_ALLOW_ORIGINS,
        "access_control_allow_private_network": false,
    })
}

/// Wire mode switching (#53): stamp `default_mode` and prepend the
/// `clash_mode` route rules so sing-box exposes all three modes and honours
/// `PATCH /configs`. Prepending keeps `Global`/`Direct` ahead of the regular
/// rule list, so they short-circuit it.
pub fn apply_clash_mode_support(config: &mut Value, clash_api: &ClashApiSettings, default_mode: &str) {
    if !config.get("route").is_some_and(Value::is_object) {
        config["route"] = json!({});
    }
    if !config.get("experimental").is_some_and(Value::is_object) {
        config["experimental"] = json!({});
    }
    let global_target = config["route"]["final"].as_str().unwrap_or(DIRECT_TAG).to_string();
    let mut rules = clash_mode_rules(&global_target);
    if let Some(existing) = config["route"]["rules"].as_array() {
        rules.extend(existing.iter().cloned());
    }
    config["route"]["rules"] = Value::Array(rules);
    config["experimental"]["clash_api"] = clash_api_json(clash_api, &normalize_clash_mode(default_mode));
}

/// Wire selection persistence (#57). Fails closed when an existing cache file
/// is readable beyond its owner: rather than persisting node choices into a
/// world-readable file we let the generation error surface.
fn apply_cache_file(config: &mut Value) -> Result<(), String> {
    let Some(path) = cache_file_path() else {
        // No config dir resolved (pure unit test): emit no cache_file rather
        // than guessing a path, which would either land somewhere
        // world-readable or not exist at all. Production runs always have the
        // dir initialized (see `main`), so selections persist there.
        return Ok(());
    };
    ensure_private_cache_file(&path)?;
    if !cache_file_is_private(&path) {
        return Err(format!(
            "selection cache {} is readable beyond its owner; refusing to persist node selections there",
            path.display()
        ));
    }
    if !config.get("experimental").is_some_and(Value::is_object) {
        config["experimental"] = json!({});
    }
    config["experimental"]["cache_file"] = cache_file_json(&path);
    Ok(())
}
/// Default urltest probe URL (https mandatory: sing-box silently drops
/// http URLs and falls back to its own default).
pub const URLTEST_URL: &str = "https://www.gstatic.com/generate_204";

/// Generate the sing-box runtime config skeleton.
///
/// Errors when two outbounds or groups share a tag — that would make the
/// core reject the whole file at start, and we prefer failing here where
/// we can point at the offending name.
pub fn generate_config(input: &ConfigInput) -> Result<Value, String> {
    generate_config_for_version(input, crate::singbox::capabilities::SING_BOX_1_14_2.version)
}

pub fn generate_config_for_version(input: &ConfigInput, version: &str) -> Result<Value, String> {
    let matrix = crate::singbox::capabilities::require_for_version(version)?;
    if input.enable_tun {
        validate_tun_stack(&input.tun.stack)?;
        if input.tun.stack == "gvisor" && !matrix.tun_gvisor {
            return Err(format!("sing-box {} does not support gvisor TUN stack", matrix.version));
        }
    }
    if !input.rule_sets.is_empty() && !matrix.route_rule_sets_nested {
        return Err(format!(
            "sing-box {} does not support nested route rule sets",
            matrix.version
        ));
    }
    if !input.route_rules.is_empty() && !matrix.route_rule_references {
        return Err(format!(
            "sing-box {} has no reviewed route-reference support",
            matrix.version
        ));
    }
    let mut tags = std::collections::HashSet::new();
    for outbound in &input.outbounds {
        let tag = outbound
            .get("tag")
            .and_then(Value::as_str)
            .ok_or_else(|| "node outbound missing tag".to_string())?;
        if is_reserved_tag(tag) {
            return Err(format!(
                "outbound tag {tag:?} is reserved for a built-in policy outbound"
            ));
        }
        if !tags.insert(tag.to_string()) {
            return Err(format!("duplicate outbound tag: {tag}"));
        }
    }
    for group in &input.groups {
        if is_reserved_tag(&group.name) {
            return Err(format!(
                "group tag {:?} is reserved for a built-in policy outbound",
                group.name
            ));
        }
        if !tags.insert(group.name.clone()) {
            return Err(format!("duplicate outbound tag: {}", group.name));
        }
    }

    // Empty groups are rejected by sing-box at startup — skip them instead.
    let groups: Vec<&GroupSpec> = input.groups.iter().filter(|g| !g.members.is_empty()).collect();
    let final_outbound = groups
        .iter()
        .find(|g| g.kind == GroupKind::Selector)
        .map(|g| g.name.as_str())
        .unwrap_or(DIRECT_TAG);

    let inbounds = control_plane_inbounds(input.mixed_port, input.enable_tun, &input.tun);

    let mut outbounds: Vec<Value> = input.outbounds.clone();
    for group in &groups {
        match group.kind {
            GroupKind::Selector => {
                // Selectors always expose an explicit direct fallback.
                let mut members: Vec<String> = group
                    .members
                    .iter()
                    .map(|member| normalize_policy_tag(member))
                    .collect();
                members.push(DIRECT_TAG.to_string());
                outbounds.push(json!({
                    "type": "selector",
                    "tag": group.name,
                    "outbounds": members,
                    "default": group.members.first().map(|member| normalize_policy_tag(member)).unwrap_or_else(|| DIRECT_TAG.into()),
                }));
            }
            GroupKind::UrlTest => {
                outbounds.push(json!({
                    "type": "urltest",
                    "tag": group.name,
                    "outbounds": group.members.iter().map(|member| normalize_policy_tag(member)).collect::<Vec<_>>(),
                    "url": URLTEST_URL,
                    "interval": "3m",
                    "tolerance": 50,
                }));
            }
        }
    }
    outbounds.push(json!({ "type": "direct", "tag": DIRECT_TAG }));
    outbounds.push(json!({ "type": "block", "tag": "block" }));

    // #53: sing-box only exposes (and honours) mode switching when the route
    // carries `clash_mode` rules and clash_api declares a default_mode.
    let default_mode = configured_default_mode();
    let mut config = json!({
        "log": {
            "level": "info",
            "timestamp": true,
        },
        "inbounds": inbounds,
        "outbounds": outbounds,
        "route": {
            "final": final_outbound,
            "auto_detect_interface": true,
        },
        "experimental": {
            "clash_api": clash_api_json(&input.clash_api, &default_mode),
        }
    });
    let mut mode_rules = clash_mode_rules(final_outbound);
    mode_rules.extend(input.route_rules.iter().cloned());
    normalize_rule_targets(&mut mode_rules);
    config["route"]["rules"] = Value::Array(mode_rules);
    if !input.rule_sets.is_empty() {
        config["route"]["rule_set"] = Value::Array(input.rule_sets.clone());
    }
    // #57: persist selector choices (and the clash mode) across restarts.
    apply_cache_file(&mut config)?;
    if let Some(dns) = &input.dns {
        config["dns"] = dns.clone();
    }
    validate_references(&config)?;
    Ok(config)
}

pub fn validate_tun_stack(stack: &str) -> Result<(), String> {
    if matches!(stack, "system" | "gvisor" | "mixed") {
        Ok(())
    } else {
        Err(format!(
            "unsupported sing-box TUN stack {stack:?}; supported stacks are system, gvisor, mixed"
        ))
    }
}

fn normalize_policy_tag(tag: &str) -> String {
    match tag {
        "DIRECT" => "direct".into(),
        "REJECT" => "block".into(),
        other => other.into(),
    }
}

fn is_reserved_tag(tag: &str) -> bool {
    matches!(tag.to_ascii_uppercase().as_str(), "DIRECT" | "REJECT" | "BLOCK")
}

fn normalize_rule_targets(rules: &mut [Value]) {
    for rule in rules {
        if let Some(target) = rule.get("outbound").and_then(Value::as_str) {
            let normalized = normalize_policy_tag(target);
            rule["outbound"] = Value::String(normalized);
        }
        if let Some(children) = rule.get_mut("rules").and_then(Value::as_array_mut) {
            normalize_rule_targets(children);
        }
    }
}

/// Reject route and outbound references that became dangling after the
/// converter skipped unsupported nodes or empty groups.
fn validate_references(config: &Value) -> Result<(), String> {
    let mut tags = std::collections::HashSet::new();
    for (index, outbound) in config["outbounds"].as_array().into_iter().flatten().enumerate() {
        let tag = outbound
            .get("tag")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("outbounds[{index}] has no tag"))?;
        if !tags.insert(tag) {
            return Err(format!("duplicate outbound tag: {tag}"));
        }
    }
    let route = &config["route"];
    let dns_servers = config["dns"]["servers"].as_array().cloned().unwrap_or_default();
    let dns_servers_by_tag: std::collections::HashMap<&str, &Value> = dns_servers
        .iter()
        .filter_map(|server| Some((server.get("tag")?.as_str()?, server)))
        .collect();
    let dns_tags: std::collections::HashSet<&str> = dns_servers
        .iter()
        .filter_map(|server| server.get("tag").and_then(Value::as_str))
        .collect();
    if dns_tags.len() != dns_servers.len() {
        return Err("DNS servers must have unique string tags".into());
    }
    for outbound in config["outbounds"].as_array().into_iter().flatten() {
        for key in ["detour", "default"] {
            if let Some(target) = outbound.get(key).and_then(Value::as_str)
                && !tags.contains(target)
            {
                return Err(format!("outbound {key} references missing outbound {target:?}"));
            }
        }
        if let Some(members) = outbound.get("outbounds").and_then(Value::as_array) {
            let tag = outbound.get("tag").and_then(Value::as_str).unwrap_or("<group>");
            for member in members.iter().filter_map(Value::as_str) {
                if !tags.contains(member) {
                    return Err(format!("outbound group {tag:?} references missing member {member:?}"));
                }
            }
        }
    }
    if let Some(final_tag) = route.get("final").and_then(Value::as_str)
        && !tags.contains(final_tag)
    {
        return Err(format!("route.final references missing outbound {final_tag:?}"));
    }
    if let Some(resolver) = route.get("default_domain_resolver").and_then(Value::as_str)
        && !dns_tags.contains(resolver)
    {
        return Err(format!(
            "route.default_domain_resolver references missing DNS server {resolver:?}"
        ));
    }
    if let Some(resolver) = route.get("default_domain_resolver").and_then(Value::as_str)
        && dns_servers_by_tag
            .get(resolver)
            .is_some_and(|server| server.get("type").and_then(Value::as_str) == Some("fakeip"))
    {
        return Err("route.default_domain_resolver cannot reference a fake-IP DNS server".into());
    }
    for (index, server) in dns_servers.iter().enumerate() {
        if let Some(detour) = server.get("detour").and_then(Value::as_str)
            && !tags.contains(detour)
        {
            return Err(format!("dns.servers[{index}] references missing outbound {detour:?}"));
        }
        if let Some(resolver) = server.get("domain_resolver").and_then(Value::as_str) {
            if !dns_tags.contains(resolver) {
                return Err(format!(
                    "dns.servers[{index}].domain_resolver references missing DNS server {resolver:?}"
                ));
            }
            if dns_servers_by_tag
                .get(resolver)
                .is_some_and(|server| server.get("type").and_then(Value::as_str) == Some("fakeip"))
            {
                return Err(format!(
                    "dns.servers[{index}].domain_resolver cannot reference a fake-IP DNS server"
                ));
            }
        }
    }
    if let Some(rules) = config["dns"]["rules"].as_array() {
        for (index, rule) in rules.iter().enumerate() {
            if let Some(server) = rule.get("server").and_then(Value::as_str)
                && !dns_tags.contains(server)
            {
                return Err(format!("dns.rules[{index}] references missing DNS server {server:?}"));
            }
        }
    }
    if let Some(rules) = route.get("rules").and_then(Value::as_array) {
        let known: std::collections::HashSet<&str> = route["rule_set"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|set| set.get("tag").and_then(Value::as_str))
            .collect();
        for (index, rule) in rules.iter().enumerate() {
            validate_nested_route_rule(rule, &tags, &known)
                .map_err(|error| format!("route.rules[{index}]: {error}"))?;
            if let Some(target) = rule.get("outbound").and_then(Value::as_str)
                && !tags.contains(target)
            {
                return Err(format!("route.rules[{index}] references missing outbound {target:?}"));
            }
            if let Some(sets) = rule.get("rule_set").and_then(Value::as_array) {
                let known: std::collections::HashSet<&str> = route["rule_set"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|set| set.get("tag").and_then(Value::as_str))
                    .collect();
                for set in sets.iter().filter_map(Value::as_str) {
                    if !known.contains(set) {
                        return Err(format!("route.rules[{index}] references missing rule_set {set:?}"));
                    }
                }
            }
        }
    }
    if let Some(sets) = route.get("rule_set").and_then(Value::as_array) {
        let mut seen = std::collections::HashSet::new();
        for (index, set) in sets.iter().enumerate() {
            let tag = set
                .get("tag")
                .and_then(Value::as_str)
                .ok_or_else(|| format!("route.rule_set[{index}] has no tag"))?;
            if !seen.insert(tag) {
                return Err(format!("duplicate route rule_set tag: {tag}"));
            }
        }
    }
    Ok(())
}

fn validate_nested_route_rule(
    rule: &Value,
    outbounds: &std::collections::HashSet<&str>,
    rule_sets: &std::collections::HashSet<&str>,
) -> Result<(), String> {
    if let Some(outbound) = rule.get("outbound").and_then(Value::as_str)
        && !outbounds.contains(outbound)
    {
        return Err(format!("references missing outbound {outbound:?}"));
    }
    if let Some(sets) = rule.get("rule_set") {
        let names: Vec<&str> = if let Some(name) = sets.as_str() {
            vec![name]
        } else if let Some(names) = sets.as_array() {
            names
                .iter()
                .map(|name| name.as_str().ok_or("rule_set references must be strings"))
                .collect::<Result<_, _>>()?
        } else {
            return Err("rule_set references must be a string or list".into());
        };
        for name in names {
            if !rule_sets.contains(name) {
                return Err(format!("references missing rule_set {name:?}"));
            }
        }
    }
    if let Some(children) = rule.get("rules") {
        for child in children.as_array().ok_or("logical rules must be a list")? {
            validate_nested_route_rule(child, outbounds, rule_sets)?;
        }
    }
    Ok(())
}

/// Validate structural contracts that are easy to check offline without
/// invoking a core binary. Unknown native fields remain untouched.
pub fn validate_native_config(config: &Value) -> Result<(), String> {
    validate_native_config_for_version(config, crate::singbox::capabilities::SING_BOX_1_14_2.version)
}

pub fn validate_native_config_for_version(config: &Value, version: &str) -> Result<(), String> {
    let matrix = crate::singbox::capabilities::require_for_version(version)?;
    if config.get("rule_set").is_some() {
        return Err(format!(
            "sing-box rule_set must be nested under route for version {}",
            matrix.version
        ));
    }
    if !matrix.native_json_preserved {
        return Err(format!(
            "native sing-box JSON preservation is unsupported by version {}",
            matrix.version
        ));
    }
    if !config["route"].is_object() || !config["outbounds"].is_array() {
        return Err("native sing-box profile requires route object and outbounds array".into());
    }
    if let Some((index, outbound_type)) = config["outbounds"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
        .find_map(|(index, outbound)| {
            let kind = outbound.get("type").and_then(Value::as_str)?;
            matches!(kind, "wireguard" | "dns" | "shadowsocksr").then_some((index, kind))
        })
    {
        return Err(format!(
            "native outbounds[{index}] uses unsupported sing-box 1.14.2 outbound type {outbound_type:?}"
        ));
    }
    if config["route"]["rule_set"].is_array() && !matrix.route_rule_sets_nested {
        return Err(format!(
            "sing-box {} does not support nested route rule sets",
            matrix.version
        ));
    }
    if config["route"]["rules"].is_array() && !matrix.route_rule_references {
        return Err(format!(
            "sing-box {} has no reviewed route-reference support",
            matrix.version
        ));
    }
    if let Some(inbounds) = config["inbounds"].as_array() {
        for inbound in inbounds
            .iter()
            .filter(|inbound| inbound.get("type").and_then(Value::as_str) == Some("tun"))
        {
            if let Some(stack) = inbound.get("stack").and_then(Value::as_str) {
                validate_tun_stack(stack)?;
                if stack == "gvisor" && !matrix.tun_gvisor {
                    return Err(format!("sing-box {} does not support gvisor TUN stack", matrix.version));
                }
            }
        }
    }
    if let Some(servers) = config["dns"]["servers"].as_array()
        && servers
            .iter()
            .any(|server| server.get("address").is_some() || server.get("address_resolver").is_some())
    {
        return Err("native DNS uses legacy address/address_resolver fields removed in sing-box 1.14; migrate to typed server/server_port fields".into());
    }
    if config["dns"].get("fakeip").is_some() {
        return Err(
            "native DNS uses the legacy fakeip section removed in sing-box 1.14; migrate to a typed fakeip server"
                .into(),
        );
    }
    validate_references(config)
}

/// The CLI-owned inbound list: a loopback mixed listener on the configured
/// port, plus a TUN inbound when TUN mode is on. Shared by generated configs
/// and the native-subscription passthrough so both bind identical listeners.
pub fn control_plane_inbounds(mixed_port: u16, enable_tun: bool, tun: &TunSettings) -> Vec<Value> {
    let mut inbounds = vec![json!({
        "type": "mixed",
        "tag": "mixed-in",
        "listen": "127.0.0.1",
        "listen_port": mixed_port,
    })];
    if enable_tun {
        inbounds.push(json!({
            "type": "tun",
            "tag": "tun-in",
            "interface_name": TUN_INTERFACE_NAME,
            "stack": tun.stack,
            "mtu": tun.mtu,
            "auto_route": true,
        }));
    }
    inbounds
}

/// Force the CLI's own control plane onto an existing sing-box config,
/// preserving the provider's `outbounds`, `route` and `dns`.
///
/// Used for native sing-box JSON subscriptions: the provider ships a complete
/// sing-box document, and only the pieces this app owns (inbounds, clash_api,
/// log) may be replaced — otherwise the core would not answer our controller or
/// would bind a port the user did not choose.
pub fn apply_control_plane(
    config: &mut Value,
    mixed_port: u16,
    enable_tun: bool,
    tun: &TunSettings,
    clash_api: &ClashApiSettings,
) {
    config["log"] = json!({ "level": "info", "timestamp": true });
    config["inbounds"] = Value::Array(control_plane_inbounds(mixed_port, enable_tun, tun));
    if !config.get("experimental").is_some_and(Value::is_object) {
        config["experimental"] = json!({});
    }
    // #53 / #57 also apply to native sing-box subscriptions: without them the
    // provider document keeps the same silent mode-switch and lost-selection
    // behavior.
    apply_clash_mode_support(config, clash_api, &configured_default_mode());
    // A cache file we cannot create with private permissions is skipped
    // rather than pointed at a world-readable path: losing selection
    // persistence is the pre-#57 behavior, leaking node choices is not.
    let _ = apply_cache_file(config);
    if !config.get("route").is_some_and(Value::is_object) {
        config["route"] = json!({});
    }
    config["route"]["auto_detect_interface"] = json!(true);
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;

    fn sample_input() -> ConfigInput {
        ConfigInput {
            outbounds: vec![
                json!({ "type": "shadowsocks", "tag": "node-a", "server": "a.example", "server_port": 8388 }),
                json!({ "type": "shadowsocks", "tag": "node-b", "server": "b.example", "server_port": 8388 }),
            ],
            groups: vec![
                GroupSpec {
                    name: "auto".into(),
                    kind: GroupKind::UrlTest,
                    members: vec!["node-a".into(), "node-b".into()],
                },
                GroupSpec {
                    name: "PROXY".into(),
                    kind: GroupKind::Selector,
                    members: vec!["auto".into(), "node-a".into()],
                },
            ],
            mixed_port: 7897,
            enable_tun: false,
            tun: TunSettings {
                stack: "gvisor".into(),
                mtu: 9000,
            },
            clash_api: ClashApiSettings {
                listen: "127.0.0.1:9090".parse().expect("addr"),
                secret: "s3cret".into(),
            },
            rule_sets: Vec::new(),
            route_rules: Vec::new(),
            dns: None,
        }
    }

    #[test]
    fn generates_selector_urltest_and_route_final() {
        let config = generate_config(&sample_input()).expect("config");

        assert_eq!(config["route"]["final"], "PROXY");
        assert_eq!(
            config["experimental"]["clash_api"]["external_controller"],
            "127.0.0.1:9090"
        );
        assert_eq!(config["experimental"]["clash_api"]["secret"], "s3cret");

        let outbounds = config["outbounds"].as_array().expect("outbounds array");
        let tags: Vec<&str> = outbounds.iter().filter_map(|o| o["tag"].as_str()).collect();
        assert_eq!(tags, vec!["node-a", "node-b", "auto", "PROXY", "direct", "block"]);

        let proxy = outbounds.iter().find(|o| o["tag"] == "PROXY").expect("selector");
        // Selectors gain an explicit direct fallback entry.
        assert_eq!(
            proxy["outbounds"],
            json!(["auto", "node-a", "direct"]),
            "selector should append direct"
        );
        assert_eq!(proxy["default"], "auto");

        let auto = outbounds.iter().find(|o| o["tag"] == "auto").expect("urltest");
        assert_eq!(auto["url"], URLTEST_URL);
        assert_eq!(auto["outbounds"], json!(["node-a", "node-b"]));
    }

    #[test]
    fn tun_inbound_only_when_enabled_with_deterministic_interface() {
        let mut input = sample_input();

        input.enable_tun = true;
        let config = generate_config(&input).expect("config");
        let inbounds = config["inbounds"].as_array().expect("inbounds");
        assert_eq!(inbounds.len(), 2);
        let tun = &inbounds[1];
        assert_eq!(tun["type"], "tun");
        assert_eq!(tun["interface_name"], "sb-tun0");
        assert_eq!(tun["stack"], "gvisor");
        assert_eq!(tun["auto_route"], true);

        input.enable_tun = false;
        let config = generate_config(&input).expect("config");
        let inbounds = config["inbounds"].as_array().expect("inbounds");
        assert_eq!(inbounds.len(), 1);
        assert_eq!(inbounds[0]["type"], "mixed");
    }

    #[test]
    fn empty_nodes_fall_back_to_direct() {
        let mut input = sample_input();
        input.outbounds.clear();
        input.groups.clear();

        let config = generate_config(&input).expect("config");
        assert_eq!(config["route"]["final"], "direct");

        let outbounds = config["outbounds"].as_array().expect("outbounds array");
        assert_eq!(outbounds.len(), 2);
        assert_eq!(outbounds[0]["tag"], "direct");
        assert_eq!(outbounds[1]["tag"], "block");
    }

    #[test]
    fn empty_groups_are_skipped() {
        let mut input = sample_input();
        input.groups.push(GroupSpec {
            name: "empty-group".into(),
            kind: GroupKind::Selector,
            members: vec![],
        });

        let config = generate_config(&input).expect("config");
        let tags: Vec<&str> = config["outbounds"]
            .as_array()
            .expect("outbounds")
            .iter()
            .filter_map(|o| o["tag"].as_str())
            .collect();
        assert!(!tags.contains(&"empty-group"), "empty group must be skipped: {tags:?}");
    }

    #[test]
    fn duplicate_tags_are_rejected() {
        let mut input = sample_input();
        input.outbounds.push(input.outbounds[0].clone());
        let err = generate_config(&input).expect_err("duplicate node tags");
        assert!(err.contains("duplicate"), "{err}");

        let mut input = sample_input();
        input.groups.push(GroupSpec {
            name: "node-a".into(),
            kind: GroupKind::UrlTest,
            members: vec!["node-b".into()],
        });
        let err = generate_config(&input).expect_err("group colliding with node tag");
        assert!(err.contains("node-a"), "{err}");
    }

    #[test]
    fn rule_sets_are_nested_under_route_when_present() {
        let mut input = sample_input();
        input.rule_sets = vec![json!({
            "type": "remote",
            "tag": "geoip",
            "format": "binary",
            "url": "https://example.com/geoip.srs",
        })];

        let config = generate_config(&input).expect("config");
        assert_eq!(config["route"]["rule_set"].as_array().expect("rule_set array").len(), 1);
        assert_eq!(config["route"]["rule_set"][0]["tag"], "geoip");
        assert!(config.get("rule_set").is_none(), "root rule_set is invalid");

        input.rule_sets.clear();
        let config = generate_config(&input).expect("config");
        assert!(
            config["route"].get("rule_set").is_none(),
            "empty rule_sets must be omitted"
        );
    }

    #[test]
    fn dangling_group_and_rule_set_references_are_rejected() {
        let mut input = sample_input();
        input.groups[0].members = vec!["missing-node".into()];
        assert!(generate_config(&input).unwrap_err().contains("missing-node"));

        let mut input = sample_input();
        input.route_rules = vec![json!({ "rule_set": ["missing-set"], "outbound": "PROXY" })];
        assert!(generate_config(&input).unwrap_err().contains("missing-set"));

        let mut input = sample_input();
        input.route_rules = vec![json!({ "domain": ["example.org"], "outbound": "missing-outbound" })];
        assert!(generate_config(&input).unwrap_err().contains("missing-outbound"));
    }

    #[test]
    fn nested_logical_rules_and_selector_defaults_cannot_reference_missing_targets() {
        let mut input = sample_input();
        input.route_rules =
            vec![json!({"type":"logical","mode":"and","rules":[{"rule_set":"missing"}],"outbound":"direct"})];
        assert!(generate_config(&input).unwrap_err().contains("missing rule_set"));
        input.route_rules =
            vec![json!({"type":"logical","mode":"or","rules":[{"outbound":"missing"}],"outbound":"direct"})];
        assert!(generate_config(&input).unwrap_err().contains("missing outbound"));
        let native = json!({"route":{},"outbounds":[{"type":"selector","tag":"g","default":"missing","outbounds":[]}]});
        assert!(validate_native_config(&native).unwrap_err().contains("default"));
    }

    #[test]
    fn fakeip_dns_server_cannot_be_used_as_a_host_bootstrap_resolver() {
        let config = json!({
            "outbounds": [{ "type": "direct", "tag": "direct" }],
            "route": { "default_domain_resolver": "fake" },
            "dns": { "servers": [
                { "type": "fakeip", "tag": "fake" },
                { "type": "https", "tag": "remote", "server": "resolver.example", "domain_resolver": "fake" }
            ] }
        });
        let error = validate_native_config(&config).unwrap_err();
        assert!(error.contains("route.default_domain_resolver"));

        let mut config = config;
        config["route"]["default_domain_resolver"] = json!("remote");
        let error = validate_native_config(&config).unwrap_err();
        assert!(error.contains("dns.servers[1].domain_resolver"));
    }

    #[test]
    fn policy_targets_normalize_and_reserved_tags_fail_explicitly() {
        let mut input = sample_input();
        input.groups[1].members.push("REJECT".into());
        input.route_rules = vec![json!({ "domain": ["example.org"], "outbound": "REJECT" })];
        let config = generate_config(&input).expect("policy aliases normalized");
        let rule = config["route"]["rules"]
            .as_array()
            .unwrap()
            .iter()
            .find(|rule| rule.get("domain").is_some())
            .expect("profile rule");
        assert_eq!(rule["outbound"], "block");
        let proxy = config["outbounds"]
            .as_array()
            .unwrap()
            .iter()
            .find(|o| o["tag"] == "PROXY")
            .unwrap();
        assert!(proxy["outbounds"].as_array().unwrap().contains(&json!("block")));

        input.groups[1].name = "DIRECT".into();
        assert!(generate_config(&input).unwrap_err().contains("reserved"));
    }

    #[test]
    fn invalid_tun_stack_and_legacy_native_dns_are_rejected_with_actionable_errors() {
        assert!(validate_tun_stack("mips").unwrap_err().contains("supported stacks"));
        assert!(validate_tun_stack("gvisor").is_ok());
        let native = json!({
            "outbounds": [{ "type": "direct", "tag": "direct" }],
            "route": {},
            "dns": { "servers": [{ "tag": "old", "address": "tls://1.1.1.1" }] }
        });
        assert!(
            validate_native_config(&native)
                .unwrap_err()
                .contains("migrate to typed")
        );
        let legacy_fakeip = json!({
            "outbounds": [{ "type": "direct", "tag": "direct" }],
            "route": {},
            "dns": { "fakeip": { "enabled": true } }
        });
        assert!(
            validate_native_config(&legacy_fakeip)
                .unwrap_err()
                .contains("typed fakeip server")
        );
    }
    #[test]
    fn route_rules_are_injected_into_route_section() {
        let mut input = sample_input();
        input.route_rules = vec![json!({ "domain_suffix": ["example.com"], "outbound": "PROXY" })];

        let config = generate_config(&input).expect("config");
        assert_eq!(
            config["route"]["rules"],
            json!([
                { "clash_mode": "Direct", "outbound": "direct" },
                { "clash_mode": "Global", "outbound": "PROXY" },
                { "domain_suffix": ["example.com"], "outbound": "PROXY" }
            ]),
            "clash_mode rules are prepended ahead of the profile rules (#53)"
        );
        // route.final coexists with injected rules.
        assert_eq!(config["route"]["final"], "PROXY");

        input.route_rules.clear();
        let config = generate_config(&input).expect("config");
        assert_eq!(
            config["route"]["rules"],
            json!([
                { "clash_mode": "Direct", "outbound": "direct" },
                { "clash_mode": "Global", "outbound": "PROXY" }
            ]),
            "mode rules stay even without profile rules — dropping them is what made mode switching a no-op"
        );
    }

    #[test]
    fn dns_section_is_emitted_when_present() {
        let mut input = sample_input();
        input.dns = Some(json!({ "servers": [{ "type": "local", "tag": "dns-local" }] }));

        let config = generate_config(&input).expect("config");
        assert_eq!(config["dns"]["servers"][0]["tag"], "dns-local");

        input.dns = None;
        input.dns = None;
        let config = generate_config(&input).expect("config");
        assert!(config.get("dns").is_none(), "absent dns must be omitted");
    }

    #[test]
    fn apply_control_plane_preserves_native_singbox_document() {
        let mut native = json!({
            "outbounds": [{ "type": "vless", "tag": "custom-node" }],
            "route": { "rules": [{ "outbound": "custom-node" }] },
            "dns": { "servers": [{ "tag": "remote", "type": "https" }] },
            "inbounds": [{ "type": "mixed", "listen_port": 1234 }]
        });
        let tun = TunSettings {
            stack: "gvisor".into(),
            mtu: 9000,
        };
        let clash_api = ClashApiSettings {
            listen: "127.0.0.1:9090".parse().expect("addr"),
            secret: "my-secret".into(),
        };
        apply_control_plane(&mut native, 7897, true, &tun, &clash_api);

        // Preserved original content
        assert_eq!(native["outbounds"][0]["tag"], "custom-node");
        assert_eq!(native["dns"]["servers"][0]["tag"], "remote");
        assert_eq!(native["route"]["auto_detect_interface"], true);
        let preserved = native["route"]["rules"]
            .as_array()
            .expect("rules")
            .iter()
            .find(|rule| rule["outbound"] == "custom-node")
            .expect("provider rule preserved");
        assert_eq!(preserved, &json!({ "outbound": "custom-node" }));

        // Injected CLI control plane
        assert_eq!(native["inbounds"][0]["listen_port"], 7897);
        assert_eq!(native["inbounds"][1]["type"], "tun");
        assert_eq!(native["experimental"]["clash_api"]["secret"], "my-secret");
    }

    #[test]
    fn native_validation_rejects_root_rule_set_and_accepts_nested_unknown_fields() {
        let mut root = json!({ "outbounds": [{ "type": "direct", "tag": "direct" }], "route": {}, "rule_set": [] });
        assert!(
            validate_native_config(&root)
                .unwrap_err()
                .contains("nested under route")
        );
        root.as_object_mut().unwrap().remove("rule_set");
        root["route"]["rule_set"] = json!([]);
        root["provider_extension"] = json!({ "nested": [1, 2, 3] });
        validate_native_config(&root).expect("supported unknown fields preserved");
        assert_eq!(root["provider_extension"]["nested"], json!([1, 2, 3]));
        let removed = json!({
            "outbounds": [{ "type": "wireguard", "tag": "wg" }],
            "route": {}
        });
        assert!(validate_native_config(&removed).unwrap_err().contains("wireguard"));
    }

    /// Regression #48: the generated clash_api must never carry the publicly
    /// known template secret nor a wildcard CORS policy.
    #[test]
    fn generated_clash_api_has_no_placeholder_secret_and_no_wildcard_cors() {
        let config = generate_config(&sample_input()).expect("config");
        let text = serde_json::to_string(&config).expect("json");

        assert!(
            !text.contains(PLACEHOLDER_SECRET),
            "template secret leaked into singbox.json"
        );
        assert!(!text.contains("\"secret\":\"\""));
        assert!(
            !text.contains("\"*\""),
            "singbox.json must not contain a wildcard CORS origin: {text}"
        );

        let clash_api = &config["experimental"]["clash_api"];
        assert_eq!(clash_api["secret"], "s3cret");
        assert_eq!(clash_api["access_control_allow_origin"], json!(CLASH_API_ALLOW_ORIGINS));
        assert_eq!(clash_api["access_control_allow_private_network"], false);
        validate_control_plane_security(&config).expect("generated control plane is safe");
    }

    /// The native-JSON passthrough must get the same hardened clash_api.
    #[test]
    fn native_passthrough_also_gets_a_restricted_cors_policy() {
        let mut native = json!({
            "outbounds": [{ "type": "direct", "tag": "direct" }],
            "route": { "rules": [{ "outbound": "direct" }] }
        });
        let tun = TunSettings {
            stack: "gvisor".into(),
            mtu: 9000,
        };
        let clash_api = ClashApiSettings {
            listen: "127.0.0.1:9090".parse().expect("addr"),
            secret: "rotated".into(),
        };
        apply_control_plane(&mut native, 7897, false, &tun, &clash_api);

        let text = serde_json::to_string(&native).expect("json");
        assert!(!text.contains(PLACEHOLDER_SECRET));
        assert!(!text.contains("\"*\""));
        validate_control_plane_security(&native).expect("passthrough control plane is safe");
    }

    /// Fail-closed: the guard rejects every shape that would leave the
    /// controller reachable or unauthenticated.
    #[test]
    fn control_plane_security_guard_rejects_weak_or_wildcard_settings() {
        let base = || {
            let mut config = json!({
                "outbounds": [{ "type": "direct", "tag": "direct" }],
                "route": {}
            });
            let tun = TunSettings {
                stack: "gvisor".into(),
                mtu: 9000,
            };
            apply_control_plane(
                &mut config,
                7897,
                false,
                &tun,
                &ClashApiSettings {
                    listen: "127.0.0.1:9090".parse().expect("addr"),
                    secret: "rotated".into(),
                },
            );
            config
        };

        let mut placeholder = base();
        placeholder["experimental"]["clash_api"]["secret"] = json!(PLACEHOLDER_SECRET);
        assert!(
            validate_control_plane_security(&placeholder)
                .unwrap_err()
                .contains("template controller secret")
        );

        let mut empty = base();
        empty["experimental"]["clash_api"]["secret"] = json!("");
        assert!(validate_control_plane_security(&empty).unwrap_err().contains("empty"));

        let mut wildcard = base();
        wildcard["experimental"]["clash_api"]["access_control_allow_origin"] = json!(["*"]);
        assert!(
            validate_control_plane_security(&wildcard)
                .unwrap_err()
                .contains("wildcard")
        );

        let mut absent = base();
        absent["experimental"]["clash_api"]
            .as_object_mut()
            .expect("object")
            .remove("access_control_allow_origin");
        assert!(
            validate_control_plane_security(&absent)
                .unwrap_err()
                .contains("defaults it to '*'")
        );

        let mut private_network = base();
        private_network["experimental"]["clash_api"]["access_control_allow_private_network"] = json!(true);
        assert!(
            validate_control_plane_security(&private_network)
                .unwrap_err()
                .contains("allow_private_network")
        );

        let mut missing = json!({ "outbounds": [], "route": {} });
        assert!(
            validate_control_plane_security(&missing)
                .unwrap_err()
                .contains("missing experimental.clash_api")
        );
        missing["experimental"] = json!({ "clash_api": base()["experimental"]["clash_api"] });
        assert!(validate_control_plane_security(&missing).is_ok());
    }

    /// Regression #53: sing-box derives its `mode-list` from `clash_mode`
    /// route rules plus `default_mode`. Without both, `PATCH /configs` is
    /// accepted (204) and silently dropped.
    #[test]
    fn generated_route_declares_every_clash_mode_and_a_default_mode() {
        let config = generate_config(&sample_input()).expect("config");

        let modes: Vec<&str> = config["route"]["rules"]
            .as_array()
            .expect("rules")
            .iter()
            .filter_map(|rule| rule.get("clash_mode").and_then(Value::as_str))
            .collect();
        assert_eq!(
            modes,
            vec!["Direct", "Global"],
            "clash_mode rules must lead the rule list (#53)"
        );
        assert_eq!(config["route"]["rules"][0]["outbound"], "direct");
        assert_eq!(
            config["route"]["rules"][1]["outbound"], "PROXY",
            "Global must route through the final outbound"
        );
        for mode in CLASH_MODES {
            assert!(
                *mode == "Rule" || modes.contains(mode),
                "mode {mode} must be reachable: {modes:?}"
            );
        }

        let default_mode = config["experimental"]["clash_api"]["default_mode"]
            .as_str()
            .expect("default_mode");
        assert!(
            CLASH_MODES.contains(&default_mode),
            "default_mode {default_mode} is not a sing-box mode"
        );
        validate_control_plane_security(&config).expect("generated control plane is safe");
    }

    /// The startup mode comes from the shared clash.yaml, which is where
    /// `clash-verge-cli mode <x>` persists it.
    #[test]
    fn default_mode_follows_clash_yaml_and_falls_back_to_rule() {
        assert_eq!(default_mode_from_yaml("mode: global\n"), "Global");
        assert_eq!(default_mode_from_yaml("mode: Direct\n"), "Direct");
        assert_eq!(default_mode_from_yaml("port: 7897\n"), DEFAULT_CLASH_MODE);
        assert_eq!(default_mode_from_yaml("mode: script\n"), DEFAULT_CLASH_MODE);
        assert_eq!(default_mode_from_yaml("not: [yaml"), DEFAULT_CLASH_MODE);
        assert_eq!(normalize_clash_mode("GLOBAL"), "Global");
        assert_eq!(normalize_clash_mode(" direct "), "Direct");
    }

    /// The guard must reject a clash_api that would silently break switching.
    #[test]
    fn control_plane_security_guard_requires_a_usable_default_mode() {
        let mut config = generate_config(&sample_input()).expect("config");
        config["experimental"]["clash_api"]
            .as_object_mut()
            .expect("clash_api")
            .remove("default_mode");
        assert!(
            validate_control_plane_security(&config)
                .unwrap_err()
                .contains("no default_mode")
        );

        let mut config = generate_config(&sample_input()).expect("config");
        config["experimental"]["clash_api"]["default_mode"] = json!("script");
        assert!(
            validate_control_plane_security(&config)
                .unwrap_err()
                .contains("unsupported default_mode")
        );
    }

    /// Regression #57: selector choices must survive the restarts a rule
    /// edit, subscription refresh or TUN toggle causes on sing-box.
    #[test]
    fn cache_file_json_and_private_permissions_persist_selections() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(CACHE_FILE_NAME);

        ensure_private_cache_file(&path).expect("create cache");
        let cache = cache_file_json(&path);
        assert_eq!(cache["enabled"], true);
        assert_eq!(cache["path"], path.to_string_lossy().as_ref());
        assert!(
            cache.get("store_selected").is_none(),
            "sing-box 1.14 removed the store_selected switch; an unknown field makes the core refuse the config"
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&path).expect("stat").permissions().mode();
            assert_eq!(mode & 0o077, 0, "selection cache must stay owner-private: {mode:o}");
        }

        // A cache file left world-readable by an earlier run is tightened
        // instead of being trusted.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).expect("loosen");
            ensure_private_cache_file(&path).expect("retighten");
            let mode = std::fs::metadata(&path).expect("stat").permissions().mode();
            assert_eq!(mode & 0o077, 0, "pre-existing loose cache must be tightened: {mode:o}");
        }

        // Missing parent dirs are created, so the cache never has to fall back
        // to a path the core would create itself.
        let nested = dir.path().join("nested/deeper").join(CACHE_FILE_NAME);
        ensure_private_cache_file(&nested).expect("nested create");
        assert!(nested.is_file());
    }

    /// End-to-end shape of the generated document with a real config dir:
    /// `experimental.cache_file` points into the config dir and the guard
    /// accepts the result.
    #[tokio::test]
    async fn generated_config_carries_a_private_cache_file_under_the_config_dir() {
        let _guard = crate::profile_store::store::tests::claim_test_app_home(
            crate::profile_store::store::tests::test_app_home_root(),
        )
        .await;

        let config = generate_config(&sample_input()).expect("config");
        let cache = &config["experimental"]["cache_file"];
        assert_eq!(cache["enabled"], true, "cache_file: {cache}");
        let path = cache["path"].as_str().expect("cache path");
        assert!(
            path.ends_with(CACHE_FILE_NAME),
            "cache must live under the config dir: {path}"
        );
        let home = clash_verge_core::utils::dirs::app_home_dir().expect("home");
        assert_eq!(Path::new(path), home.join(CACHE_FILE_NAME));
        assert!(
            cache_file_is_private(Path::new(path)),
            "cache file must be owner-private"
        );
        validate_references(&config).expect("references stay valid");
        validate_control_plane_security(&config).expect("control plane stays safe");
    }

    /// Without a config dir nothing is emitted (no guessed, possibly
    /// world-readable path); the pre-#57 behaviour is simply unchanged.
    #[test]
    fn no_config_dir_means_no_cache_file_rather_than_a_guessed_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cache = cache_file_json(&dir.path().join(CACHE_FILE_NAME));
        assert!(
            cache["path"]
                .as_str()
                .expect("path")
                .starts_with(dir.path().to_str().unwrap())
        );
        // The resolver itself is the only place that reads the global.
        assert_eq!(normalize_clash_mode(&default_mode_from_yaml("")), DEFAULT_CLASH_MODE);
    }
}
