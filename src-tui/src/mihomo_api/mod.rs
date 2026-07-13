// Public API of the mihomo REST client. Items are consumed by tests
// in this crate and by the CLI/manager in upcoming plans; the
// binary-only build does not see direct usage, so silence
// dead_code/unused_imports on the module root.
#![allow(dead_code, unused_imports)]

pub mod client;
pub mod error;
pub mod types;

pub use client::MihomoApi;
pub use error::MihomoError;
pub use types::MihomoVersion;
