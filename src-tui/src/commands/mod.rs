pub mod askpass;
pub mod backup;
pub mod connections;
pub mod core;
pub mod daemon;
pub mod docs;
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
pub mod unlock;

use std::path::PathBuf;

use crate::mihomo_api::MihomoApi;
use crate::mihomo_api::error::MihomoError;
use crate::mihomo_manager::manager::MihomoManager;
use clash_verge_core::config::IClashTemp;

/// Build a MihomoManager wired with config from the standalone config dir.
///
/// The core kind comes from the **running** pid record when there is one
/// and from `verge.yaml`'s `proxy_core` otherwise (issue #56). The record
/// is the source of truth for what is actually serving this controller:
/// editing `proxy_core` while a core runs used to make `status`/`stop`/
/// `restart` look for a core that was never started and refuse with "not
/// started by clash-verge-cli". `adopt_running_core` then validates the
/// record (kind, executable, controller endpoint) before any pid is used.
pub async fn build_manager(config_dir: PathBuf) -> anyhow::Result<MihomoManager> {
    let clash = IClashTemp::new().await;
    let socket = controller_socket_path(&clash.0).await;
    let singbox = crate::mihomo_manager::manager::configured_singbox_controller(&clash.0)?;
    let kind = running_core_kind(&socket, singbox).await;
    build_manager_for_kind(config_dir, kind).await
}

/// Build a manager pinned to `kind`, ignoring the running record. Used for
/// the *replacement* side of `restart`/`core use`, which must follow the
/// configured `proxy_core` rather than the core being replaced.
pub async fn build_manager_for_kind(
    config_dir: PathBuf,
    kind: crate::mihomo_manager::CoreKind,
) -> anyhow::Result<MihomoManager> {
    let clash = IClashTemp::new().await;
    let info = clash.get_client_info();
    let socket_path = controller_socket_path(&clash.0).await;
    let secret = info.secret.unwrap_or_default();

    let mut manager = MihomoManager::new(config_dir)
        .with_socket(socket_path)
        .with_singbox_controller(crate::mihomo_manager::manager::configured_singbox_controller(&clash.0)?)
        .with_secret(secret);

    if kind == crate::mihomo_manager::CoreKind::SingBox {
        manager = manager.with_core_kind(crate::mihomo_manager::CoreKind::SingBox);
    }

    // A core started by an earlier `start` (or the TUI) is managed here too.
    manager.adopt_running_core();
    Ok(manager)
}

/// The core kind a lifecycle command must talk to (#56): the kind recorded
/// for the running core, else `verge.yaml`'s `proxy_core`.
pub async fn running_core_kind(
    socket_path: &std::path::Path,
    singbox_controller: std::net::SocketAddr,
) -> crate::mihomo_manager::CoreKind {
    use crate::mihomo_manager::CoreKind;
    use crate::mihomo_manager::pidfile;
    // Only a record that would actually be ADOPTED counts: kind, recorded
    // executable, live pid and controller endpoint must all check out.
    // Reading the field alone would let a stale record — or a recycled pid
    // — steer the lifecycle commands at the wrong core.
    let path = pidfile::path_for(socket_path);
    for kind in [CoreKind::Mihomo, CoreKind::SingBox] {
        let endpoint = (kind == CoreKind::SingBox).then_some(singbox_controller);
        if pidfile::read_live_for_kind(&path, socket_path, kind, endpoint).is_some() {
            return kind;
        }
    }
    configured_core_kind().await
}

/// Read the Unix socket path from the CLI's own clash config; fall back to
/// the standalone socket (never a GUI path).
pub async fn controller_socket_path(config: &serde_yaml_ng::Mapping) -> PathBuf {
    config
        .get("external-controller-unix")
        .and_then(|v| v.as_str())
        .map(PathBuf::from)
        .unwrap_or_else(clash_verge_core::utils::dirs::standalone_socket_path)
}

/// The controller socket this CLI manages, as [`build_manager`] sees it.
pub async fn active_socket_path() -> PathBuf {
    let clash = IClashTemp::new().await;
    controller_socket_path(&clash.0).await
}

/// The core `verge.yaml` selects, re-read on every lifecycle command so a
/// switch written by the TUI, a restore or a hand edit is always honoured.
pub async fn configured_core_kind() -> crate::mihomo_manager::CoreKind {
    crate::mihomo_manager::manager::configured_core_kind().await
}

