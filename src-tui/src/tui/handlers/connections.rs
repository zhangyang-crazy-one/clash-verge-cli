//! Connections, logs, and the live traffic / log streams.

use tokio::sync::mpsc;
use tokio_stream::StreamExt as _;

use crate::app::{Action, App, Focus, Overlay};
use crate::mihomo_api::types::{LogEntry, TrafficData};

use super::Ctx;

/// Most recent log entries kept in memory.
const MAX_LOG_ENTRIES: usize = 1_000;

pub(super) fn refresh_traffic(app: &mut App, ctx: &Ctx) {
    app.runtime_loading.traffic = true;
    app.runtime_errors.traffic = None;
    let api = ctx.manager.api();
    ctx.spawn(|tx| async move {
        if let Err(error) = receive_traffic_stream(api, tx.clone()).await {
            let _ = tx.send(Action::TrafficFailed(error));
        }
    });
}

pub(super) fn refresh_connections(app: &mut App, ctx: &Ctx) {
    app.runtime_loading.connections = true;
    app.runtime_errors.connections = None;
    let api = ctx.manager.api();
    ctx.spawn_result(
        async move { api.get_connections().await },
        Action::ConnectionsFetched,
        |error| Action::ConnectionsFailed(error.to_string()),
    );
}

/// New connection list: keep the selection on the same connection when it
/// is still open, otherwise fall back to the first visible one.
pub(super) fn note_connections(app: &mut App, data: crate::mihomo_api::types::ConnectionsData) {
    app.runtime_loading.connections = false;
    app.runtime_errors.connections = None;
    app.traffic_totals = Some((data.upload_total, data.download_total));
    let connections = data.connections;
    if app
        .selected_connection_id
        .as_ref()
        .is_some_and(|id| !connections.iter().any(|connection| &connection.id == id))
    {
        app.selected_connection_id = None;
        app.pending_connection_close = None;
    }
    app.connections = connections;
    let ids = visible_connection_ids(app);
    if let Some(id) = ids.first() {
        if app.selected_connection_id.is_none() {
            app.selected_connection_id = Some(id.clone());
        }
        app.connection_selected_index = app
            .selected_connection_id
            .as_deref()
            .and_then(|selected| ids.iter().position(|id| id == selected))
            .unwrap_or_default();
    } else {
        app.selected_connection_id = None;
        app.pending_connection_close = None;
        app.connection_selected_index = 0;
    }
}

/// `D`: ask before closing every connection.
pub(super) fn request_close_all(app: &mut App) {
    app.overlay = Some(Overlay::CloseAllConnectionsConfirmation);
    app.status_msg = Some("Close ALL connections? Press Enter to confirm".into());
    app.focus = Focus::Content;
}

/// `Enter` on the close confirmation: close it only if the prompt still
/// names the selected connection.
pub(super) fn confirm_close_from_key(app: &mut App, ctx: &Ctx, id: String) {
    if close_confirmation_is_current(app, &id) {
        close_connection(app, ctx, id);
    } else {
        app.status_msg = Some("Connection close confirmation expired".into());
    }
}

pub(super) fn close_connection(app: &mut App, ctx: &Ctx, id: String) {
    app.pending_connection_close = None;
    app.overlay = None;
    let api = ctx.manager.api();
    ctx.spawn(|tx| async move {
        let _ = tx.send(match api.close_connection(&id).await {
            Ok(()) => Action::ConnectionClosed(id),
            Err(error) => Action::CloseConnectionFailed {
                id,
                error: error.to_string(),
            },
        });
    });
}

pub(super) fn close_all(app: &mut App, ctx: &Ctx) {
    app.overlay = None;
    app.status_msg = Some("Closing all connections...".into());
    let api = ctx.manager.api();
    ctx.spawn_result(
        async move { api.close_all_connections().await },
        |()| Action::AllConnectionsClosed,
        |error| Action::CloseAllConnectionsFailed(error.to_string()),
    );
}

