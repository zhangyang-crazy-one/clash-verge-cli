pub mod action;
pub mod filter;
pub mod state;

pub use action::{Action, EditorTarget};
pub use state::CoreState;

use crate::i18n::Language;
use crate::mihomo_api::types::{ConnectionInfo, LogEntry, ProxyGroup, Rule, RuleProvider, TrafficData};
use clash_verge_core::config::{IClashTemp, IVerge, PrfItem};
use std::collections::HashMap;

pub enum InputMode {
    Normal,
    Importing(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    Home,
    Proxies,
    Profiles,
    Connections,
    Rules,
    Logs,
    Unlock,
    Settings,
}

impl View {
    pub const ALL: [Self; 8] = [
        Self::Home,
        Self::Proxies,
        Self::Profiles,
        Self::Connections,
        Self::Rules,
        Self::Logs,
        Self::Unlock,
        Self::Settings,
    ];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Home => "Home",
            Self::Proxies => "Proxies",
            Self::Profiles => "Profiles",
            Self::Connections => "Connections",
            Self::Rules => "Rules",
            Self::Logs => "Logs",
            Self::Unlock => "Unlock",
            Self::Settings => "Settings",
        }
    }

    pub fn localized_label(self, language: Language) -> &'static str {
        crate::i18n::tr(
            language,
            match self {
                Self::Home => "view.home",
                Self::Proxies => "view.proxies",
                Self::Profiles => "view.profiles",
                Self::Connections => "view.connections",
                Self::Rules => "view.rules",
                Self::Logs => "view.logs",
                Self::Unlock => "view.unlock",
                Self::Settings => "view.settings",
            },
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Menu,
    Content,
}

