//! `sysproxy on|off|status|env`.

use crate::mihomo_manager::manager::MihomoManager;
use crate::sys_proxy::{self, ProxySettings};

const NO_BACKEND_HINT: &str = "no desktop proxy backend (gsettings, kwriteconfig6/5) found; \
for terminal programs use `eval \"$(clash-verge-cli sysproxy env)\"`";

async fn save_enabled(enabled: bool) -> anyhow::Result<()> {
    let mut verge = clash_verge_core::config::IVerge::new().await;
    verge.enable_system_proxy = Some(enabled);
    verge.save_file().await
}

pub async fn on(manager: &MihomoManager) -> anyhow::Result<()> {
    if !sys_proxy::backend_available() {
        anyhow::bail!(NO_BACKEND_HINT);
    }
    save_enabled(true).await?;
    let settings = ProxySettings::load().await;
    if super::core_running(&manager.api()).await {
        let applied = settings.clone();
        tokio::task::spawn_blocking(move || sys_proxy::set_system_proxy(&applied)).await??;
        println!("system proxy on: {}:{}", settings.host, settings.port);
    } else {
        println!("system proxy enabled; applied when the core starts");
    }
    Ok(())
}

pub async fn off() -> anyhow::Result<()> {
    save_enabled(false).await?;
    if sys_proxy::backend_available() {
        tokio::task::spawn_blocking(sys_proxy::unset_system_proxy).await??;
    }
    println!("system proxy off");
    Ok(())
}

pub async fn status(manager: &MihomoManager) -> anyhow::Result<()> {
    let verge = clash_verge_core::config::IVerge::new().await;
    let settings = ProxySettings::load().await;
    let backend = sys_proxy::backend_available();
    let probe = settings.clone();
    let applied = backend && tokio::task::spawn_blocking(move || sys_proxy::is_applied(&probe)).await?;
    let running = super::core_running(&manager.api()).await;

    println!(
        "setting:  {}",
        if verge.enable_system_proxy.unwrap_or(false) {
            "on"
        } else {
            "off"
        }
    );
    println!("endpoint: {}:{}", settings.host, settings.port);
    println!("bypass:   {}", settings.bypass.join(","));
    println!(
        "desktop:  {}",
        match (backend, applied) {
            (false, _) => "no backend (use `sysproxy env`)",
            (true, true) => "using this proxy",
            (true, false) => "not using this proxy",
        }
    );
    println!("core:     {}", if running { "running" } else { "stopped" });
    Ok(())
}

pub async fn env(unset: bool) {
    if unset {
        print!("{}", sys_proxy::env_unsets());
    } else {
        print!("{}", sys_proxy::env_exports(&ProxySettings::load().await));
    }
}
