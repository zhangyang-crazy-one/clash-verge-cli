pub mod action;
pub mod state;

pub use action::Action;
pub use state::CoreState;

use crate::i18n::Language;
use crate::mihomo_api::types::{ConnectionInfo, LogEntry, ProxyGroup, TrafficData};
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
    pub selected_index: usize,
    pub input_mode: InputMode,
    pub status_msg: Option<String>,
    pub view: View,
    pub focus: Focus,
    pub overlay: Option<Overlay>,
    pub filter: Option<String>,
    pub traffic: Option<TrafficData>,
    pub connections: Vec<ConnectionInfo>,
    pub logs: Vec<LogEntry>,
    pub selected_connection_id: Option<String>,
    pub connection_selected_index: usize,
    pub log_selected_index: usize,
    pub pending_connection_close: Option<String>,
    pub connection_filter: Option<String>,
    pub log_filter: Option<String>,
    pub runtime_loading: RuntimeLoading,
    pub runtime_errors: RuntimeErrors,

    // Proxy node state
    pub proxy_groups: HashMap<String, ProxyGroup>,
    pub expanded_proxy_group: Option<String>,
    pub node_selected_index: usize,
    pub delay_map: HashMap<String, Option<u64>>,
    // Chain proxy state
    pub chain_mode: bool,
    pub chain_nodes: Vec<String>,
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
            selected_index: 0,
            input_mode: InputMode::Normal,
            status_msg: None,
            view: View::Home,
            focus: Focus::Menu,
            overlay: None,
            filter: None,
            traffic: None,
            connections: Vec::new(),
            logs: Vec::new(),
            selected_connection_id: None,
            connection_selected_index: 0,
            log_selected_index: 0,
            pending_connection_close: None,
            connection_filter: None,
            log_filter: None,
            runtime_loading: RuntimeLoading::default(),
            runtime_errors: RuntimeErrors::default(),
            proxy_groups: HashMap::new(),
            expanded_proxy_group: None,
            node_selected_index: 0,
            delay_map: HashMap::new(),
            chain_mode: false,
            chain_nodes: Vec::new(),
        }
    }

    pub fn clear_runtime_caches(&mut self) {
        self.traffic = None;
        self.connections.clear();
        self.logs.clear();
        self.selected_connection_id = None;
        self.connection_selected_index = 0;
        self.log_selected_index = 0;
        self.pending_connection_close = None;
        self.runtime_loading = RuntimeLoading::default();
        self.runtime_errors = RuntimeErrors::default();
    }

    pub fn tr(&self, key: &'static str) -> &'static str {
        crate::i18n::tr(self.language, key)
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

/// Convert the API's unordered proxy map into the single stable sequence used by
/// rendering and cursor-driven actions. The top portion contains selectable
/// groups; only the currently expanded group contributes node rows. This avoids
/// rendering the same concrete node repeatedly for every selector that contains
/// it while keeping all group choices reachable from the keyboard.
pub fn proxy_display_rows(groups: &HashMap<String, ProxyGroup>, expanded_group: Option<&str>) -> Vec<ProxyDisplayRow> {
    let mut ordered_groups: Vec<_> = groups
        .iter()
        .filter(|(_, group)| group.all.as_ref().is_some_and(|nodes| !nodes.is_empty()))
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
        && let Some(mut nodes) = group.all.clone().filter(|nodes| !nodes.is_empty())
    {
        nodes.sort_unstable();
        let current = group.now.as_deref();
        rows.extend(nodes.into_iter().map(|name| ProxyDisplayRow::Node {
            current: current == Some(name.as_str()),
            group: expanded_group.to_string(),
            name,
        }));
    }
    rows
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

        let rows = proxy_display_rows(&groups, Some("zeta"));
        let labels: Vec<_> = rows
            .iter()
            .map(|row| match row {
                ProxyDisplayRow::Group { name, .. } => format!("group:{name}"),
                ProxyDisplayRow::Node { group, name, .. } => format!("node:{group}:{name}"),
            })
            .collect();

        assert_eq!(
            labels,
            vec!["group:alpha", "group:zeta", "node:zeta:alpha", "node:zeta:beta",]
        );
    }
}
