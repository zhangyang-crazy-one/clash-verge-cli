use super::View;
use crate::mihomo_api::types::{ConnectionInfo, LogEntry, Rule, RuleProvider, TrafficData};

/// What config file to open in `$EDITOR`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditorTarget {
    Verge,
    #[allow(dead_code)]
    Dns,
}

#[derive(Debug, Clone)]
pub enum Action {
    StartCore,
    StopCore,
    RestartCore,
    CoreStarted {
        version: Option<String>,
        binary_path: Option<String>,
        /// system | cached | downloaded
        binary_source: Option<String>,
    },
    CoreExited(i32),
    CoreError(String),
    Quit,

    // Profile management
    StartImport,
    ConfirmImport(String),
    MoveNext,
    MovePrevious,
    Activate,
    UpdateProfile,
    ProfileImported,
    ProfileImportFailed(String),
    ProfileUpdated {
        uid: String,
        is_current: bool,
    },
    ProfileUpdateFailed(String),

    // Shell navigation
    SwitchView(View),
    CycleFocus,
    FocusMenu,
    FocusContent,
    StartFilter,
    SubmitFilter,
    ToggleHelp,
    DismissOverlay,

    // Proxy nodes
    ProxiesRefresh,
    ProxiesFetched(std::collections::HashMap<String, crate::mihomo_api::types::ProxyGroup>),
    ProxiesFailed(String),
    NodeDelayTest,
    NodeDelayAll,
    DelayResult(String, Option<u64>),
    DelayFailed(String, String),
    /// Batch-only result events. Kept distinct from the single-node
    /// `DelayResult`/`DelayFailed` so a single-node `t` result can never
    /// advance or clear the active batch progress/guard.
    BatchDelayResult(String, Option<u64>),
    BatchDelayFailed(String, String),

    // Chain proxy
    ToggleChainMode,
    ApplyChain,
    ClearChain,
    ChainApplied(Vec<String>),
    ChainFailed(String),

    // Runtime data. Requests and their result actions deliberately stay separate so the
    // event loop remains the only owner of Mihomo I/O while views render cached state.
    TrafficRefresh,
    TrafficFetched(TrafficData),
    TrafficFailed(String),
    ConnectionsRefresh,
    ConnectionsFetched(Vec<ConnectionInfo>),
    ConnectionsFailed(String),
    LogsRefresh,
    LogReceived(LogEntry),
    LogsFailed(String),

    // Connection close is a two-step flow. The event loop only sends the destructive
    // request after it has received ConfirmCloseConnection for the selected identity.
    RequestCloseConnection,
    ConfirmCloseConnection(String),
    ConnectionClosed(String),
    CloseConnectionFailed {
        id: String,
        error: String,
    },

    /// Close all connections via `DELETE /connections`.
    RequestCloseAllConnections,
    ConfirmCloseAllConnections,
    AllConnectionsClosed,
    CloseAllConnectionsFailed(String),

    /// Auto-update tick finished (clears in-flight guard).
    AutoUpdateFinished,
    CycleClashMode,
    ModeChanged {
        mode: String,
        announce: bool,
    },
    ModeChangeFailed(String),

    // Settings editor via $EDITOR
    /// Open the given config target in the user's editor.
    OpenEditor(EditorTarget),
    /// Editor session finished. `ok` is false if YAML validation failed and
    /// the pre-edit snapshot was restored.
    #[allow(dead_code)]
    EditorFinished {
        target: EditorTarget,
        ok: bool,
    },
    /// Reload settings from disk after an editor session.
    #[allow(dead_code)]
    ReloadSettings,

    // Rules
    RulesRefresh,
    RulesFetched(Vec<Rule>),
    RulesFailed(String),
    RuleProvidersRefresh,
    RuleProvidersFetched(Vec<RuleProvider>),
    RuleProvidersFailed(String),
    RuleProviderUpdated(String),
    RuleProviderUpdateFailed {
        name: String,
        error: String,
    },

    // Probe loop (dead-node detection → forced refresh / rollback notices).
    ProbeNotice(String),
    /// Read-only report of the resolved binary's TUN capability state
    /// (refresh after setup / start / startup probe).
    TunCapabilityState(bool),
    /// TUN capability was applied (explicit one-time sudo); settings shows
    /// (privileged).
    TunPrivilegeApplied,
    /// Password popup input (hidden buffer, `•` masked).
    PasswordChar(char),
    PasswordBackspace,
    PasswordSubmit,
    PasswordCancel,
    /// The user chose the explicit Settings → TUN setup action and the
    /// resolved binary needs capabilities; open the popup.
    TunSetupRequested(std::path::PathBuf),
}

const fn _assert_send_sync() {
    const fn assert<T: Send + Sync>() {}
    assert::<Action>();
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn test_action_send_sync() {
        // Compile-time check that Action is Send + Sync.
        assert_send_sync::<Action>();
    }

    #[test]
    fn test_action_variants_construct() {
        // Construct every variant to ensure the enum surface is complete.
        let _ = Action::StartCore;
        let _ = Action::StopCore;
        let _ = Action::RestartCore;
        let _ = Action::CoreStarted {
            version: None,
            binary_path: None,
            binary_source: None,
        };
        let _ = Action::CoreExited(0);
        let _ = Action::CoreExited(137);
        let _ = Action::CoreError("boom".to_string());
        let _ = Action::Quit;
    }

    fn assert_send_sync<T: Send + Sync>() {}
}
