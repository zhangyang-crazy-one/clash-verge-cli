//! Core-agnostic proxy controller API.
//!
//! Both mihomo and sing-box expose a Clash-compatible REST controller
//! (sing-box through its experimental `clash_api`), but the two differ in
//! transport (unix socket vs TCP), endpoint coverage (provider endpoints
//! are empty stubs on sing-box), and configuration semantics. This module
//! holds the [`ProxyCoreApi`] trait that abstracts those differences so
//! the rest of the TUI can swap cores without caring which one runs.

// Consumed from group 3 of add-singbox-dual-core onward (SingboxApi
// implementation and event-loop core swap); until then this is
// scaffold plus its mihomo impl.
#[allow(dead_code)]
pub mod proxy_core;