impl Focus {
    pub const fn cycle(self) -> Self {
        match self {
            Self::Menu => Self::Content,
            Self::Content => Self::Menu,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Overlay {
    Help,
    Filter,
    CloseConfirmation,
    CloseAllConnectionsConfirmation,
    /// Bordered password popup for one-time TUN capability setup.
    PasswordInput,
    /// SSRF safety check blocked the imported URL host; explicit `y` required
    /// before retrying that import with the host in `trusted_hosts`.
    TrustConfirmation,
    /// Core start with TUN enabled needs the one-time setup (file capability
    /// and/or the systemd-resolved DNS polkit rule); explicit `y` opens the
    /// password popup, `n`/Esc/q starts without setup.
    TunSetupConfirmation,
}

/// Pending SSRF trust confirmation for a subscription import or refresh.
///
/// `host` is the bare host that the safety check rejected; only this host is
/// offered for `trusted_hosts`. No profile data is written until the user
/// confirms via `y`.
///
/// `uid` is `None` for the import flow and `Some(profile uid)` for a manual
/// refresh of an existing profile: the update flow must carry the uid so that
/// confirming persists the trusted host into that profile's stored option.
#[derive(Debug, Clone)]
pub struct TrustPending {
    pub url: String,
    pub host: String,
    /// Profile uid for the refresh flow (`None` = import flow).
    pub uid: Option<String>,
}

/// Why the core-start setup confirm was offered. Decides what `n`/Esc/q
/// means on the dialog: dismissing a capability-missing prompt must cancel
/// the start (the spawn preflight would hard-fail anyway), while dismissing
/// a missing-DNS-rule prompt may start without setup (that path works, it
/// just triggers system polkit dialogs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TunSetupReason {
    /// The resolved binary lacks the TUN file capability (and the process
    /// is not root): hard gate — setup is required before the core can run
    /// with TUN enabled.
    MissingCapability,
    /// Capability is fine; only the systemd-resolved DNS polkit rule is
    /// missing: soft gate — the core can still start, it will just show
    /// system auth dialogs until the rule is installed.
    MissingDnsRule,
}

/// Context for a TUN setup waiting on password input (or the inline confirm
/// dialog on core start).
#[derive(Debug, Clone)]
pub struct TunPending {
    pub binary: std::path::PathBuf,
    /// Resume a pending core start after the transaction succeeds:
    /// `Some(enable_tun)` when the setup was offered from the core-start
    /// prompt, `None` for the explicit Settings → TUN setup action (nothing
    /// to resume).
    pub resume_start: Option<bool>,
    /// Which gate prompted the setup; only meaningful for the core-start
    /// confirm (the explicit Settings flow has nothing to skip).
    pub reason: TunSetupReason,
}

#[derive(Debug, Default)]
pub struct UnlockState {
    pub running: bool,
    pub report: Option<crate::services::unlock::Report>,
    pub checked_at: Option<chrono::DateTime<chrono::Local>>,
    pub error: Option<String>,
}

#[derive(Debug, Default)]
pub struct RuntimeLoading {
    pub proxies: bool,
    pub traffic: bool,
    pub connections: bool,
    pub logs: bool,
}

#[derive(Debug, Default)]
pub struct RuntimeErrors {
    pub proxies: Option<String>,
    pub traffic: Option<String>,
    pub connections: Option<String>,
    pub logs: Option<String>,
}

/// Application state shared between TUI components.
pub struct App {
    pub core_state: CoreState,
    pub core_version: Option<String>,
    pub core_pid: Option<u32>,
    pub gui_config: IVerge,
    pub core_config: IClashTemp,
    pub language: Language,

    // Profile state
    pub profiles: Vec<PrfItem>,
    /// uid of the profile in use (the Profiles cursor may be elsewhere).
    pub current_profile_uid: Option<String>,
    pub selected_index: usize,
    pub input_mode: InputMode,
    pub status_msg: Option<String>,
    /// A problem with `tui.yaml`, kept on Home (status messages get replaced).
    pub config_warning: Option<String>,
    pub view: View,
    pub focus: Focus,
    pub overlay: Option<Overlay>,
    /// Subscription URL or profile refresh waiting on an explicit SSRF trust
    /// confirmation (import when `uid` is `None`, manual update otherwise).
    pub pending_trust: Option<TrustPending>,
    pub filter: Option<String>,
    pub traffic: Option<TrafficData>,
    /// (upload, download) bytes since the core started.
    pub traffic_totals: Option<(u64, u64)>,
    pub connections: Vec<ConnectionInfo>,
    pub logs: Vec<LogEntry>,
    pub selected_connection_id: Option<String>,
    pub connection_selected_index: usize,
    pub log_selected_index: usize,
    pub pending_connection_close: Option<String>,
    pub connection_filter: Option<String>,
    pub log_filter: Option<String>,
    pub proxy_filter: Option<String>,
    pub profile_filter: Option<String>,
    pub rule_filter: Option<String>,
    pub runtime_loading: RuntimeLoading,
    pub runtime_errors: RuntimeErrors,
    /// Level of the core's logs and of the Logs view's stream (`L` cycles it).
    pub log_level: String,
    /// The running log stream, aborted when the level changes.
    pub log_stream: Option<tokio::task::AbortHandle>,

    // Proxy node state
    pub proxy_groups: HashMap<String, ProxyGroup>,
    pub expanded_proxy_group: Option<String>,
    pub node_selected_index: usize,
    pub delay_map: HashMap<String, Option<u64>>,
    /// Node order in the expanded group (`o` cycles it).
    pub proxy_sort: ProxySort,
    /// Hide nodes whose last delay test failed (`H`).
    pub hide_failed_proxies: bool,
    /// Progress of the active batch delay test: (completed, total).
    /// `None` while no batch is running (also used to reject duplicate starts).
    pub batch_delay: Option<(usize, usize)>,
    // Chain proxy state
    pub chain_mode: bool,
    pub chain_nodes: Vec<String>,

    /// Unlock view: the last report, and whether a run is in progress.
    pub unlock: UnlockState,

    /// Settings list cursor (language / system proxy / TUN / mode).
    pub settings_selected_index: usize,
    /// Last known clash mode from mihomo or saved config (tolerant string).
    pub clash_mode: String,

    // Rules state
    pub rules: Vec<Rule>,
    pub rule_providers: Vec<RuleProvider>,
    pub rules_loading: bool,
    pub rules_error: Option<String>,
    pub rule_providers_loading: bool,
    pub rule_providers_error: Option<String>,
    /// Tab between Rules and Providers panels.
    pub rules_focus_providers: bool,
    pub rules_selected_index: usize,
    /// Whether the mihomo binary carries TUN capabilities (set after the
    /// one-time askpass setup).
    pub tun_privileged: bool,
    /// Hidden password buffer for the `PasswordInput` overlay.
    pub password_buffer: Vec<char>,
    /// Prompt label shown in the password popup.
    pub password_prompt: Option<String>,
    /// TUN-enable action waiting on password input.
    pub pending_tun: Option<TunPending>,
}

/// Log levels `L` cycles through, most verbose first.
pub const LOG_LEVELS: [&str; 4] = ["debug", "info", "warning", "error"];

/// The level after `current` in [`LOG_LEVELS`] (`info` for an unknown one).
pub fn next_log_level(current: &str) -> &'static str {
    LOG_LEVELS
        .iter()
        .position(|level| *level == current)
        .map_or("info", |index| LOG_LEVELS[(index + 1) % LOG_LEVELS.len()])
}

impl App {
    pub fn new() -> Self {
        Self {
            core_state: CoreState::Stopped,
            core_version: None,
            core_pid: None,
            gui_config: IVerge::default(),
            core_config: IClashTemp::default(),
            language: Language::English,
            profiles: Vec::new(),
            current_profile_uid: None,
            selected_index: 0,
            input_mode: InputMode::Normal,
            status_msg: None,
            config_warning: None,
            view: View::Home,
            focus: Focus::Menu,
            overlay: None,
            pending_trust: None,
            filter: None,
            traffic: None,
            traffic_totals: None,
            connections: Vec::new(),
            logs: Vec::new(),
            selected_connection_id: None,
            connection_selected_index: 0,
            log_selected_index: 0,
            pending_connection_close: None,
            connection_filter: None,
            log_filter: None,
            proxy_filter: None,
            profile_filter: None,
            rule_filter: None,
            runtime_loading: RuntimeLoading::default(),
            runtime_errors: RuntimeErrors::default(),
            log_level: "info".into(),
            log_stream: None,
            proxy_groups: HashMap::new(),
            expanded_proxy_group: None,
            node_selected_index: 0,
            delay_map: HashMap::new(),
            proxy_sort: ProxySort::default(),
            hide_failed_proxies: false,
            batch_delay: None,
            chain_mode: false,
            chain_nodes: Vec::new(),
            unlock: UnlockState::default(),
            settings_selected_index: 0,
            clash_mode: "rule".into(),
            rules: Vec::new(),
            rule_providers: Vec::new(),
            rules_loading: false,
            rules_error: None,
            rule_providers_loading: false,
            rule_providers_error: None,
            rules_focus_providers: false,
            rules_selected_index: 0,
            tun_privileged: false,
            password_buffer: Vec::new(),
            password_prompt: None,
            pending_tun: None,
        }
    }

