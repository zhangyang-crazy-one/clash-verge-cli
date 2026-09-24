//! UI-independent operations shared by the interactive TUI and the
//! non-interactive CLI commands. Nothing here renders or prints: callers
//! decide how to report results.

pub mod backup;
pub mod mode;
pub mod profile;
pub mod proxy;
pub mod tun;
pub mod unlock;
pub mod webdav;
