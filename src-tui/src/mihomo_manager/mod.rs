//! Mihomo process lifecycle: spawn, watch, graceful shutdown, restart policy.
//!
//! - `manager` owns the struct and shared inner state
//! - `signal` implements SIGTERM-then-SIGKILL graceful shutdown
//! - `watcher` spawns the background task that monitors child exits
//! - `binary` resolves the mihomo binary path on disk

// Foundation module — public surface wired up by Plan 02-03.
#![allow(dead_code, unused_imports)]

pub mod binary;
pub mod manager;
pub mod signal;
pub mod watcher;

pub use manager::{ManagerInner, MihomoManager};
