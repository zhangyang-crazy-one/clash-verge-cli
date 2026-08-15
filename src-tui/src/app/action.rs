use super::{TunSetupReason, View};
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
    /// The SSRF safety check found a blocked host during import; the user must
    /// explicitly trust it before that URL may be imported.
    ImportNeedsTrust {
        url: String,
        host: String,
    },
    /// The SSRF safety check found a blocked host during a manual profile
    /// refresh; the user must explicitly trust it before the update retries.
    /// Carries the profile uid so confirming persists the trust into that
    /// profile's stored option.
    UpdateNeedsTrust {
        uid: String,
        host: String,
    },
    /// User pressed `y` on the trust prompt: retry only that import with the
    /// host in `trusted_hosts` (reads `app.pending_trust`).
    ConfirmTrustImport,
    /// User pressed `n`/Esc on the trust prompt: cancel. No trust is saved.
    CancelTrustImport,
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
    /// TUN setup transaction finished. `resume_start` is `Some(enable_tun)`
    /// when a pending core start (offered from the core-start prompt) should
    /// resume now that the capability/DNS rule are installed, `None` for the
    /// explicit Settings → TUN setup action.
    TunSetupSucceeded {
        resume_start: Option<bool>,
    },
    /// The TUN-enabled core-start preflight found a missing file capability
    /// or DNS polkit rule; open the TUI-native confirm dialog. `reason`
    /// records which gate fired so the skip key knows whether starting
    /// anyway is safe (missing DNS rule only) or must cancel (missing
    /// capability).
    TunSetupPrompt {
        binary: std::path::PathBuf,
        enable_tun: bool,
        reason: TunSetupReason,
    },
    /// User pressed `y` on the core-start setup confirm: open the password
    /// popup; the same one-time transaction runs and the start resumes.
    ConfirmTunSetup,
    /// User pressed `n`/Esc/q on the core-start setup confirm: dismiss.
    /// Starts anyway when only the DNS rule is missing; cancels the start
    /// with a pointer at TUN setup when the capability is missing.
    SkipTunSetupStart,
    /// Resume the pending core start (post-setup success or after the user
    /// chose to start without setup).
    ResumeCoreStart {
        enable_tun: bool,
    },
    /// Password popup input (hidden buffer, `•` masked).
    PasswordChar(char),
    PasswordBackspace,
    PasswordSubmit,
    PasswordCancel,
    /// The user chose the explicit Settings → TUN setup action and the
    /// resolved binary needs capabilities; open the popup.
    TunSetupRequested(std::path::PathBuf),

    // System service + login autostart (Settings rows 5 and 6)
    /// Refresh the cached read-only service/autostart probes
    /// (`systemctl is-active/is-enabled`, `systemctl --user is-enabled`).
    ServiceStatusRefresh,
    /// Result of the refresh probe: cached values for the Settings rows.
    ServiceStatus {
        active: String,
        enabled: String,
        /// Whether the system service unit file is installed (unit presence
        /// probe, not `is-enabled` — an installed-but-disabled unit must
        /// still offer uninstall).
        installed: bool,
        auto_launch: bool,
    },
    /// User pressed `y` on the service-uninstall confirm: open the password
    /// popup with a `ServiceUninstall` pending action.
    ConfirmServiceUninstall,
    /// User pressed `n`/Esc/q on the service-uninstall confirm: cancel.
    CancelServiceUninstall,
    /// The sudo -S service install transaction succeeded.
    ServiceInstalled,
    /// The sudo -S service uninstall transaction succeeded.
    ServiceUninstalled,
    /// A service install/uninstall transaction failed (`sudo`/`systemctl`
    /// stderr is the payload).
    ServiceActionFailed(String),
    /// Re-asserting the GNOME/KDE system proxy after a live core was
    /// confirmed failed (no working backend / command error). Routed through
    /// its own action so the failure survives the core-started status line.
    SysProxyApplyFailed(String),
    /// The login-autostart toggle succeeded; `enabled` is the new state.
    AutoLaunchChanged {
        enabled: bool,
    },
    /// The login-autostart toggle failed (`systemctl --user` error text is
    /// the payload — headless/no-user-session surfaces verbatim).
    AutoLaunchFailed(String),
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
        let _ = Action::TunSetupSucceeded {
            resume_start: Some(true),
        };
        let _ = Action::TunSetupPrompt {
            binary: std::path::PathBuf::from("/fake/mihomo"),
            enable_tun: true,
            reason: TunSetupReason::MissingDnsRule,
        };
        let _ = Action::ConfirmTunSetup;
        let _ = Action::SkipTunSetupStart;
        let _ = Action::ResumeCoreStart { enable_tun: true };
        let _ = Action::ServiceStatusRefresh;
        let _ = Action::ServiceStatus {
            active: "active".to_string(),
            enabled: "enabled".to_string(),
            installed: true,
            auto_launch: true,
        };
        let _ = Action::ConfirmServiceUninstall;
        let _ = Action::CancelServiceUninstall;
        let _ = Action::ServiceInstalled;
        let _ = Action::ServiceUninstalled;
        let _ = Action::ServiceActionFailed("sudo: no tty".to_string());
        let _ = Action::AutoLaunchChanged { enabled: true };
        let _ = Action::AutoLaunchFailed("systemctl --user enable failed".to_string());
    }

    fn assert_send_sync<T: Send + Sync>() {}
}
