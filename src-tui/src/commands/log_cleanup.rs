//! Auto-log cleanup — removes old mihomo log files per `auto_log_clean` verge setting.
//!
//! Mapping (matching GUI `auto_log_clean`):
//!   0 = off, 1 = 1 day, 2 = 7 days, 3 = 30 days, 4 = 90 days.

use std::path::Path;
use std::time::{Duration, SystemTime};

/// Run cleanup for the given app home directory. Errors are logged as warnings
/// and never block startup.
pub async fn run(app_home: &Path, auto_log_clean: Option<i32>) {
    let cutoff_days = match auto_log_clean.unwrap_or(0) {
        0 => return,
        1 => 1,
        2 => 7,
        3 => 30,
        4 => 90,
        _ => return,
    };

    let logs_dir = match app_home.join("logs").canonicalize() {
        Ok(d) if d.is_dir() => d,
        _ => return,
    };

    let cutoff = match SystemTime::now().checked_sub(Duration::from_secs(cutoff_days * 86400)) {
        Some(t) => t,
        None => return,
    };

    let mut entries = match tokio::fs::read_dir(&logs_dir).await {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!(target: "log_cleanup", "cannot read logs dir {}: {e}", logs_dir.display());
            return;
        }
    };

    let mut cleaned = 0u64;
    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let meta = match path.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        let modified = match meta.modified() {
            Ok(t) => t,
            Err(_) => continue,
        };
        if modified >= cutoff {
            continue;
        }
        if let Err(e) = tokio::fs::remove_file(&path).await {
            tracing::warn!(target: "log_cleanup", "failed to remove {}: {e}", path.display());
        } else {
            cleaned += 1;
        }
    }

    if cleaned > 0 {
        tracing::info!(target: "log_cleanup", "removed {cleaned} old log files older than {cutoff_days} days");
    }
}
