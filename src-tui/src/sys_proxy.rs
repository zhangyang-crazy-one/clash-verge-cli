//! System proxy management for desktop environments (GNOME/KDE).
//!
//! Intentionally does **not** mutate process environment variables: calling
//! `std::env::set_var` from a multithreaded Tokio runtime is unsound when other
//! tasks (e.g. reqwest) may read the environment concurrently.

use std::process::Command;

/// Toggle system proxy on via desktop environment settings.
pub fn set_system_proxy(host: &str, port: u16) -> std::io::Result<()> {
    let proxy_url = format!("http://{host}:{port}");

    // GNOME gsettings
    let _ = Command::new("gsettings")
        .args(["set", "org.gnome.system.proxy", "mode", "manual"])
        .output();
    let _ = Command::new("gsettings")
        .args(["set", "org.gnome.system.proxy.http", "host", host])
        .output();
    let _ = Command::new("gsettings")
        .args(["set", "org.gnome.system.proxy.http", "port", &port.to_string()])
        .output();
    let _ = Command::new("gsettings")
        .args(["set", "org.gnome.system.proxy.https", "host", host])
        .output();
    let _ = Command::new("gsettings")
        .args(["set", "org.gnome.system.proxy.https", "port", &port.to_string()])
        .output();

    // KDE
    let _ = Command::new("kwriteconfig5")
        .args([
            "--file",
            "kioslaverc",
            "--group",
            "Proxy Settings",
            "--key",
            "ProxyType",
            "1",
        ])
        .output();
    let _ = Command::new("kwriteconfig5")
        .args([
            "--file",
            "kioslaverc",
            "--group",
            "Proxy Settings",
            "--key",
            "httpProxy",
            &proxy_url,
        ])
        .output();
    let _ = Command::new("kwriteconfig5")
        .args([
            "--file",
            "kioslaverc",
            "--group",
            "Proxy Settings",
            "--key",
            "httpsProxy",
            &proxy_url,
        ])
        .output();

    Ok(())
}

/// Toggle system proxy off via desktop environment settings.
pub fn unset_system_proxy() -> std::io::Result<()> {
    // GNOME
    let _ = Command::new("gsettings")
        .args(["set", "org.gnome.system.proxy", "mode", "none"])
        .output();

    // KDE
    let _ = Command::new("kwriteconfig5")
        .args([
            "--file",
            "kioslaverc",
            "--group",
            "Proxy Settings",
            "--key",
            "ProxyType",
            "0",
        ])
        .output();

    Ok(())
}
