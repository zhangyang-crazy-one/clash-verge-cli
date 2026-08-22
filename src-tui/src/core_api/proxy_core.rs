//! The [`ProxyCoreApi`] trait: the controller surface every proxy core
//! must expose to the TUI, plus per-core capability reporting.

use crate::mihomo_api::error::MihomoError;
use crate::mihomo_api::types::{
    ConnectionsData, MihomoVersion, ProxyData, ProxyDelay, RuleProvidersResponse, RulesResponse,
};
use async_trait::async_trait;

/// Capabilities that differ between cores' controller APIs.
///
/// Callers MUST check the relevant capability before invoking the
/// corresponding methods; implementations for unsupported surfaces
/// return an error rather than pretending success.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CoreCapabilities {
    /// `/providers/rules` and `/providers/proxies` are functional.
    ///
    /// mihomo implements them; sing-box's clash_api routes exist but are
    /// empty stubs (verified against sing-box testing branch), so any
    /// provider UI must be hidden while a sing-box core is active.
    pub rule_providers: bool,
}

/// Transport-agnostic controller API shared by mihomo and sing-box.
///
/// Method set mirrors [`crate::mihomo_api::MihomoApi`] — the monitoring
/// and control endpoints both cores implement with identical wire formats.
/// Object-safe (`async_trait`) so callers can hold
/// `Box<dyn ProxyCoreApi>` and swap cores at runtime.
#[async_trait]
pub trait ProxyCoreApi: Send + Sync {
    /// Static capability description of this core's controller API.
    fn capabilities(&self) -> CoreCapabilities;

    /// Convenience predicate mirroring [`CoreCapabilities::rule_providers`].
    fn supports_providers(&self) -> bool {
        self.capabilities().rule_providers
    }

    async fn version(&self) -> Result<MihomoVersion, MihomoError>;
    async fn get_proxies(&self) -> Result<ProxyData, MihomoError>;
    async fn select_proxy(&self, group: &str, name: &str) -> Result<(), MihomoError>;
    /// `PATCH /configs` — set clash mode (`rule` / `global` / `direct`).
    async fn patch_mode(&self, mode: &str) -> Result<(), MihomoError>;
    async fn get_mode(&self) -> Result<String, MihomoError>;
    async fn delay_test(&self, name: &str, test_url: &str, timeout_ms: u64) -> Result<ProxyDelay, MihomoError>;
    async fn stream_traffic(&self) -> Result<reqwest::Response, MihomoError>;
    async fn get_connections(&self) -> Result<ConnectionsData, MihomoError>;
    async fn close_connection(&self, id: &str) -> Result<(), MihomoError>;
    async fn close_all_connections(&self) -> Result<(), MihomoError>;
    async fn stream_logs(&self, level: &str) -> Result<reqwest::Response, MihomoError>;
    async fn get_rules(&self) -> Result<RulesResponse, MihomoError>;

    /// Capability-gated: only meaningful when
    /// [`Self::supports_providers`] returns `true`.
    async fn get_rule_providers(&self) -> Result<RuleProvidersResponse, MihomoError>;

    /// Capability-gated: only meaningful when
    /// [`Self::supports_providers`] returns `true`.
    async fn update_rule_provider(&self, name: &str) -> Result<(), MihomoError>;
}

#[async_trait]
impl ProxyCoreApi for crate::mihomo_api::MihomoApi {
    fn capabilities(&self) -> CoreCapabilities {
        // mihomo implements provider endpoints fully.
        CoreCapabilities { rule_providers: true }
    }

    async fn version(&self) -> Result<MihomoVersion, MihomoError> {
        Self::version(self).await
    }

    async fn get_proxies(&self) -> Result<ProxyData, MihomoError> {
        Self::get_proxies(self).await
    }

    async fn select_proxy(&self, group: &str, name: &str) -> Result<(), MihomoError> {
        Self::select_proxy(self, group, name).await
    }

    async fn patch_mode(&self, mode: &str) -> Result<(), MihomoError> {
        Self::patch_mode(self, mode).await
    }

    async fn get_mode(&self) -> Result<String, MihomoError> {
        Self::get_mode(self).await
    }

    async fn delay_test(&self, name: &str, test_url: &str, timeout_ms: u64) -> Result<ProxyDelay, MihomoError> {
        Self::delay_test(self, name, test_url, timeout_ms).await
    }

    async fn stream_traffic(&self) -> Result<reqwest::Response, MihomoError> {
        Self::stream_traffic(self).await
    }

    async fn get_connections(&self) -> Result<ConnectionsData, MihomoError> {
        Self::get_connections(self).await
    }

    async fn close_connection(&self, id: &str) -> Result<(), MihomoError> {
        Self::close_connection(self, id).await
    }

    async fn close_all_connections(&self) -> Result<(), MihomoError> {
        Self::close_all_connections(self).await
    }

    async fn stream_logs(&self, level: &str) -> Result<reqwest::Response, MihomoError> {
        Self::stream_logs(self, level).await
    }

    async fn get_rules(&self) -> Result<RulesResponse, MihomoError> {
        Self::get_rules(self).await
    }

    async fn get_rule_providers(&self) -> Result<RuleProvidersResponse, MihomoError> {
        Self::get_rule_providers(self).await
    }

    async fn update_rule_provider(&self, name: &str) -> Result<(), MihomoError> {
        Self::update_rule_provider(self, name).await
    }
}

#[cfg(test)]
#[allow(clippy::panic, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::mihomo_api::MihomoApi;

    #[tokio::test]
    async fn trait_is_object_safe_and_dispatches() {
        // Proves Box<dyn ProxyCoreApi> is usable: the event loop will hold
        // the active core's API behind this indirection and swap it at
        // runtime when the user switches cores.
        let api: Box<dyn ProxyCoreApi> = Box::new(
            MihomoApi::new(
                std::path::PathBuf::from("/tmp/nonexistent-proxycore-trait-test.sock"),
                "secret",
            )
            .expect("build"),
        );

        assert!(api.supports_providers(), "mihomo implements providers");
        assert!(api.capabilities().rule_providers);

        match api.version().await {
            Err(MihomoError::CoreDown { endpoint }) => {
                assert!(endpoint.contains("proxycore-trait-test"));
            }
            other => panic!("expected CoreDown through dyn dispatch, got {other:?}"),
        }
    }
}
