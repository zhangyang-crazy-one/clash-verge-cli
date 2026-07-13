//! Minimal `Config` placeholder for Phase 1.
//!
//! The original `Config` singleton coordinated runtime config, profile cleanup,
//! and the validation pipeline. It depended on `core/`, `enhance/`, `process/`
//! and `feat/` modules which are excluded from Phase 1. The full singleton
//! will return in a later phase when those modules are reintroduced.
//!
//! For Phase 1, callers should use the type-specific readers directly:
//!   - `config::verge::IVerge::new()` (returns the verge.yaml schema)
//!   - `config::profiles::IProfiles::new()` (returns the profiles.yaml schema)
//!   - `config::clash::IClashTemp::new()` (returns the clash config.yaml schema)

#[derive(Debug)]
pub enum ConfigType {
    Run,
    Check,
}
