// System proxy management: set/unset http_proxy/https_proxy env vars
// and GNOME/KDE desktop environment proxy settings.

use std::process::Command;

/// Toggle system proxy on. Sets environment variables + DE settings.
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

    // Set env vars for current process tree
    unsafe {
        std::env::set_var("http_proxy", &proxy_url);
        std::env::set_var("https_proxy", &proxy_url);
        std::env::set_var("all_proxy", &proxy_url);
        std::env::set_var("HTTP_PROXY", &proxy_url);
        std::env::set_var("HTTPS_PROXY", &proxy_url);
        std::env::set_var("ALL_PROXY", &proxy_url);
    }

    Ok(())
}

/// Toggle system proxy off. Unsets all proxy settings.
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

    // Unset env vars
    unsafe {
        std::env::remove_var("http_proxy");
        std::env::remove_var("https_proxy");
        std::env::remove_var("all_proxy");
        std::env::remove_var("HTTP_PROXY");
        std::env::remove_var("HTTPS_PROXY");
        std::env::remove_var("ALL_PROXY");
    }

    Ok(())
}