    pub fn clear_runtime_caches(&mut self) {
        self.traffic = None;
        self.traffic_totals = None;
        self.connections.clear();
        self.logs.clear();
        self.selected_connection_id = None;
        self.connection_selected_index = 0;
        self.log_selected_index = 0;
        self.pending_connection_close = None;
        self.runtime_loading = RuntimeLoading::default();
        self.runtime_errors = RuntimeErrors::default();
    }

    /// The log level the core starts with: `log-level` from the runtime
    /// config (the `L` key only changes it until the core restarts).
    pub fn configured_log_level(&self) -> String {
        self.core_config
            .0
            .get("log-level")
            .and_then(|level| level.as_str())
            .filter(|level| LOG_LEVELS.contains(level))
            .unwrap_or("info")
            .to_string()
    }

    /// Take the profile list and the current profile from the store.
    pub fn load_profiles(&mut self, store: &crate::profile_store::store::ProfileStore) {
        self.profiles = store.items();
        self.current_profile_uid = store.current_uid().map(|uid| uid.to_string());
    }

    /// The profile in use, if it is known.
    pub fn current_profile(&self) -> Option<&PrfItem> {
        let uid = self.current_profile_uid.as_deref()?;
        self.profiles.iter().find(|profile| profile.uid.as_deref() == Some(uid))
    }

    /// Where traffic goes by default; see [`outbound_chain`].
    pub fn outbound_chain(&self) -> Vec<String> {
        outbound_chain(&self.proxy_groups, &self.clash_mode)
    }

