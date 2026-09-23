pub mod askpass;
pub mod connections;
pub mod daemon;
pub mod log_cleanup;
pub mod mode;
pub mod privilege;
pub mod profile;
pub mod provider;
pub mod proxy;
pub mod restart;
pub mod service;
pub mod start;
pub mod status;
pub mod stop;
pub mod sysproxy;
pub mod tun;

use std::path::PathBuf;

use crate::mihomo_api::MihomoApi;
use crate::mihomo_manager::manager::MihomoManager;
use clash_verge_core::config::IClashTemp;

/// Build a MihomoManager wired with config from the standalone config dir.
pub async fn build_manager(config_dir: PathBuf) -> anyhow::Result<MihomoManager> {
    let clash = IClashTemp::new().await;
    let _info = clash.get_client_info();
    // Read Unix socket path from the CLI's own clash config; fall back to the
    // standalone socket (never a GUI path).
    let socket_path = clash
        .0
        .get("external-controller-unix")
        .and_then(|v| v.as_str())
        .map(PathBuf::from)
        .unwrap_or_else(clash_verge_core::utils::dirs::standalone_socket_path);
    let secret = _info.secret.unwrap_or_default();

    let manager = MihomoManager::new(config_dir)
        .with_socket(socket_path)
        .with_secret(secret);
    // A core started by an earlier `start` (or the TUI) is managed here too.
    manager.adopt_running_core();
    Ok(manager)
}

/// Wait until a just-started core answers on its controller socket. When it
/// exits instead (the watcher gave up restarting it) or never answers, the
/// error carries the tail of its log so the cause is visible.
pub async fn wait_until_ready(manager: &MihomoManager, timeout: std::time::Duration) -> anyhow::Result<()> {
    let api = manager.api();
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if core_running(&api).await {
            return Ok(());
        }
        let failure = match manager.state() {
            crate::app::CoreState::Error(message) => Some(format!("mihomo exited: {message}")),
            _ if tokio::time::Instant::now() >= deadline => Some(format!(
                "mihomo did not answer on {} within {}s",
                manager.socket_path().display(),
                timeout.as_secs()
            )),
            _ => None,
        };
        if let Some(failure) = failure {
            let log = crate::mihomo_manager::manager::core_log_path(manager.config_dir());
            anyhow::bail!("{failure}\n{}", log_tail(&log, 10));
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

/// The last `lines` lines of `path`, for error messages.
fn log_tail(path: &std::path::Path, lines: usize) -> String {
    match std::fs::read_to_string(path) {
        Ok(text) if !text.trim().is_empty() => {
            let tail: Vec<&str> = text.lines().rev().take(lines).collect();
            let tail: Vec<&str> = tail.into_iter().rev().collect();
            format!("last lines of {}:\n{}", path.display(), tail.join("\n"))
        }
        _ => format!("(no output in {})", path.display()),
    }
}

/// Whether the core answers on its controller socket.
pub async fn core_running(api: &MihomoApi) -> bool {
    api.version().await.is_ok()
}

/// The controller API of a running core, or an error pointing at `start`.
pub async fn running_api(manager: &MihomoManager) -> anyhow::Result<MihomoApi> {
    let api = manager.api();
    if core_running(&api).await {
        Ok(api)
    } else {
        anyhow::bail!(
            "mihomo is not running (controller {} not reachable); start it with `clash-verge-cli start`",
            manager.socket_path().display()
        )
    }
}

/// `1.2 KiB`-style size for transfer counters.
pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_tail_keeps_the_last_lines_in_order() {
        let path = std::env::temp_dir().join(format!("cv-tail-{}.log", uuid::Uuid::new_v4()));
        std::fs::write(&path, "a\nb\nc\nd\n").unwrap();
        assert!(log_tail(&path, 2).ends_with("c\nd"));
        std::fs::write(&path, "").unwrap();
        assert!(log_tail(&path, 2).starts_with("(no output"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn format_bytes_scales_units() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(1023), "1023 B");
        assert_eq!(format_bytes(1536), "1.5 KiB");
        assert_eq!(format_bytes(5 * 1024 * 1024 * 1024), "5.0 GiB");
    }
}
