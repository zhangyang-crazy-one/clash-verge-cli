use super::View;
use crate::mihomo_api::types::{ConnectionInfo, LogEntry, TrafficData};

#[derive(Debug, Clone)]
pub enum Action {
    StartCore,
    StopCore,
    RestartCore,
    CoreStarted,
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
    CloseConnectionFailed { id: String, error: String },
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
        let _ = Action::CoreStarted;
        let _ = Action::CoreExited(0);
        let _ = Action::CoreExited(137);
        let _ = Action::CoreError("boom".to_string());
        let _ = Action::Quit;
    }

    fn assert_send_sync<T: Send + Sync>() {}
}