/// The last `lines` lines of `path`, for error messages.
pub fn log_tail(path: &std::path::Path, lines: usize) -> String {
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
/// Only an unreachable controller means "not running"; a controller that
/// answers with an error (wrong secret, bad response) is reported as such.
pub async fn running_api(manager: &MihomoManager) -> anyhow::Result<MihomoApi> {
    let api = manager.api();
    let core = match manager.core_kind() {
        crate::mihomo_manager::CoreKind::Mihomo => "mihomo",
        crate::mihomo_manager::CoreKind::SingBox => "sing-box",
    };
    match api.version().await {
        Ok(_) => Ok(api),
        Err(MihomoError::CoreDown { .. } | MihomoError::Io(_)) => Err(crate::exit::CoreNotRunning(format!(
            "{core} is not running (controller {} not reachable); start it with `clash-verge-cli start`",
            manager.socket_path().display()
        ))
        .into()),
        Err(error) => Err(anyhow::Error::new(error).context(format!(
            "{core} controller {} answered with an error",
            manager.socket_path().display()
        ))),
    }
}

/// Left-aligned text table (header row, then one row per item), padded by
/// display width so CJK names line up. Empty trailing padding is trimmed.
pub fn table<I>(headers: &[&str], rows: I) -> String
where
    I: IntoIterator<Item = Vec<String>>,
{
    use unicode_width::UnicodeWidthStr as _;

    let rows: Vec<Vec<String>> = std::iter::once(headers.iter().map(|h| (*h).to_string()).collect())
        .chain(rows)
        .collect();
    let columns = headers.len();
    let widths: Vec<usize> = (0..columns)
        .map(|column| {
            rows.iter()
                .filter_map(|row| row.get(column))
                .map(|cell| cell.width())
                .max()
                .unwrap_or(0)
        })
        .collect();
    let mut out = String::new();
    for row in rows {
        let mut line = String::new();
        for (column, cell) in row.iter().enumerate() {
            line.push_str(cell);
            if column + 1 < columns {
                line.push_str(&" ".repeat(widths[column] - cell.width() + 2));
            }
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
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
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn only_an_adoptable_pid_record_decides_the_kind_not_the_file_alone() {
        use crate::mihomo_manager::CoreKind;
        use crate::mihomo_manager::pidfile::{self, CoreRecord};

        let dir = std::env::temp_dir().join(format!("cv-corekind-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("external-controller.sock");
        let path = pidfile::path_for(&socket);

        // No record: the configured selection decides (this test's home has
        // no verge.yaml, so it falls back to mihomo).
        let endpoint: std::net::SocketAddr = "127.0.0.1:9090".parse().unwrap();
        assert_eq!(running_core_kind(&socket, endpoint).await, CoreKind::Mihomo);

        // A dead record is stale and must not steer lifecycle commands.
        pidfile::write(
            &path,
            CoreRecord::with_kind(u32::MAX, chrono::Utc::now(), CoreKind::SingBox),
        )
        .unwrap();
        assert_eq!(running_core_kind(&socket, endpoint).await, CoreKind::Mihomo);

        // A live pid whose cmdline does not serve this controller is not
        // adoptable either (here: the test binary itself), so the
        // configuration still decides. #56 only redirects lifecycle
        // commands for a core this CLI could really have started.
        pidfile::write(
            &path,
            CoreRecord::with_kind(std::process::id(), chrono::Utc::now(), CoreKind::SingBox),
        )
        .unwrap();
        assert_eq!(running_core_kind(&socket, endpoint).await, CoreKind::Mihomo);

        let _ = std::fs::remove_dir_all(&dir);
    }

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
    fn table_aligns_by_display_width() {
        let out = table(
            &["UID", "NAME", "URL"],
            [
                vec!["R1".into(), "家".into(), "a".into()],
                vec!["R22".into(), "ab".into(), "b".into()],
            ],
        );
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[0], "UID  NAME  URL");
        // "家" is two columns wide, like "ab".
        assert_eq!(lines[1], "R1   家    a");
        assert_eq!(lines[2], "R22  ab    b");
    }

    #[test]
    fn format_bytes_scales_units() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(1023), "1023 B");
        assert_eq!(format_bytes(1536), "1.5 KiB");
        assert_eq!(format_bytes(5 * 1024 * 1024 * 1024), "5.0 GiB");
    }
}
