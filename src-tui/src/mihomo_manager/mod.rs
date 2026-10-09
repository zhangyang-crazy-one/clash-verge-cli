//! Mihomo process lifecycle: spawn, watch, graceful shutdown, restart policy.
//!
//! - `manager` owns the struct and shared inner state
//! - `signal` implements SIGTERM-then-SIGKILL graceful shutdown
//! - `watcher` spawns the background task that monitors child exits
//! - `binary` resolves the mihomo binary path and auto-installs when missing
//! - `pidfile` records the running core so other CLI processes can adopt it

// Foundation module — public surface wired up by Plan 02-03.
#![allow(dead_code, unused_imports)]

pub mod binary;
mod controller_socket;
pub mod core_policy;
// `core_policy` and binary parser tests are offline fixture checks. The live
// sing-box spawn/controller test remains explicitly ignored and must stay out
// of the safe suite (`real_sing_box_spawns_and_answers_controller`).
pub mod manager;
pub mod ownership;
pub mod pidfile;
pub mod signal;
pub mod singbox_binary;
#[cfg(test)]
#[allow(clippy::expect_used)]
mod singbox_e2e;
pub mod watcher;

pub use manager::{CoreKind, ManagerInner, MihomoManager};
