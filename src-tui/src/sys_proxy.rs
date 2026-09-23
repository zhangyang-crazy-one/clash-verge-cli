//! System proxy management for desktop environments (GNOME/KDE).
//!
//! Intentionally does **not** mutate process environment variables: calling
//! `std::env::set_var` from a multithreaded Tokio runtime is unsound when other
//! tasks (e.g. reqwest) may read the environment concurrently.
//!
//! Lifecycle: the desktop proxy follows the core. When `enable_system_proxy`
//! is on it is applied after mihomo spawns and released when mihomo stops or
//! dies, so the desktop is never left pointing at a dead port. Before the
//! first apply the previous desktop settings are snapshotted to
//! `<config-dir>/sysproxy-snapshot.json` and restored on release. Release
//! only acts when that file exists (we applied the proxy) and only touches a
//! backend whose settings still point at our endpoint.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};

/// Hosts that never go through the proxy when `use_default_bypass` is on.
const DEFAULT_BYPASS: &[&str] = &[
    "localhost",
    "127.0.0.0/8",
    "::1",
    "10.0.0.0/8",
    "172.16.0.0/12",
    "192.168.0.0/16",
];

const SNAPSHOT_FILE: &str = "sysproxy-snapshot.json";

/// Proxy endpoint and bypass list to publish to the desktop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxySettings {
    pub host: String,
    pub port: u16,
    pub bypass: Vec<String>,
}

impl ProxySettings {
    pub fn from_config(verge: &clash_verge_core::config::IVerge, mixed_port: u16) -> Self {
        Self {
            host: verge.proxy_host.as_deref().unwrap_or("127.0.0.1").to_string(),
            port: mixed_port,
            bypass: bypass_list(
                verge.use_default_bypass.unwrap_or(true),
                verge.system_proxy_bypass.as_deref(),
            ),
        }
    }

    /// Settings from the current `verge.yaml` and clash config.
    pub async fn load() -> Self {
        let verge = clash_verge_core::config::IVerge::new().await;
        let clash = clash_verge_core::config::IClashTemp::new().await;
        Self::from_config(&verge, clash.get_mixed_port())
    }
}

/// Merge the default bypass list with the user's `system_proxy_bypass`
/// (comma/semicolon separated, GUI format), dropping blanks and duplicates.
pub fn bypass_list(use_default: bool, custom: Option<&str>) -> Vec<String> {
    let defaults = DEFAULT_BYPASS.iter().copied().filter(|_| use_default);
    let custom = custom.unwrap_or_default().split([',', ';']);
    let mut list: Vec<String> = Vec::new();
    for entry in defaults.chain(custom).map(str::trim).filter(|e| !e.is_empty()) {
        if !list.iter().any(|existing| existing == entry) {
            list.push(entry.to_string());
        }
    }
    list
}

/// Shell commands exporting the proxy for terminal programs, for
/// `eval "$(clash-verge-cli sysproxy env)"`.
pub fn env_exports(settings: &ProxySettings) -> String {
    let http = format!("http://{}:{}", settings.host, settings.port);
    let socks = format!("socks5://{}:{}", settings.host, settings.port);
    let no_proxy = settings.bypass.join(",");
    let mut out = String::new();
    for (name, value) in [
        ("http_proxy", &http),
        ("https_proxy", &http),
        ("all_proxy", &socks),
        ("no_proxy", &no_proxy),
    ] {
        out.push_str(&format!("export {name}={}\n", shell_quote(value)));
        out.push_str(&format!("export {}={}\n", name.to_uppercase(), shell_quote(value)));
    }
    out
}