    pub fn tr(&self, key: &'static str) -> &'static str {
        crate::i18n::tr(self.language, key)
    }

    /// Choice hint for the core-start TUN setup confirm dialog: the two
    /// cases differ in what `n`/Esc/q does. Missing capability → dismissing
    /// cancels the start; only the DNS rule missing → dismissing starts
    /// without setup.
    pub fn tun_setup_confirm_hint(&self) -> &'static str {
        if self
            .pending_tun
            .as_ref()
            .is_some_and(|pending| pending.reason == TunSetupReason::MissingCapability)
        {
            self.tr("dialog.tun_setup_confirm_hard")
        } else {
            self.tr("dialog.tun_setup_confirm")
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProxyDisplayRow {
    Group {
        name: String,
        group_type: String,
        current: Option<String>,
        node_count: usize,
    },
    Node {
        group: String,
        name: String,
        current: bool,
    },
}

impl ProxyDisplayRow {
    pub fn node_identity(&self) -> Option<(&str, &str)> {
        match self {
            Self::Node { group, name, .. } => Some((group, name)),
            Self::Group { .. } => None,
        }
    }
}

/// Return a stable initial group, preferring the GUI's root `GLOBAL` selector.
pub fn first_selectable_proxy_group(groups: &HashMap<String, ProxyGroup>) -> Option<String> {
    let mut names: Vec<_> = groups
        .iter()
        .filter(|(_, group)| group.all.as_ref().is_some_and(|nodes| !nodes.is_empty()))
        .map(|(name, _)| name.clone())
        .collect();
    names.sort_unstable_by(|left, right| match (left.as_str(), right.as_str()) {
        ("GLOBAL", "GLOBAL") => std::cmp::Ordering::Equal,
        ("GLOBAL", _) => std::cmp::Ordering::Less,
        (_, "GLOBAL") => std::cmp::Ordering::Greater,
        _ => left.cmp(right),
    });
    names.into_iter().next()
}

/// Where traffic goes by default: the group the mode starts from, then
/// each group's selection down to a real node, e.g.
/// `["Proxy", "Auto", "Tokyo 01"]`. `["DIRECT"]` in direct mode; empty
/// before the proxies are loaded.
pub fn outbound_chain(groups: &HashMap<String, ProxyGroup>, mode: &str) -> Vec<String> {
    if mode.eq_ignore_ascii_case("direct") {
        return vec!["DIRECT".into()];
    }
    let is_group = |name: &str| {
        groups
            .get(name)
            .is_some_and(|group| group.all.as_ref().is_some_and(|nodes| !nodes.is_empty()))
    };
    // Rule mode: the first group GLOBAL lists (the profile's main
    // selector), as the GUI shows it.
    let start = if mode.eq_ignore_ascii_case("global") {
        is_group("GLOBAL").then(|| "GLOBAL".to_string())
    } else {
        groups
            .get("GLOBAL")
            .and_then(|global| global.all.as_ref())
            .and_then(|names| names.iter().find(|name| is_group(name)).cloned())
    };
    let Some(start) = start else {
        return Vec::new();
    };
    let mut chain = vec![start];
    // Bounded: a misconfigured profile could make groups select each other.
    while chain.len() < 8 {
        let Some(next) = chain
            .last()
            .and_then(|name| groups.get(name))
            .and_then(|group| group.now.clone())
        else {
            break;
        };
        if chain.contains(&next) {
            break;
        }
        let done = !is_group(&next);
        chain.push(next);
        if done {
            break;
        }
    }
    chain
}

/// Order of the nodes in the expanded proxy group.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ProxySort {
    /// The order of the group in the profile (what mihomo reports).
    #[default]
    Config,
    /// Fastest first; untested, then failed nodes last.
    Delay,
    Name,
}

impl ProxySort {
    pub const fn next(self) -> Self {
        match self {
            Self::Config => Self::Delay,
            Self::Delay => Self::Name,
            Self::Name => Self::Config,
        }
    }

    pub const fn label_key(self) -> &'static str {
        match self {
            Self::Config => "proxies.sort_config",
            Self::Delay => "proxies.sort_delay",
            Self::Name => "proxies.sort_name",
        }
    }
}

/// How the proxy list is narrowed and ordered.
#[derive(Debug, Clone, Copy, Default)]
pub struct ProxyListOptions<'a> {
    pub filter: Option<&'a str>,
    pub sort: ProxySort,
    /// Drop nodes whose last delay test failed (the selected node stays).
    pub hide_failed: bool,
    pub delays: Option<&'a HashMap<String, Option<u64>>>,
}

impl ProxyListOptions<'_> {
    fn delay(&self, node: &str) -> Option<&Option<u64>> {
        self.delays.and_then(|delays| delays.get(node))
    }

