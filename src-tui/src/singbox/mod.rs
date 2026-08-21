//! sing-box runtime configuration generation.
//!
//! The generator produces the `singbox.json` runtime file consumed by
//! `sing-box run -c`. Node outbounds arrive pre-converted (see the
//! subscription converter), while inbound/route/clash_api plumbing is
//! generated here from TUI settings.

// Consumed from task 2.3 of add-singbox-dual-core onward (manager writes
// singbox.json; converter feeds outbounds). Until then this is scaffold
// exercised by unit tests only.
#[allow(dead_code)]
pub mod config_gen;