/// Shell commands undoing [`env_exports`].
pub fn env_unsets() -> String {
    let names = ["http_proxy", "https_proxy", "all_proxy", "no_proxy"];
    let all: Vec<String> = names
        .iter()
        .flat_map(|name| [name.to_string(), name.to_uppercase()])
        .collect();
    format!("unset {}\n", all.join(" "))
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

/// Toggle the desktop proxy on (explicit user action or core start).
pub fn set_system_proxy(settings: &ProxySettings) -> anyhow::Result<()> {
    enable_with(&SystemRunner, &snapshot_path()?, settings)
}

/// Toggle the desktop proxy off (explicit user action): restore what was
/// there before we applied ours, or switch the proxy off.
pub fn unset_system_proxy() -> anyhow::Result<()> {
    disable_with(&SystemRunner, &snapshot_path()?)
}

/// Serialises lifecycle apply/release within the process so a release for
/// a core that died immediately can never interleave with its apply.
static LIFECYCLE_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Core started: publish the proxy when the user enabled it. Best effort.
///
/// Callers run this before the exit watcher exists, so any release for this
/// core is ordered after it.
pub async fn apply_on_core_start() {
    let verge = clash_verge_core::config::IVerge::new().await;
    if !verge.enable_system_proxy.unwrap_or(false) {
        return;
    }
    let settings = ProxySettings::load().await;
    let _lifecycle = LIFECYCLE_LOCK.lock().await;
    match tokio::task::spawn_blocking(move || set_system_proxy(&settings)).await {
        Ok(Ok(())) => tracing::info!(target: "sysproxy", "system proxy applied"),
        Ok(Err(error)) => tracing::warn!(target: "sysproxy", "failed to apply system proxy: {error}"),
        Err(error) => tracing::warn!(target: "sysproxy", "system proxy task failed: {error}"),
    }
}

/// Core stopped or died: withdraw the proxy if it still points at us so the
/// desktop is not left pointing at a closed port. Best effort.
pub async fn release_on_core_stop() {
    let settings = ProxySettings::load().await;
    let Ok(snapshot) = snapshot_path() else {
        return;
    };
    let _lifecycle = LIFECYCLE_LOCK.lock().await;
    match tokio::task::spawn_blocking(move || release_with(&SystemRunner, &snapshot, &settings)).await {
        Ok(Ok(true)) => tracing::info!(target: "sysproxy", "system proxy released"),
        Ok(Ok(false)) => {}
        Ok(Err(error)) => tracing::warn!(target: "sysproxy", "failed to release system proxy: {error}"),
        Err(error) => tracing::warn!(target: "sysproxy", "system proxy task failed: {error}"),
    }
}

fn snapshot_path() -> anyhow::Result<PathBuf> {
    Ok(clash_verge_core::utils::dirs::app_home_dir()?.join(SNAPSHOT_FILE))
}

// ---------------------------------------------------------------------------
// Backends

/// Runs desktop tools; injectable so the backends can be tested.
trait Runner {
    /// Run to completion; true on exit status 0.
    fn run(&self, program: &str, args: &[&str]) -> bool;
    /// Trimmed stdout on exit status 0.
    fn output(&self, program: &str, args: &[&str]) -> Option<String>;
    fn exists(&self, program: &str) -> bool;
}

struct SystemRunner;

impl Runner for SystemRunner {
    fn run(&self, program: &str, args: &[&str]) -> bool {
        Command::new(program)
            .args(args)
            .output()
            .map(|output| output.status.success())
            .unwrap_or(false)
    }

    fn output(&self, program: &str, args: &[&str]) -> Option<String> {
        let output = Command::new(program).args(args).output().ok()?;
        output
            .status
            .success()
            .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    fn exists(&self, program: &str) -> bool {
        std::env::var_os("PATH").is_some_and(|path| {
            std::env::split_paths(&path).any(|dir| {
                let candidate = dir.join(program);
                std::fs::metadata(candidate).is_ok_and(|meta| meta.is_file())
            })
        })
    }
}

/// Desktop settings captured before the first apply, per backend, plus the
/// endpoint we last published.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Snapshot {
    /// Endpoint written to the desktop by the last apply. Release matches the
    /// desktop against this, not the current config, so editing
    /// `proxy_host`/`mixed-port` while the core runs cannot orphan the proxy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    applied: Option<Endpoint>,
    /// `"<schema> <key>"` → raw `gsettings get` value (GVariant text).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    gnome: BTreeMap<String, String>,
    /// kioslaverc `Proxy Settings` key → value (empty = unset).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    kde: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Endpoint {
    host: String,
    port: u16,
}

impl Endpoint {
    fn of(settings: &ProxySettings) -> Self {
        Self {
            host: settings.host.clone(),
            port: settings.port,
        }
    }

    /// Settings carrying only this endpoint (all the ownership checks read).
    fn as_settings(&self) -> ProxySettings {
        ProxySettings {
            host: self.host.clone(),
            port: self.port,
            bypass: Vec::new(),
        }
    }
}

fn read_snapshot(path: &Path) -> Snapshot {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

fn write_snapshot(path: &Path, snapshot: &Snapshot) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, serde_json::to_string_pretty(snapshot)?)?;
    Ok(())
}