    fn failed(&self, node: &str) -> bool {
        matches!(self.delay(node), Some(None))
    }
}

/// Convert the API's unordered proxy map into the single stable sequence used by
/// rendering and cursor-driven actions. The top portion contains selectable
/// groups; only the currently expanded group contributes node rows. This avoids
/// rendering the same concrete node repeatedly for every selector that contains
/// it while keeping all group choices reachable from the keyboard.
///
/// With a filter, a group is listed when its name or one of its nodes
/// matches (the expanded group always is), and only matching nodes are
/// listed.
pub fn proxy_display_rows(
    groups: &HashMap<String, ProxyGroup>,
    expanded_group: Option<&str>,
    options: &ProxyListOptions<'_>,
) -> Vec<ProxyDisplayRow> {
    let filter = filter::active(options.filter);
    let node_matches = |group: &str, node: &str| filter.is_none_or(|query| filter::matches(query, &[group, node]));

    let mut ordered_groups: Vec<_> = groups
        .iter()
        .filter(|(_, group)| group.all.as_ref().is_some_and(|nodes| !nodes.is_empty()))
        .filter(|(name, group)| {
            filter.is_none()
                || Some(name.as_str()) == expanded_group
                || group.all.iter().flatten().any(|node| node_matches(name.as_str(), node))
        })
        .collect();
    ordered_groups.sort_unstable_by(|(left, _), (right, _)| match (left.as_str(), right.as_str()) {
        ("GLOBAL", "GLOBAL") => std::cmp::Ordering::Equal,
        ("GLOBAL", _) => std::cmp::Ordering::Less,
        (_, "GLOBAL") => std::cmp::Ordering::Greater,
        _ => left.cmp(right),
    });

    let mut rows = Vec::new();
    for (group_name, group) in ordered_groups {
        let current = group.now.clone();
        rows.push(ProxyDisplayRow::Group {
            name: group_name.clone(),
            group_type: group.group_type.clone(),
            current: current.clone(),
            node_count: group.all.as_ref().map_or(0, Vec::len),
        });
    }

    if let Some(expanded_group) = expanded_group
        && let Some(group) = groups.get(expanded_group)
        && let Some(nodes) = group.all.as_ref().filter(|nodes| !nodes.is_empty())
    {
        let current = group.now.as_deref();
        let mut nodes: Vec<&String> = nodes
            .iter()
            .filter(|node| node_matches(expanded_group, node))
            .filter(|node| !options.hide_failed || current == Some(node.as_str()) || !options.failed(node))
            .collect();
        match options.sort {
            ProxySort::Config => {}
            ProxySort::Name => nodes.sort(),
            // Stable: equal delays keep the profile order.
            ProxySort::Delay => nodes.sort_by_key(|node| match options.delay(node) {
                Some(Some(delay)) => (0, *delay),
                None => (1, 0),
                Some(None) => (2, 0),
            }),
        }
        rows.extend(nodes.into_iter().map(|name| ProxyDisplayRow::Node {
            current: current == Some(name.as_str()),
            group: expanded_group.to_string(),
            name: name.clone(),
        }));
    }
    rows
}

