//! `tun setup` / `tun status` — the only explicit TUN privilege path.
//!
//! `tun setup` foreground-runs the self-rendered askpass (`sudo -A`) and
//! applies `cap_net_admin,cap_net_raw+eip` to the resolved mihomo binary.
//! `tun status` reports the capability state read-only. Daily core lifecycle
//! never invokes sudo; it only consumes the state prepared here.

use anyhow::Context;

/// Resolve the mihomo binary and grant TUN capabilities if missing.
///
/// Idempotent: when the binary is already capable the command succeeds
/// without invoking sudo.
pub async fn setup() -> anyhow::Result<()> {
    let resolved = crate::mihomo_manager::binary::resolve_or_install()
        .await
        .context("failed to resolve mihomo binary")?;
    println!("mihomo binary: {}", resolved.path.display());
    println!("version:       {}", resolved.version);

    let applied = crate::commands::privilege::apply_tun_capability(&resolved.path)?;
    if applied {
        println!("TUN capabilities applied ({}).", crate::commands::privilege::TUN_CAPS);
    } else {
        println!("TUN capabilities already present — nothing to do.");
    }
    Ok(())
}

/// Read-only report of the resolved binary's TUN capability state.
pub async fn status() -> anyhow::Result<()> {
    let resolved = crate::mihomo_manager::binary::resolve_or_install()
        .await
        .context("failed to resolve mihomo binary")?;
    let privileged = crate::commands::privilege::has_tun_capability(&resolved.path);
    let root = crate::commands::privilege::running_as_root();

    println!("mihomo binary: {}", resolved.path.display());
    println!("version:       {}", resolved.version);
    println!("TUN capability: {}", if privileged { "present" } else { "missing" });
    println!(
        "effective uid:  {}",
        if root {
            "root (bypasses capability check)"
        } else {
            "non-root"
        }
    );
    if !privileged && !root {
        println!(
            "hint: run `{}` to grant {}",
            crate::commands::privilege::TUN_SETUP_COMMAND,
            crate::commands::privilege::TUN_CAPS
        );
    }
    Ok(())
}