const GNOME_KEYS: &[(&str, &str)] = &[
    ("org.gnome.system.proxy", "mode"),
    ("org.gnome.system.proxy", "ignore-hosts"),
    ("org.gnome.system.proxy.http", "host"),
    ("org.gnome.system.proxy.http", "port"),
    ("org.gnome.system.proxy.https", "host"),
    ("org.gnome.system.proxy.https", "port"),
    ("org.gnome.system.proxy.socks", "host"),
    ("org.gnome.system.proxy.socks", "port"),
];

const KDE_GROUP: &str = "Proxy Settings";
const KDE_KEYS: &[&str] = &["ProxyType", "httpProxy", "httpsProxy", "socksProxy", "NoProxyFor"];

fn gvariant_string(value: &str) -> String {
    format!("'{}'", value.replace('\\', r"\\").replace('\'', r"\'"))
}

fn gvariant_string_array(values: &[String]) -> String {
    let items: Vec<String> = values.iter().map(|v| gvariant_string(v)).collect();
    format!("[{}]", items.join(", "))
}

struct Gnome;

impl Gnome {
    fn available(runner: &dyn Runner) -> bool {
        runner.exists("gsettings")
            && runner
                .output("gsettings", &["get", "org.gnome.system.proxy", "mode"])
                .is_some()
    }

    fn get(runner: &dyn Runner, schema: &str, key: &str) -> Option<String> {
        runner.output("gsettings", &["get", schema, key])
    }

    fn set(runner: &dyn Runner, schema: &str, key: &str, value: &str) -> bool {
        runner.run("gsettings", &["set", schema, key, value])
    }

    fn is_ours(runner: &dyn Runner, settings: &ProxySettings) -> bool {
        Self::get(runner, "org.gnome.system.proxy", "mode").as_deref() == Some("'manual'")
            && Self::get(runner, "org.gnome.system.proxy.http", "host") == Some(gvariant_string(&settings.host))
            && Self::get(runner, "org.gnome.system.proxy.http", "port") == Some(settings.port.to_string())
    }

    fn snapshot(runner: &dyn Runner) -> BTreeMap<String, String> {
        GNOME_KEYS
            .iter()
            .filter_map(|(schema, key)| Self::get(runner, schema, key).map(|v| (format!("{schema} {key}"), v)))
            .collect()
    }

    fn apply(runner: &dyn Runner, settings: &ProxySettings) -> bool {
        let host = settings.host.as_str();
        let port = settings.port.to_string();
        let ok = ["http", "https", "socks"].iter().all(|kind| {
            let schema = format!("org.gnome.system.proxy.{kind}");
            Self::set(runner, &schema, "host", host) && Self::set(runner, &schema, "port", &port)
        });
        // Switch the mode last so the desktop never sees a half-written proxy.
        ok && Self::set(
            runner,
            "org.gnome.system.proxy",
            "ignore-hosts",
            &gvariant_string_array(&settings.bypass),
        ) && Self::set(runner, "org.gnome.system.proxy", "mode", "manual")
    }

    fn restore(runner: &dyn Runner, snapshot: &BTreeMap<String, String>) -> bool {
        if snapshot.is_empty() {
            return Self::set(runner, "org.gnome.system.proxy", "mode", "none");
        }
        // Mode last, mirroring `apply`.
        let mut ok = true;
        for (entry, value) in snapshot.iter().filter(|(entry, _)| !entry.ends_with(" mode")) {
            if let Some((schema, key)) = entry.split_once(' ') {
                ok &= Self::set(runner, schema, key, value);
            }
        }
        let mode = snapshot
            .get("org.gnome.system.proxy mode")
            .map_or("'none'", String::as_str);
        Self::set(runner, "org.gnome.system.proxy", "mode", mode) && ok
    }
}

struct Kde {
    write: &'static str,
    read: &'static str,
}

impl Kde {
    /// Plasma 6 ships `kwriteconfig6`; Plasma 5 `kwriteconfig5`.
    fn detect(runner: &dyn Runner) -> Option<Self> {
        [("kwriteconfig6", "kreadconfig6"), ("kwriteconfig5", "kreadconfig5")]
            .into_iter()
            .find(|(write, _)| runner.exists(write))
            .map(|(write, read)| Self { write, read })
    }