pub(super) fn refresh_logs(app: &mut App, ctx: &Ctx) {
    app.runtime_loading.logs = true;
    app.runtime_errors.logs = None;
    let api = ctx.manager.api();
    let tx = ctx.tx.clone();
    let level = app.log_level.clone();
    let task = tokio::spawn(async move {
        if let Err(error) = receive_log_stream(api, &level, tx.clone()).await {
            let _ = tx.send(Action::LogsFailed(error));
        }
    });
    if let Some(previous) = app.log_stream.replace(task.abort_handle()) {
        previous.abort();
    }
}

/// `L` on Logs: switch the running core to the next log level.
pub(super) fn cycle_log_level(app: &mut App, ctx: &Ctx) {
    if app.core_state != crate::app::CoreState::Running {
        app.status_msg = Some(app.tr("logs.level_needs_core").into());
        return;
    }
    let level = crate::app::next_log_level(&app.log_level).to_string();
    let api = ctx.manager.api();
    ctx.spawn_result(
        async move { api.patch_log_level(&level).await.map(|()| level) },
        Action::LogLevelChanged,
        |error| Action::LogLevelFailed(error.to_string()),
    );
}

/// The core now logs at `level`: resubscribe the stream at that level (the
/// stream filters by level too, so the old one would miss debug lines).
pub(super) fn note_log_level(app: &mut App, ctx: &Ctx, level: String) {
    app.status_msg = Some(format!("{}: {level}", app.tr("logs.level")));
    app.log_level = level;
    if let Some(stream) = app.log_stream.take() {
        stream.abort();
    }
    app.runtime_loading.logs = false;
    if app.view == crate::app::View::Logs {
        refresh_logs(app, ctx);
    }
}

pub(super) fn note_log(app: &mut App, log: LogEntry) {
    app.runtime_errors.logs = None;
    app.logs.push(log);
    if app.logs.len() > MAX_LOG_ENTRIES {
        let excess = app.logs.len() - MAX_LOG_ENTRIES;
        app.logs.drain(..excess);
    }
    app.log_selected_index = app.log_selected_index.min(visible_log_count(app).saturating_sub(1));
}

pub(super) fn begin_connection_close(app: &mut App) {
    if let Some(id) = app.selected_connection_id.clone() {
        app.pending_connection_close = Some(id.clone());
        app.overlay = Some(Overlay::CloseConfirmation);
        app.status_msg = Some(format!("Close connection {id}? Press Enter to confirm"));
    } else {
        app.status_msg = Some("Select a connection before closing it".into());
    }
}

pub(super) fn close_confirmation_is_current(app: &App, id: &str) -> bool {
    app.pending_connection_close.as_deref() == Some(id)
        && app.selected_connection_id.as_deref() == Some(id)
        && app.overlay == Some(Overlay::CloseConfirmation)
}

pub(super) fn visible_connection_ids(app: &App) -> Vec<String> {
    app.visible_connections()
        .into_iter()
        .map(|connection| connection.id.clone())
        .collect()
}

pub(super) fn move_connection_selection(app: &mut App, forward: bool) {
    let ids = visible_connection_ids(app);
    if ids.is_empty() {
        app.selected_connection_id = None;
        app.connection_selected_index = 0;
        return;
    }
    let current = app
        .selected_connection_id
        .as_deref()
        .and_then(|id| ids.iter().position(|candidate| candidate == id))
        .unwrap_or(0);
    let next = if forward {
        (current + 1) % ids.len()
    } else {
        (current + ids.len() - 1) % ids.len()
    };
    app.connection_selected_index = next;
    app.selected_connection_id = Some(ids[next].clone());
}

pub(super) fn visible_log_count(app: &App) -> usize {
    app.visible_logs().len()
}

pub(super) fn move_log_selection(app: &mut App, forward: bool) {
    let count = visible_log_count(app);
    if count == 0 {
        app.log_selected_index = 0;
    } else if forward {
        app.log_selected_index = (app.log_selected_index + 1) % count;
    } else {
        app.log_selected_index = (app.log_selected_index + count - 1) % count;
    }
}