impl App {
    pub fn proxy_list_options(&self) -> ProxyListOptions<'_> {
        ProxyListOptions {
            filter: self.proxy_filter.as_deref(),
            sort: self.proxy_sort,
            hide_failed: self.hide_failed_proxies,
            delays: Some(&self.delay_map),
        }
    }

    /// The Proxies view's rows, as rendered and as the cursor indexes them.
    pub fn proxy_rows(&self) -> Vec<ProxyDisplayRow> {
        proxy_display_rows(
            &self.proxy_groups,
            self.expanded_proxy_group.as_deref(),
            &self.proxy_list_options(),
        )
    }

    /// Indices into `profiles` that the profile filter keeps.
    pub fn visible_profile_indices(&self) -> Vec<usize> {
        let filter = filter::active(self.profile_filter.as_deref());
        self.profiles
            .iter()
            .enumerate()
            .filter(|(_, profile)| {
                filter.is_none_or(|query| {
                    filter::matches(
                        query,
                        &[
                            profile.name.as_deref().unwrap_or_default(),
                            profile.uid.as_deref().unwrap_or_default(),
                            profile.desc.as_deref().unwrap_or_default(),
                        ],
                    )
                })
            })
            .map(|(index, _)| index)
            .collect()
    }

    pub fn visible_rules(&self) -> Vec<&Rule> {
        let filter = filter::active(self.rule_filter.as_deref());
        self.rules
            .iter()
            .filter(|rule| {
                filter.is_none_or(|query| filter::matches(query, &[&rule.rule_type, &rule.payload, &rule.proxy]))
            })
            .collect()
    }

    pub fn visible_rule_providers(&self) -> Vec<&RuleProvider> {
        let filter = filter::active(self.rule_filter.as_deref());
        self.rule_providers
            .iter()
            .filter(|provider| filter.is_none_or(|query| filter::matches(query, &[&provider.name, &provider.behavior])))
            .collect()
    }

    /// Items in the focused Rules panel (rules or providers).
    pub fn visible_rules_panel_len(&self) -> usize {
        if self.rules_focus_providers {
            self.visible_rule_providers().len()
        } else {
            self.visible_rules().len()
        }
    }

    pub fn visible_connections(&self) -> Vec<&ConnectionInfo> {
        let filter = filter::active(self.connection_filter.as_deref());
        self.connections
            .iter()
            .filter(|connection| {
                filter.is_none_or(|query| {
                    let host = connection
                        .metadata
                        .as_ref()
                        .and_then(|metadata| metadata.host.as_deref())
                        .unwrap_or_default();
                    let rule = connection.rule.as_deref().unwrap_or_default();
                    filter::matches(query, &[&connection.id, host, rule])
                })
            })
            .collect()
    }

    pub fn visible_logs(&self) -> Vec<&LogEntry> {
        let filter = filter::active(self.log_filter.as_deref());
        self.logs
            .iter()
            .filter(|entry| filter.is_none_or(|query| filter::matches(query, &[&entry.level, &entry.payload])))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proxy_display_rows_are_sorted_for_stable_selection() {
        let mut groups = HashMap::new();
        groups.insert(
            "zeta".to_string(),
            ProxyGroup {
                group_type: "Selector".to_string(),
                now: Some("beta".to_string()),
                all: Some(vec!["beta".to_string(), "alpha".to_string()]),
                history: None,
            },
        );
        groups.insert(
            "alpha".to_string(),
            ProxyGroup {
                group_type: "Selector".to_string(),
                now: None,
                all: Some(vec!["node".to_string()]),
                history: None,
            },
        );
        groups.insert(
            "direct-node".to_string(),
            ProxyGroup {
                group_type: "AnyTLS".to_string(),
                now: None,
                all: None,
                history: None,
            },
        );
        groups.insert(
            "DIRECT".to_string(),
            ProxyGroup {
                group_type: "Direct".to_string(),
                now: None,
                all: Some(Vec::new()),
                history: None,
            },
        );

        let rows = proxy_display_rows(&groups, Some("zeta"), &ProxyListOptions::default());
        let labels: Vec<_> = rows
            .iter()
            .map(|row| match row {
                ProxyDisplayRow::Group { name, .. } => format!("group:{name}"),
                ProxyDisplayRow::Node { group, name, .. } => format!("node:{group}:{name}"),
            })
            .collect();

        // Groups by name (GLOBAL first); nodes in the group's profile order.
        assert_eq!(
            labels,
            vec!["group:alpha", "group:zeta", "node:zeta:beta", "node:zeta:alpha",]
        );
    }

    fn group(now: &str, nodes: &[&str]) -> ProxyGroup {
        ProxyGroup {
            group_type: "Selector".to_string(),
            now: Some(now.to_string()),
            all: Some(nodes.iter().map(|node| (*node).to_string()).collect()),
            history: None,
        }
    }

    fn node_names(rows: &[ProxyDisplayRow]) -> Vec<&str> {
        rows.iter()
            .filter_map(|row| row.node_identity().map(|(_, name)| name))
            .collect()
    }

    fn group_names(rows: &[ProxyDisplayRow]) -> Vec<&str> {
        rows.iter()
            .filter_map(|row| match row {
                ProxyDisplayRow::Group { name, .. } => Some(name.as_str()),
                ProxyDisplayRow::Node { .. } => None,
            })
            .collect()
    }

    #[test]
    fn nodes_sort_by_delay_or_name_and_failed_ones_can_be_hidden() {
        let groups = HashMap::from([(
            "Proxy".to_string(),
            group("dead", &["slow", "dead", "fast", "untested", "broken"]),
        )]);
        let delays = HashMap::from([
            ("slow".to_string(), Some(300)),
            ("fast".to_string(), Some(40)),
            ("dead".to_string(), None),
            ("broken".to_string(), None),
        ]);
        let rows = |sort, hide_failed| {
            let options = ProxyListOptions {
                sort,
                hide_failed,
                delays: Some(&delays),
                ..ProxyListOptions::default()
            };
            proxy_display_rows(&groups, Some("Proxy"), &options)
        };

        assert_eq!(
            node_names(&rows(ProxySort::Config, false)),
            ["slow", "dead", "fast", "untested", "broken"]
        );
        assert_eq!(
            node_names(&rows(ProxySort::Delay, false)),
            ["fast", "slow", "untested", "dead", "broken"]
        );
        assert_eq!(
            node_names(&rows(ProxySort::Name, false)),
            ["broken", "dead", "fast", "slow", "untested"]
        );
        // The selected node stays visible even though its test failed.
        assert_eq!(
            node_names(&rows(ProxySort::Config, true)),
            ["slow", "dead", "fast", "untested"]
        );
        assert_eq!(ProxySort::Config.next().next().next(), ProxySort::Config);
    }

    #[test]
    fn the_filter_keeps_matching_nodes_and_groups_that_contain_them() {
        let groups = HashMap::from([
            (
                "Proxy".to_string(),
                group("HK 01", &["HK 01", "JP Tokyo 01", "JP Osaka"]),
            ),
            ("Streaming".to_string(), group("US", &["US"])),
            ("Asia".to_string(), group("JP Osaka", &["JP Osaka"])),
        ]);
        let options = ProxyListOptions {
            filter: Some("jp"),
            ..ProxyListOptions::default()
        };
        let rows = proxy_display_rows(&groups, Some("Proxy"), &options);
        assert_eq!(group_names(&rows), ["Asia", "Proxy"]);
        assert_eq!(node_names(&rows), ["JP Tokyo 01", "JP Osaka"]);

        // The expanded group stays listed even when nothing in it matches.
        let options = ProxyListOptions {
            filter: Some("us"),
            ..ProxyListOptions::default()
        };
        let rows = proxy_display_rows(&groups, Some("Proxy"), &options);
        assert_eq!(group_names(&rows), ["Proxy", "Streaming"]);
        assert!(node_names(&rows).is_empty());
    }

    #[test]
    fn outbound_chain_follows_selections_from_the_mode_start_group() {
        let mut app = App::new();
        app.proxy_groups = HashMap::from([
            ("GLOBAL".to_string(), group("Proxy", &["DIRECT", "Proxy", "Auto"])),
            ("Proxy".to_string(), group("Auto", &["Auto", "HK"])),
            ("Auto".to_string(), group("HK", &["HK", "JP"])),
            (
                "DIRECT".to_string(),
                ProxyGroup {
                    group_type: "Direct".to_string(),
                    now: None,
                    all: None,
                    history: None,
                },
            ),
        ]);

        app.clash_mode = "rule".into();
        assert_eq!(app.outbound_chain(), ["Proxy", "Auto", "HK"]);
        app.clash_mode = "global".into();
        assert_eq!(app.outbound_chain(), ["GLOBAL", "Proxy", "Auto", "HK"]);
        app.clash_mode = "direct".into();
        assert_eq!(app.outbound_chain(), ["DIRECT"]);

        // Groups selecting each other must not loop.
        app.clash_mode = "rule".into();
        app.proxy_groups.insert("Auto".to_string(), group("Proxy", &["Proxy"]));
        assert_eq!(app.outbound_chain(), ["Proxy", "Auto"]);

        app.proxy_groups.clear();
        assert!(app.outbound_chain().is_empty());
    }

    #[test]
    fn log_levels_cycle_from_verbose_to_quiet() {
        assert_eq!(next_log_level("debug"), "info");
        assert_eq!(next_log_level("info"), "warning");
        assert_eq!(next_log_level("error"), "debug");
        assert_eq!(next_log_level("silent"), "info");
    }
}