    fn get(&self, runner: &dyn Runner, key: &str) -> String {
        runner
            .output(self.read, &["--file", "kioslaverc", "--group", KDE_GROUP, "--key", key])
            .unwrap_or_default()
    }

    fn set(&self, runner: &dyn Runner, key: &str, value: &str) -> bool {
        if value.is_empty() {
            return runner.run(
                self.write,
                &["--file", "kioslaverc", "--group", KDE_GROUP, "--key", key, "--delete"],
            );
        }
        runner.run(
            self.write,
            &["--file", "kioslaverc", "--group", KDE_GROUP, "--key", key, value],
        )
    }

    /// kioslaverc stores `scheme://host port` (space before the port).
    fn endpoint(scheme: &str, settings: &ProxySettings) -> String {
        format!("{scheme}://{} {}", settings.host, settings.port)
    }

    fn is_ours(&self, runner: &dyn Runner, settings: &ProxySettings) -> bool {
        self.get(runner, "ProxyType") == "1" && self.get(runner, "httpProxy") == Self::endpoint("http", settings)
    }

    fn snapshot(&self, runner: &dyn Runner) -> BTreeMap<String, String> {
        KDE_KEYS
            .iter()
            .map(|key| (key.to_string(), self.get(runner, key)))
            .collect()
    }

    fn apply(&self, runner: &dyn Runner, settings: &ProxySettings) -> bool {
        let http = Self::endpoint("http", settings);
        let ok = self.set(runner, "httpProxy", &http)
            && self.set(runner, "httpsProxy", &http)
            && self.set(runner, "socksProxy", &Self::endpoint("socks", settings))
            && self.set(runner, "NoProxyFor", &settings.bypass.join(","))
            && self.set(runner, "ProxyType", "1");
        Self::notify(runner);
        ok
    }

    fn restore(&self, runner: &dyn Runner, snapshot: &BTreeMap<String, String>) -> bool {
        let ok = if snapshot.is_empty() {
            self.set(runner, "ProxyType", "0")
        } else {
            let mut ok = true;
            for key in KDE_KEYS.iter().filter(|key| **key != "ProxyType") {
                ok &= self.set(runner, key, snapshot.get(*key).map_or("", String::as_str));
            }
            let proxy_type = snapshot.get("ProxyType").map_or("0", String::as_str);
            let proxy_type = if proxy_type.is_empty() { "0" } else { proxy_type };
            self.set(runner, "ProxyType", proxy_type) && ok
        };
        Self::notify(runner);
        ok
    }

    /// Ask running KIO workers to re-read kioslaverc (best effort).
    fn notify(runner: &dyn Runner) {
        let _ = runner.run(
            "dbus-send",
            &[
                "--type=signal",
                "/KIO/Scheduler",
                "org.kde.KIO.Scheduler.reparseSlaveConfiguration",
                "string:",
            ],
        );
    }
}

const NO_BACKEND: &str = "no desktop proxy backend available (tried gsettings, kwriteconfig6 and kwriteconfig5)";

fn enable_with(runner: &dyn Runner, snapshot_path: &Path, settings: &ProxySettings) -> anyhow::Result<()> {
    let gnome = Gnome::available(runner);
    let kde = Kde::detect(runner);
    if !gnome && kde.is_none() {
        anyhow::bail!(NO_BACKEND);
    }

    // Keep the oldest snapshot: one left behind by a crash still describes
    // the desktop before we first touched it. Never snapshot our own values.
    // The file is always written: its presence marks the desktop proxy as
    // applied by us, which `release_with` requires.
    let mut snapshot = read_snapshot(snapshot_path);
    // "Ours" is the endpoint being applied or the one applied last time
    // (the config may have changed since).
    let mut ours = vec![settings.clone()];
    ours.extend(snapshot.applied.as_ref().map(Endpoint::as_settings));
    if gnome && snapshot.gnome.is_empty() && !ours.iter().any(|s| Gnome::is_ours(runner, s)) {
        snapshot.gnome = Gnome::snapshot(runner);
    }
    if let Some(kde) = &kde
        && snapshot.kde.is_empty()
        && !ours.iter().any(|s| kde.is_ours(runner, s))
    {
        snapshot.kde = kde.snapshot(runner);
    }
    snapshot.applied = Some(Endpoint::of(settings));
    write_snapshot(snapshot_path, &snapshot)?;

    let gnome_ok = gnome && Gnome::apply(runner, settings);
    let kde_ok = kde.as_ref().is_some_and(|kde| kde.apply(runner, settings));
    if gnome_ok || kde_ok {
        Ok(())
    } else {
        anyhow::bail!("failed to write the desktop proxy settings")
    }
}