/// Mihomo sends real-time APIs as newline-delimited JSON. Keep incomplete
/// packets buffered so records split across socket reads remain valid JSON.
pub(super) fn drain_ndjson<T: serde::de::DeserializeOwned>(
    buffer: &mut Vec<u8>,
    chunk: &[u8],
) -> Result<Vec<T>, String> {
    const MAX_PENDING_BYTES: usize = 1024 * 1024;

    buffer.extend_from_slice(chunk);
    if buffer.len() > MAX_PENDING_BYTES && !buffer.contains(&b'\n') {
        return Err("Mihomo stream sent more than 1 MiB without a newline".into());
    }

    let mut entries = Vec::new();
    while let Some(end) = buffer.iter().position(|byte| *byte == b'\n') {
        let line: Vec<u8> = buffer.drain(..=end).collect();
        let line = line.trim_ascii();
        if line.is_empty() {
            continue;
        }
        entries.push(serde_json::from_slice(line).map_err(|error| format!("invalid Mihomo stream entry: {error}"))?);
    }
    Ok(entries)
}

pub(super) async fn receive_traffic_stream(
    api: crate::mihomo_api::client::MihomoApi,
    tx: mpsc::UnboundedSender<Action>,
) -> Result<(), String> {
    let response = api.stream_traffic().await.map_err(|error| error.to_string())?;
    let mut stream = response.bytes_stream();
    let mut buffer = Vec::new();

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| error.to_string())?;
        for traffic in drain_ndjson::<TrafficData>(&mut buffer, &chunk)? {
            if tx.send(Action::TrafficFetched(traffic)).is_err() {
                return Ok(());
            }
        }
    }

    Err("Mihomo traffic stream closed".into())
}

pub(super) async fn receive_log_stream(
    api: crate::mihomo_api::client::MihomoApi,
    level: &str,
    tx: mpsc::UnboundedSender<Action>,
) -> Result<(), String> {
    let response = api.stream_logs(level).await.map_err(|error| error.to_string())?;
    let mut stream = response.bytes_stream();
    let mut buffer = Vec::new();

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| error.to_string())?;
        for log in drain_ndjson::<LogEntry>(&mut buffer, &chunk)? {
            if tx.send(Action::LogReceived(log)).is_err() {
                return Ok(());
            }
        }
    }

    Err("Mihomo log stream closed".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn close_connection_confirmation_stays_bound_to_the_selected_target() {
        let mut app = App::new();
        app.selected_connection_id = Some("connection-a".to_string());

        begin_connection_close(&mut app);

        assert_eq!(app.pending_connection_close.as_deref(), Some("connection-a"));
        assert_eq!(app.overlay, Some(Overlay::CloseConfirmation));
        assert!(close_confirmation_is_current(&app, "connection-a"));
        assert!(!close_confirmation_is_current(&app, "connection-b"));

        app.selected_connection_id = Some("connection-b".to_string());
        assert!(!close_confirmation_is_current(&app, "connection-a"));
    }

    #[test]
    fn ndjson_parser_buffers_partial_records_and_preserves_order() {
        let mut buffer = Vec::new();

        let initial = match drain_ndjson::<TrafficData>(&mut buffer, br#"{"up":1,"down":2}"#) {
            Ok(records) => records,
            Err(error) => panic!("partial record: {error}"),
        };
        assert!(initial.is_empty());

        let entries = match drain_ndjson::<TrafficData>(&mut buffer, b"\n{\"up\":3,\"down\":4}\n") {
            Ok(records) => records,
            Err(error) => panic!("complete records: {error}"),
        };
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].up, 1);
        assert_eq!(entries[1].down, 4);
    }

    #[test]
    fn ndjson_parser_ignores_blank_lines() {
        let mut buffer = Vec::new();
        let entries = match drain_ndjson::<LogEntry>(&mut buffer, b"\n {\"type\":\"info\",\"payload\":\"ready\"}\n\n") {
            Ok(records) => records,
            Err(error) => panic!("valid log record: {error}"),
        };

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].level, "info");
        assert_eq!(entries[0].payload, "ready");
    }
}
