//! System proxy management for desktop environments (GNOME/KDE).
//!
//! Intentionally does **not** mutate process environment variables: calling
//! `std::env::set_var` from a multithreaded Tokio runtime is unsound when other
//! tasks (e.g. reqwest) may read the environment concurrently.

use std::process::Command;

fn run(program: &str, args: &[&str]) -> bool {
    Command::new(program)
        .args(args)
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn apply_gnome(host: &str, port: u16) -> bool {
    let port = port.to_string();
    run("gsettings", &["set", "org.gnome.system.proxy", "mode", "manual"])
        && run("gsettings", &["set", "org.gnome.system.proxy.http", "host", host])
        && run(
            "gsettings",
            &["set", "org.gnome.system.proxy.http", "port", port.as_str()],
        )
        && run("gsettings", &["set", "org.gnome.system.proxy.https", "host", host])
        && run(
            "gsettings",
            &["set", "org.gnome.system.proxy.https", "port", port.as_str()],
        )
}

fn clear_gnome() -> bool {
    run("gsettings", &["set", "org.gnome.system.proxy", "mode", "none"])
}

fn apply_kde(proxy_url: &str) -> bool {
    run(
        "kwriteconfig5",
        &[
            "--file",
            "kioslaverc",
            "--group",
            "Proxy Settings",
            "--key",
            "ProxyType",
            "1",
        ],
    ) && run(
        "kwriteconfig5",
        &[
            "--file",
            "kioslaverc",
            "--group",
            "Proxy Settings",
            "--key",
            "httpProxy",
            proxy_url,
        ],
    ) && run(
        "kwriteconfig5",
        &[
            "--file",
            "kioslaverc",
            "--group",
            "Proxy Settings",
            "--key",
            "httpsProxy",
            proxy_url,
        ],
    )
}

fn clear_kde() -> bool {
    run(
        "kwriteconfig5",
        &[
            "--file",
            "kioslaverc",
            "--group",
            "Proxy Settings",
            "--key",
            "ProxyType",
            "0",
        ],
    )
}

/// Toggle system proxy on via desktop environment settings.
pub fn set_system_proxy(host: &str, port: u16) -> anyhow::Result<()> {
    let proxy_url = format!("http://{host}:{port}");
    let gnome_ok = apply_gnome(host, port);
    let kde_ok = apply_kde(&proxy_url);
    if gnome_ok || kde_ok {
        return Ok(());
    }
    anyhow::bail!("no desktop proxy backend available (tried gsettings and kwriteconfig5)");
}

/// Toggle system proxy off via desktop environment settings.
pub fn unset_system_proxy() -> anyhow::Result<()> {
    let gnome_ok = clear_gnome();
    let kde_ok = clear_kde();
    if gnome_ok || kde_ok {
        return Ok(());
    }
    anyhow::bail!("no desktop proxy backend available (tried gsettings and kwriteconfig5)");
}