fn disable_with(runner: &dyn Runner, snapshot_path: &Path) -> anyhow::Result<()> {
    let gnome = Gnome::available(runner);
    let kde = Kde::detect(runner);
    if !gnome && kde.is_none() {
        anyhow::bail!(NO_BACKEND);
    }
    let snapshot = read_snapshot(snapshot_path);
    let gnome_ok = gnome && Gnome::restore(runner, &snapshot.gnome);
    let kde_ok = kde.as_ref().is_some_and(|kde| kde.restore(runner, &snapshot.kde));
    let _ = std::fs::remove_file(snapshot_path);
    if gnome_ok || kde_ok {
        Ok(())
    } else {
        anyhow::bail!("failed to write the desktop proxy settings")
    }
}

/// Restore each backend that still points at the endpoint we applied
/// (`fallback` only for snapshots written before the endpoint was
/// recorded); leave backends the user has since re-pointed alone. Only acts
/// when we applied the proxy (the snapshot marker exists), so another client
/// on the same endpoint — e.g. the Clash Verge GUI on 127.0.0.1:7897 — is
/// never switched off. Returns whether anything was changed.
fn release_with(runner: &dyn Runner, snapshot_path: &Path, fallback: &ProxySettings) -> anyhow::Result<bool> {
    if !snapshot_path.exists() {
        return Ok(false);
    }
    let snapshot = read_snapshot(snapshot_path);
    let settings = snapshot
        .applied
        .as_ref()
        .map_or_else(|| fallback.clone(), Endpoint::as_settings);
    let gnome_ours = Gnome::available(runner) && Gnome::is_ours(runner, &settings);
    let kde = Kde::detect(runner).filter(|kde| kde.is_ours(runner, &settings));
    if !gnome_ours && kde.is_none() {
        return Ok(false);
    }
    let gnome_ok = !gnome_ours || Gnome::restore(runner, &snapshot.gnome);
    let kde_ok = kde.as_ref().is_none_or(|kde| kde.restore(runner, &snapshot.kde));
    let _ = std::fs::remove_file(snapshot_path);
    if gnome_ok && kde_ok {
        Ok(true)
    } else {
        anyhow::bail!("failed to restore the desktop proxy settings")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashMap;

    /// In-memory gsettings/kreadconfig store.
    #[derive(Default)]
    struct FakeDesktop {
        programs: Vec<&'static str>,
        gnome: RefCell<HashMap<String, String>>,
        kde: RefCell<HashMap<String, String>>,
    }

    impl FakeDesktop {
        fn gnome() -> Self {
            let desktop = Self {
                programs: vec!["gsettings"],
                ..Self::default()
            };
            for (schema, key) in GNOME_KEYS {
                let value = match *key {
                    "mode" => "'auto'",
                    "ignore-hosts" => "['corp.example']",
                    "host" => "''",
                    _ => "0",
                };
                desktop
                    .gnome
                    .borrow_mut()
                    .insert(format!("{schema} {key}"), value.into());
            }
            desktop
        }

        fn kde(write: &'static str) -> Self {
            let desktop = Self {
                programs: vec![write],
                ..Self::default()
            };
            desktop.kde.borrow_mut().insert("ProxyType".into(), "2".into());
            desktop
        }

        fn g(&self, key: &str) -> String {
            self.gnome.borrow().get(key).cloned().unwrap_or_default()
        }

        fn k(&self, key: &str) -> String {
            self.kde.borrow().get(key).cloned().unwrap_or_default()
        }
    }

    impl Runner for FakeDesktop {
        fn run(&self, program: &str, args: &[&str]) -> bool {
            match (program, args) {
                ("gsettings", ["set", schema, key, value]) if self.programs.contains(&"gsettings") => {
                    // gsettings echoes plain strings back as GVariant text.
                    let stored = if value.starts_with('\'') || value.starts_with('[') || value.parse::<u64>().is_ok() {
                        value.to_string()
                    } else {
                        gvariant_string(value)
                    };
                    self.gnome.borrow_mut().insert(format!("{schema} {key}"), stored);
                    true
                }
                (write, [.., "--key", key, "--delete"]) if self.programs.contains(&write) => {
                    self.kde.borrow_mut().remove(*key);
                    true
                }
                (write, [.., "--key", key, value]) if self.programs.contains(&write) => {
                    self.kde.borrow_mut().insert(key.to_string(), value.to_string());
                    true
                }
                ("dbus-send", _) => true,
                _ => false,
            }
        }

        fn output(&self, program: &str, args: &[&str]) -> Option<String> {
            match (program, args) {
                ("gsettings", ["get", schema, key]) if self.programs.contains(&"gsettings") => {
                    self.gnome.borrow().get(&format!("{schema} {key}")).cloned()
                }
                (read, [.., "--key", key]) if read.starts_with("kreadconfig") => Some(self.k(key)),
                _ => None,
            }
        }

        fn exists(&self, program: &str) -> bool {
            self.programs.contains(&program)
        }
    }

    fn settings() -> ProxySettings {
        ProxySettings {
            host: "127.0.0.1".into(),
            port: 7897,
            bypass: bypass_list(true, Some("*.lan")),
        }
    }

    fn snapshot_file(label: &str) -> PathBuf {
        std::env::temp_dir()
            .join(format!("cv-sysproxy-{label}-{}", uuid::Uuid::new_v4()))
            .join(SNAPSHOT_FILE)
    }

    #[test]
    fn bypass_list_merges_defaults_and_custom_entries() {
        let list = bypass_list(true, Some("*.lan; example.com ,, localhost"));
        assert_eq!(list.first().map(String::as_str), Some("localhost"));
        assert!(list.contains(&"192.168.0.0/16".to_string()));
        assert!(list.contains(&"*.lan".to_string()));
        assert!(list.contains(&"example.com".to_string()));
        assert_eq!(list.iter().filter(|e| *e == "localhost").count(), 1);

        assert_eq!(bypass_list(false, Some("a.com")), vec!["a.com".to_string()]);
        assert!(bypass_list(false, None).is_empty());
    }

    #[test]
    fn env_exports_cover_lower_and_upper_case_and_quote_values() {
        let out = env_exports(&settings());
        assert!(out.contains("export http_proxy='http://127.0.0.1:7897'\n"));
        assert!(out.contains("export HTTPS_PROXY='http://127.0.0.1:7897'\n"));
        assert!(out.contains("export all_proxy='socks5://127.0.0.1:7897'\n"));
        assert!(out.contains("export NO_PROXY='localhost,127.0.0.0/8,"));
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        assert!(env_unsets().starts_with("unset http_proxy HTTP_PROXY"));
    }

    #[test]
    fn gnome_apply_sets_bypass_socks_and_restores_the_previous_desktop() {
        let desktop = FakeDesktop::gnome();
        let path = snapshot_file("gnome");

        enable_with(&desktop, &path, &settings()).unwrap();
        assert_eq!(desktop.g("org.gnome.system.proxy mode"), "'manual'");
        assert_eq!(desktop.g("org.gnome.system.proxy.socks host"), "'127.0.0.1'");
        assert_eq!(desktop.g("org.gnome.system.proxy.https port"), "7897");
        assert!(desktop.g("org.gnome.system.proxy ignore-hosts").contains("'*.lan'"));
        assert!(path.exists(), "previous desktop settings must be snapshotted");

        // Re-applying (e.g. restart) must not overwrite the original snapshot
        // with our own values.
        enable_with(&desktop, &path, &settings()).unwrap();
        assert!(read_snapshot(&path).gnome["org.gnome.system.proxy mode"] == "'auto'");

        assert!(release_with(&desktop, &path, &settings()).unwrap());
        assert_eq!(desktop.g("org.gnome.system.proxy mode"), "'auto'");
        assert_eq!(desktop.g("org.gnome.system.proxy ignore-hosts"), "['corp.example']");
        assert!(!path.exists());
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn release_leaves_a_desktop_the_user_repointed_alone() {
        let desktop = FakeDesktop::gnome();
        let path = snapshot_file("repointed");
        enable_with(&desktop, &path, &settings()).unwrap();

        // User switched the desktop to another proxy while we ran.
        desktop
            .gnome
            .borrow_mut()
            .insert("org.gnome.system.proxy.http host".into(), "'10.0.0.2'".into());
        assert!(!release_with(&desktop, &path, &settings()).unwrap());
        assert_eq!(desktop.g("org.gnome.system.proxy mode"), "'manual'");
        assert_eq!(desktop.g("org.gnome.system.proxy.http host"), "'10.0.0.2'");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn release_uses_the_applied_endpoint_after_a_config_change() {
        let desktop = FakeDesktop::gnome();
        let path = snapshot_file("moved");
        enable_with(&desktop, &path, &settings()).unwrap();

        // mixed-port edited while the core runs: release is called with the
        // new config, but the desktop still carries the applied endpoint.
        let moved = ProxySettings {
            port: 7999,
            ..settings()
        };
        assert!(release_with(&desktop, &path, &moved).unwrap());
        assert_eq!(desktop.g("org.gnome.system.proxy mode"), "'auto'");

        // Re-applying with a new endpoint over the old applied one must not
        // snapshot our own old values.
        enable_with(&desktop, &path, &settings()).unwrap();
        std::fs::remove_file(&path).unwrap();
        enable_with(&desktop, &path, &settings()).unwrap();
        enable_with(&desktop, &path, &moved).unwrap();
        assert!(read_snapshot(&path).gnome.is_empty());
        assert_eq!(read_snapshot(&path).applied, Some(Endpoint::of(&moved)));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn release_ignores_a_proxy_this_tool_did_not_apply() {
        // Same endpoint, but set by someone else (no snapshot marker).
        let desktop = FakeDesktop::gnome();
        for (key, value) in [
            ("org.gnome.system.proxy mode", "'manual'"),
            ("org.gnome.system.proxy.http host", "'127.0.0.1'"),
            ("org.gnome.system.proxy.http port", "7897"),
        ] {
            desktop.gnome.borrow_mut().insert(key.into(), value.into());
        }
        let path = snapshot_file("foreign");
        assert!(!release_with(&desktop, &path, &settings()).unwrap());
        assert_eq!(desktop.g("org.gnome.system.proxy mode"), "'manual'");
    }

    #[test]
    fn applying_over_our_own_leftover_still_marks_ownership() {
        // A crash left our proxy in place and no snapshot: re-applying must
        // not snapshot our values, but release must still switch it off.
        let desktop = FakeDesktop::gnome();
        let path = snapshot_file("leftover");
        enable_with(&desktop, &path, &settings()).unwrap();
        std::fs::remove_file(&path).unwrap();

        enable_with(&desktop, &path, &settings()).unwrap();
        assert!(read_snapshot(&path).gnome.is_empty());
        assert!(release_with(&desktop, &path, &settings()).unwrap());
        assert_eq!(desktop.g("org.gnome.system.proxy mode"), "'none'");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn kde_prefers_plasma6_tools_and_uses_the_kioslaverc_format() {
        let desktop = FakeDesktop::kde("kwriteconfig6");
        let path = snapshot_file("kde6");

        enable_with(&desktop, &path, &settings()).unwrap();
        assert_eq!(desktop.k("ProxyType"), "1");
        assert_eq!(desktop.k("httpProxy"), "http://127.0.0.1 7897");
        assert_eq!(desktop.k("socksProxy"), "socks://127.0.0.1 7897");
        assert!(desktop.k("NoProxyFor").contains("*.lan"));

        assert!(release_with(&desktop, &path, &settings()).unwrap());
        assert_eq!(desktop.k("ProxyType"), "2", "previous proxy type restored");
        assert_eq!(desktop.k("httpProxy"), "", "keys absent before are deleted again");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn kde_falls_back_to_plasma5_tools() {
        let desktop = FakeDesktop::kde("kwriteconfig5");
        let path = snapshot_file("kde5");
        enable_with(&desktop, &path, &settings()).unwrap();
        assert_eq!(desktop.k("ProxyType"), "1");
        disable_with(&desktop, &path).unwrap();
        assert_eq!(desktop.k("ProxyType"), "2");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn disable_without_snapshot_switches_the_proxy_off() {
        let desktop = FakeDesktop::gnome();
        let path = snapshot_file("nosnap");
        disable_with(&desktop, &path).unwrap();
        assert_eq!(desktop.g("org.gnome.system.proxy mode"), "'none'");
    }

    #[test]
    fn no_backend_is_an_error_for_explicit_toggles_but_not_for_release() {
        let desktop = FakeDesktop::default();
        let path = snapshot_file("none");
        assert!(enable_with(&desktop, &path, &settings()).is_err());
        assert!(disable_with(&desktop, &path).is_err());
        assert!(!release_with(&desktop, &path, &settings()).unwrap());
    }
}
