use std::time::Duration;

use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind};
use serde_yaml_ng::Value;
use tokio::sync::mpsc;
use tokio::time;
use tokio_stream::StreamExt as _;

use crate::app::{
    Action, App, CoreState, Focus, InputMode, Overlay, ProxyDisplayRow, View, first_selectable_proxy_group,
    proxy_display_rows,
};
use crate::i18n::Language;
use crate::mihomo_api::types::{LogEntry, TrafficData};
use crate::tui::{TerminalGuard, input};

fn key_context(app: &App) -> input::KeyContext<'_> {
    input::KeyContext {
        view: app.view,
        focus: app.focus,
        overlay: app.overlay,
        pending_connection_close: app.pending_connection_close.as_deref(),
    }
}

fn dismiss_overlay(app: &mut App) {
    app.overlay = None;
    app.filter = None;
    app.pending_connection_close = None;
    app.focus = Focus::Menu;
}

fn begin_connection_close(app: &mut App) {
    if let Some(id) = app.selected_connection_id.clone() {
        app.pending_connection_close = Some(id.clone());
        app.overlay = Some(Overlay::CloseConfirmation);
        app.status_msg = Some(format!("Close connection {id}? Press Enter to confirm"));
    } else {
        app.status_msg = Some("Select a connection before closing it".into());
    }
}

fn close_confirmation_is_current(app: &App, id: &str) -> bool {
    app.pending_connection_close.as_deref() == Some(id)
        && app.selected_connection_id.as_deref() == Some(id)
        && app.overlay == Some(Overlay::CloseConfirmation)
}

fn connection_matches_filter(connection: &crate::mihomo_api::types::ConnectionInfo, query: &str) -> bool {
    let query = query.to_ascii_lowercase();
    let host = connection
        .metadata
        .as_ref()
        .and_then(|metadata| metadata.host.as_deref())
        .unwrap_or_default();
    let rule = connection.rule.as_deref().unwrap_or_default();
    connection.id.to_ascii_lowercase().contains(&query)
        || host.to_ascii_lowercase().contains(&query)
        || rule.to_ascii_lowercase().contains(&query)
}

fn visible_connection_ids(app: &App) -> Vec<String> {
    match app.connection_filter.as_deref() {
        Some(query) if !query.is_empty() => app
            .connections
            .iter()
            .filter(|connection| connection_matches_filter(connection, query))
            .map(|connection| connection.id.clone())
            .collect(),
        _ => app.connections.iter().map(|connection| connection.id.clone()).collect(),
    }
}

fn move_connection_selection(app: &mut App, forward: bool) {
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

fn visible_log_count(app: &App) -> usize {
    match app.log_filter.as_deref() {
        Some(query) if !query.is_empty() => {
            let query = query.to_ascii_lowercase();
            app.logs
                .iter()
                .filter(|entry| {
                    entry.level.to_ascii_lowercase().contains(&query)
                        || entry.payload.to_ascii_lowercase().contains(&query)
                })
                .count()
        }
        _ => app.logs.len(),
    }
}

fn move_log_selection(app: &mut App, forward: bool) {
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
fn drain_ndjson<T: serde::de::DeserializeOwned>(buffer: &mut Vec<u8>, chunk: &[u8]) -> Result<Vec<T>, String> {
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

async fn receive_traffic_stream(
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

async fn receive_log_stream(
    api: crate::mihomo_api::client::MihomoApi,
    tx: mpsc::UnboundedSender<Action>,
) -> Result<(), String> {
    let response = api.stream_logs().await.map_err(|error| error.to_string())?;
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

async fn reload_config_file(api: &crate::mihomo_api::MihomoApi, path: &std::path::Path) -> Result<(), String> {
    let config_path = path
        .to_str()
        .ok_or_else(|| format!("config path is not valid UTF-8: {}", path.display()))?;
    let response = api
        .client
        .put("http://localhost/configs?force=true")
        .json(&serde_json::json!({ "path": config_path, "payload": "" }))
        .send()
        .await
        .map_err(|error| error.to_string())?;

    if response.status().is_success() {
        Ok(())
    } else {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        Err(format!("Mihomo rejected config reload ({status}): {body}"))
    }
}

async fn apply_chain_config(
    api: &crate::mihomo_api::MihomoApi,
    chain_nodes: &[String],
) -> Result<std::path::PathBuf, String> {
    let mut config = clash_verge_core::config::IClashTemp::new().await.0;
    let entries = config
        .get("proxies")
        .and_then(Value::as_sequence)
        .ok_or_else(|| "active config has no proxies list".to_string())?;
    let mut proxies = entries
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            entry
                .as_mapping()
                .cloned()
                .ok_or_else(|| format!("proxies[{index}] is not a mapping"))
        })
        .collect::<Result<Vec<_>, _>>()?;

    crate::chain::build_chain_config(chain_nodes, &mut proxies).map_err(|error| error.to_string())?;
    config.insert(
        "proxies".into(),
        Value::Sequence(proxies.into_iter().map(Value::Mapping).collect()),
    );
    let yaml = serde_yaml_ng::to_string(&config).map_err(|error| error.to_string())?;
    let path = clash_verge_core::utils::dirs::clash_path().map_err(|error| error.to_string())?;
    let original = tokio::fs::read(&path)
        .await
        .map_err(|error| format!("failed to back up {}: {error}", path.display()))?;
    let backup_path = path.with_extension("yaml.tui-chain-backup");
    tokio::fs::write(&backup_path, &original)
        .await
        .map_err(|error| format!("failed to write {}: {error}", backup_path.display()))?;

    let temporary_path = path.with_extension("yaml.tui-chain.tmp");
    tokio::fs::write(&temporary_path, yaml)
        .await
        .map_err(|error| format!("failed to stage {}: {error}", temporary_path.display()))?;
    tokio::fs::rename(&temporary_path, &path)
        .await
        .map_err(|error| format!("failed to replace {}: {error}", path.display()))?;

    if let Err(error) = reload_config_file(api, &path).await {
        let _ = tokio::fs::write(&path, original).await;
        let _ = reload_config_file(api, &path).await;
        return Err(format!("{error}; restored the previous config"));
    }

    Ok(backup_path)
}

/// Find the (group_name, node_name) at a flat index in proxy groups.
fn find_node_at_index(
    groups: &std::collections::HashMap<String, crate::mihomo_api::types::ProxyGroup>,
    expanded_group: Option<&str>,
    target: usize,
) -> Option<(String, String)> {
    proxy_display_rows(groups, expanded_group)
        .get(target)
        .and_then(|row| row.node_identity())
        .map(|(group, node)| (group.to_string(), node.to_string()))
}

/// Collect all node names from all proxy groups.
fn all_node_names(groups: &std::collections::HashMap<String, crate::mihomo_api::types::ProxyGroup>) -> Vec<String> {
    let mut names: Vec<_> = groups
        .values()
        .filter_map(|group| group.all.as_ref().filter(|nodes| !nodes.is_empty()))
        .flatten()
        .cloned()
        .collect();
    names.sort_unstable();
    names.dedup();
    names
}

/// Count total flat items in proxy groups: one per group header + one per node.
fn count_flat_nodes(
    groups: &std::collections::HashMap<String, crate::mihomo_api::types::ProxyGroup>,
    expanded_group: Option<&str>,
) -> usize {
    proxy_display_rows(groups, expanded_group).len()
}

pub async fn run(config_dir: std::path::PathBuf) -> anyhow::Result<()> {
    let mut guard = TerminalGuard::new()?;
    let (action_tx, mut action_rx) = mpsc::unbounded_channel::<Action>();

    let manager = crate::commands::build_manager(config_dir).await?;
    manager.set_action_tx(action_tx.clone());

    let mut app = App::new();
    app.gui_config = clash_verge_core::config::IVerge::new().await;
    app.language = Language::from_config(app.gui_config.language.as_deref());
    app.core_config = clash_verge_core::config::IClashTemp::new().await;

    // Load profiles on start
    if let Ok(store) = crate::profile_store::store::ProfileStore::load().await {
        app.selected_index = store.selected_index();
        app.profiles = store.items();
        app.status_msg = Some(format!("{} profiles loaded", app.profiles.len()));
    }

    let mut events = EventStream::new();
    let mut render_tick = time::interval(Duration::from_millis(100));
    let mut runtime_refresh_tick = time::interval(Duration::from_secs(1));
    let mut rendered_view = app.view;

    // Try connecting to existing mihomo (may be running from GUI)
    let api = manager.api();
    let tx = action_tx.clone();
    tokio::spawn(async move {
        if api.version().await.is_ok() {
            let _ = tx.send(Action::CoreStarted {
                version: None,
                binary_path: None,
                binary_source: None,
            });
        }
        // If no controller is available, the user can press s to start one.
    });

    loop {
        tokio::select! {
            maybe_event = events.next() => {
                match maybe_event {
                    Some(Ok(Event::Resize(_, _))) => {
                        guard.reset_screen()?;
                    }
                    Some(Ok(Event::Key(key))) if key.kind != KeyEventKind::Release => {
                        match &app.input_mode {
                            InputMode::Importing(buffer) => {
                                match key.code {
                                    KeyCode::Enter => {
                                        let url = buffer.clone();
                                        app.input_mode = InputMode::Normal;
                                        app.status_msg = Some("Importing...".into());
                                        let tx = action_tx.clone();
                                        tokio::spawn(async move {
                                            let _ = tx.send(Action::ConfirmImport(url));
                                        });
                                    }
                                    KeyCode::Esc => {
                                        app.input_mode = InputMode::Normal;
                                    }
                                    KeyCode::Backspace => {
                                        let mut b = buffer.clone();
                                        b.pop();
                                        app.input_mode = InputMode::Importing(b);
                                    }
                                    KeyCode::Char(c) => {
                                        let mut b = buffer.clone();
                                        b.push(c);
                                        app.input_mode = InputMode::Importing(b);
                                    }
                                    _ => {}
                                }
                            }
                            InputMode::Normal => {
                                if app.overlay == Some(Overlay::Filter) {
                                    match input::map_key(key, key_context(&app)) {
                                        Some(Action::SubmitFilter) => {
                                            let query = app.filter.take().unwrap_or_default();
                                            let submitted = (!query.trim().is_empty()).then_some(query);
                                            match app.view {
                                                View::Connections => app.connection_filter = submitted,
                                                View::Logs => app.log_filter = submitted,
                                                view => {
                                                    app.status_msg = Some(format!(
                                                        "Filtering is not available in {} yet",
                                                        view.label()
                                                    ));
                                                }
                                            }
                                            app.overlay = None;
                                            app.focus = Focus::Content;
                                        }
                                        Some(Action::DismissOverlay) => dismiss_overlay(&mut app),
                                        _ => match key.code {
                                            KeyCode::Backspace => {
                                                if let Some(filter) = app.filter.as_mut() {
                                                    filter.pop();
                                                }
                                            }
                                            KeyCode::Char(character) => {
                                                if let Some(filter) = app.filter.as_mut() {
                                                    filter.push(character);
                                                }
                                            }
                                            _ => {}
                                        },
                                    }
                                } else if let Some(action) = input::map_key(key, key_context(&app)) {
                                    match action {
                                        Action::Quit => break,
                                        Action::StartCore => {
                                            app.core_state = CoreState::Starting;
                                            app.status_msg = Some(app.tr("home.starting_core").into());
                                            let m = manager.clone();
                                            let tx = action_tx.clone();
                                            tokio::spawn(async move {
                                                if let Err(error) = m.start().await {
                                                    let _ = tx.send(Action::CoreError(error.to_string()));
                                                }
                                                // On success, manager emits CoreStarted with launch details.
                                            });
                                        }
                                        Action::StopCore => {
                                            let m = manager.clone();
                                            tokio::spawn(async move { let _ = m.stop().await; });
                                        }
                                        Action::RestartCore => {
                                            app.core_state = CoreState::Starting;
                                            app.status_msg = Some(app.tr("home.starting_core").into());
                                            let m = manager.clone();
                                            let tx = action_tx.clone();
                                            tokio::spawn(async move {
                                                if let Err(error) = m.restart().await {
                                                    let _ = tx.send(Action::CoreError(error.to_string()));
                                                }
                                            });
                                        }
                                        Action::StartImport => {
                                            app.input_mode = InputMode::Importing(String::new());
                                        }
                                        Action::MoveNext => {
                                            if app.focus == Focus::Menu {
                                                let index = View::ALL.iter()
                                                    .position(|view| *view == app.view)
                                                    .unwrap_or_default();
                                                app.view = View::ALL[(index + 1) % View::ALL.len()];
                                            } else {
                                            match app.view {
                                                View::Profiles => {
                                                    let len = app.profiles.len();
                                                    if len > 0 {
                                                    let sel = app.selected_index;
                                                        let next = if sel + 1 >= len { 0 } else { sel + 1 };
                                                        app.selected_index = next;
                                                    }
                                                }
                                                View::Proxies => {
                                                    let total = count_flat_nodes(
                                                        &app.proxy_groups,
                                                        app.expanded_proxy_group.as_deref(),
                                                    );
                                                    if total > 0 {
                                                        app.node_selected_index = if app.node_selected_index + 1 >= total {
                                                            0
                                                        } else {
                                                            app.node_selected_index + 1
                                                        };
                                                    }
                                                }
                                                View::Connections => move_connection_selection(&mut app, true),
                                                View::Logs => move_log_selection(&mut app, true),
                                                _ => {}
                                            }
                                            }
                                        }
                                        Action::MovePrevious => {
                                            if app.focus == Focus::Menu {
                                                let index = View::ALL.iter()
                                                    .position(|view| *view == app.view)
                                                    .unwrap_or_default();
                                                app.view = View::ALL[(index + View::ALL.len() - 1) % View::ALL.len()];
                                            } else {
                                            match app.view {
                                                View::Profiles => {
                                                    let len = app.profiles.len();
                                                    if len > 0 {
                                                        let sel = app.selected_index;
                                                        let prev = if sel == 0 { len.saturating_sub(1) } else { app.selected_index.saturating_sub(1) };
                                                        app.selected_index = prev;
                                                    }
                                                }
                                                View::Proxies => {
                                                    let total = count_flat_nodes(
                                                        &app.proxy_groups,
                                                        app.expanded_proxy_group.as_deref(),
                                                    );
                                                    if total > 0 {
                                                        app.node_selected_index = if app.node_selected_index == 0 {
                                                            total.saturating_sub(1)
                                                        } else {
                                                            app.node_selected_index.saturating_sub(1)
                                                        };
                                                    }
                                                }
                                                View::Connections => move_connection_selection(&mut app, false),
                                                View::Logs => move_log_selection(&mut app, false),
                                                _ => {}
                                            }
                                            }
                                        }
                                        Action::Activate => {
                                            if app.focus == Focus::Menu {
                                                app.focus = Focus::Content;
                                                continue;
                                            }
                                            match app.view {
                                                View::Profiles => {
                                                    // Profile tab: switch profile
                                                    if let Some(item) = app.profiles.get(app.selected_index)
                                                        && let Some(ref uid) = item.uid {
                                                            let name = item.name.clone().unwrap_or_default();
                                                            let itype = item.itype.clone().unwrap_or_default();
                                                            app.status_msg = Some(format!("Switching to {name}..."));
                                                            let api = manager.api();
                                                            let u = uid.clone();
                                                            let tx = action_tx.clone();
                                                            if itype == "remote" {
                                                                tokio::spawn(async move {
                                                                    let body = format!("{{\"path\":\"{u}\"}}");
                                                                    let _ = api.client
                                                                        .put("http://localhost/configs")
                                                                        .header("Content-Type", "application/json")
                                                                        .body(body)
                                                                        .send()
                                                                        .await;
                                                                    let _ = tx.send(Action::ProxiesRefresh);
                                                                });
                                                            } else {
                                                                let item = item.clone();
                                                                tokio::spawn(async move {
                                                                    let profiles_dir = clash_verge_core::utils::dirs::app_profiles_dir()
                                                                        .unwrap_or_default();
                                                                    match crate::chain::resolve_chain(&item, &profiles_dir).await {
                                                                        Ok(chain) => {
                                                                            let mut config = clash_verge_core::config::IClashTemp::new().await.0;
                                                                            crate::chain::apply_chain_to_config(&mut config, &chain);
                                                                            if let Ok(yaml) = serde_yaml_ng::to_string(&config)
                                                                                && let Ok(path) = clash_verge_core::utils::dirs::clash_path()
                                                                            {
                                                                                let _ = tokio::fs::write(&path, &yaml).await;
                                                                                let _ = api.client
                                                                                    .put("http://localhost/configs?force=true")
                                                                                    .header("Content-Type", "application/json")
                                                                                    .body("{}")
                                                                                    .send()
                                                                                    .await;
                                                                            }
                                                                            let _ = tx.send(Action::ProxiesRefresh);
                                                                        }
                                                                        Err(e) => {
                                                                            let _ = tx.send(Action::CoreError(format!("chain: {e}")));
                                                                        }
                                                                    }
                                                                });
                                                            }
                                                        }
                                                }
                                                View::Proxies => {
                                                    let selected_row = proxy_display_rows(
                                                        &app.proxy_groups,
                                                        app.expanded_proxy_group.as_deref(),
                                                    )
                                                    .get(app.node_selected_index)
                                                    .cloned();
                                                    if let Some(ProxyDisplayRow::Group { name, node_count, .. }) = selected_row {
                                                        app.expanded_proxy_group = Some(name.clone());
                                                        app.node_selected_index = proxy_display_rows(
                                                            &app.proxy_groups,
                                                            Some(&name),
                                                        )
                                                        .iter()
                                                        .position(|row| {
                                                            matches!(row, ProxyDisplayRow::Group { name: row_name, .. } if row_name == &name)
                                                        })
                                                        .unwrap_or_default();
                                                        app.status_msg = Some(format!(
                                                            "Browsing {name}: {node_count} choices"
                                                        ));
                                                    } else if app.chain_mode {
                                                        if let Some((_, name)) = find_node_at_index(
                                                            &app.proxy_groups,
                                                            app.expanded_proxy_group.as_deref(),
                                                            app.node_selected_index,
                                                        )
                                                            && !app.chain_nodes.contains(&name) {
                                                                app.chain_nodes.push(name.clone());
                                                                app.status_msg = Some(format!("Chain: {}", app.chain_nodes.join(" → ")));
                                                            }
                                                    } else if let Some((group, name)) = find_node_at_index(
                                                        &app.proxy_groups,
                                                        app.expanded_proxy_group.as_deref(),
                                                        app.node_selected_index,
                                                    ) {
                                                        app.status_msg = Some(format!("Switching to {name}..."));
                                                        let api = manager.api();
                                                        let g = group.clone();
                                                        let n = name.clone();
                                                        let tx = action_tx.clone();
                                                        tokio::spawn(async move {
                                                            match api.select_proxy(&g, &n).await {
                                                                Ok(()) => {
                                                                    let _ = tx.send(Action::ProxiesRefresh);
                                                                }
                                                                Err(error) => {
                                                                    let _ = tx.send(Action::ProxiesFailed(error.to_string()));
                                                                }
                                                            }
                                                        });
                                                    }
                                                }
                                                View::Connections => begin_connection_close(&mut app),
                                                View::Settings => {
                                                    let next_language = app.language.next();
                                                    let mut updated_config = app.gui_config.clone();
                                                    updated_config.language = Some(next_language.config_code().into());
                                                    match updated_config.save_file().await {
                                                        Ok(()) => {
                                                            app.gui_config = updated_config;
                                                            app.language = next_language;
                                                            app.status_msg = Some(format!(
                                                                "{}: {}",
                                                                app.tr("settings.language_saved"),
                                                                next_language.display_name()
                                                            ));
                                                        }
                                                        Err(error) => {
                                                            app.status_msg = Some(format!(
                                                                "{}: {error}",
                                                                app.tr("settings.language_save_failed")
                                                            ));
                                                        }
                                                    }
                                                }
                                                _ => {}
                                            }
                                        }
                                        Action::NodeDelayTest => {
                                            if let Some((_, name)) = find_node_at_index(
                                                &app.proxy_groups,
                                                app.expanded_proxy_group.as_deref(),
                                                app.node_selected_index,
                                            ) {
                                                let api = manager.api();
                                                let n = name.clone();
                                                let tx = action_tx.clone();
                                                app.status_msg = Some(format!("Testing delay for {name}..."));
                                                tokio::spawn(async move {
                                                    match api.delay_test(&n, "http://www.gstatic.com/generate_204", 5000).await {
                                                        Ok(d) => { let _ = tx.send(Action::DelayResult(n.clone(), Some(d.delay))); }
                                                        Err(error) => { let _ = tx.send(Action::DelayFailed(n, error.to_string())); }
                                                    }
                                                });
                                            }
                                        }
                                        Action::SwitchView(view) => {
                                            app.view = view;
                                            match view {
                                                View::Proxies if app.proxy_groups.is_empty() => {
                                                    let _ = action_tx.send(Action::ProxiesRefresh);
                                                }
                                                View::Connections => {
                                                    let _ = action_tx.send(Action::ConnectionsRefresh);
                                                }
                                                View::Logs => {
                                                    let _ = action_tx.send(Action::LogsRefresh);
                                                }
                                                _ => {}
                                            }
                                        }
                                        Action::CycleFocus => {
                                            app.focus = app.focus.cycle();
                                        }
                                        Action::FocusMenu => {
                                            app.focus = Focus::Menu;
                                        }
                                        Action::FocusContent => {
                                            app.focus = Focus::Content;
                                        }
                                        Action::StartFilter => {
                                            app.filter = Some(String::new());
                                            app.overlay = Some(Overlay::Filter);
                                            app.focus = Focus::Content;
                                        }
                                        Action::ToggleHelp => {
                                            app.overlay = match app.overlay {
                                                Some(Overlay::Help) => None,
                                                _ => Some(Overlay::Help),
                                            };
                                        }
                                        Action::DismissOverlay => {
                                            dismiss_overlay(&mut app);
                                        }
                                        Action::RequestCloseConnection => {
                                            begin_connection_close(&mut app);
                                        }
                                        Action::ConfirmCloseConnection(id) => {
                                            if close_confirmation_is_current(&app, &id) {
                                                let _ = action_tx.send(Action::ConfirmCloseConnection(id));
                                            } else {
                                                app.status_msg = Some("Connection close confirmation expired".into());
                                            }
                                        }
                                        Action::NodeDelayAll => {
                                            let api = manager.api();
                                            let tx = action_tx.clone();
                                            let all_names: Vec<String> = all_node_names(&app.proxy_groups);
                                            tokio::spawn(async move {
                                                for name in all_names {
                                                    match api.delay_test(&name, "http://www.gstatic.com/generate_204", 5000).await {
                                                        Ok(d) => { let _ = tx.send(Action::DelayResult(name, Some(d.delay))); }
                                                        Err(error) => { let _ = tx.send(Action::DelayFailed(name, error.to_string())); }
                                                    }
                                                }
                                            });
                                        }
                                        Action::UpdateProfile => {
                                            app.status_msg = Some("Updating subscriptions...".into());
                                            let selected_uid = app
                                                .profiles
                                                .get(app.selected_index)
                                                .and_then(|item| item.uid.clone())
                                                .map(|u| u.to_string());
                                            let tx = action_tx.clone();
                                            tokio::spawn(async move {
                                                match crate::profile_store::store::ProfileStore::load().await {
                                                    Ok(mut store) => {
                                                        let result = if let Some(uid) = selected_uid {
                                                            store
                                                                .update_remote(&uid, None)
                                                                .await
                                                                .map(|is_current| (uid, is_current))
                                                        } else {
                                                            store.update_all_remote().await.map(|currents| {
                                                                let uid = currents
                                                                    .first()
                                                                    .map(|u| u.to_string())
                                                                    .unwrap_or_default();
                                                                let is_current = !currents.is_empty();
                                                                (uid, is_current)
                                                            })
                                                        };
                                                        match result {
                                                            Ok((uid, is_current)) => {
                                                                let _ = tx.send(Action::ProfileUpdated {
                                                                    uid,
                                                                    is_current,
                                                                });
                                                            }
                                                            Err(error) => {
                                                                let _ = tx.send(Action::ProfileUpdateFailed(
                                                                    error.to_string(),
                                                                ));
                                                            }
                                                        }
                                                    }
                                                    Err(error) => {
                                                        let _ = tx.send(Action::ProfileUpdateFailed(
                                                            error.to_string(),
                                                        ));
                                                    }
                                                }
                                            });
                                        }
                                        Action::ToggleChainMode => {
                                            app.chain_mode = !app.chain_mode;
                                            app.status_msg = Some(if app.chain_mode {
                                                app.chain_nodes.clear();
                                                String::from("Chain edit ON: Enter add, a apply, x clear")
                                            } else {
                                                app.chain_nodes.clear();
                                                String::from("Chain OFF")
                                            });
                                        }
                                        Action::ApplyChain => {
                                            if app.chain_nodes.len() >= 2 {
                                                let nodes = app.chain_nodes.clone();
                                                let api = manager.api();
                                                let tx = action_tx.clone();
                                                app.status_msg = Some("Applying chain...".into());
                                                tokio::spawn(async move {
                                                    match apply_chain_config(&api, &nodes).await {
                                                        Ok(_) => {
                                                            let _ = tx.send(Action::ChainApplied(nodes));
                                                            let _ = tx.send(Action::ProxiesRefresh);
                                                        }
                                                        Err(error) => {
                                                            let _ = tx.send(Action::ChainFailed(error));
                                                        }
                                                    }
                                                });
                                            } else {
                                                app.status_msg = Some("Need >=2 nodes for chain".into());
                                            }
                                        }
                                        Action::ClearChain => {
                                            app.chain_nodes.clear();
                                            app.status_msg = Some("Chain cleared".into());
                                        }
                                        _ => {}
                                    }
                                }
                            }
                        }
                    }
                    Some(Err(_)) | None => break,
                    _ => {}
                }
            }

            action = action_rx.recv() => {
                match action {
                    Some(Action::CoreStarted {
                        version,
                        binary_path,
                        binary_source,
                    }) => {
                        app.core_state = CoreState::Running;
                        app.core_pid = manager.pid();
                        if let Some(version) = version.clone() {
                            app.core_version = Some(version);
                        } else if app.core_version.is_none() {
                            // Attached to an existing controller — ask the API for version.
                            let api = manager.api();
                            let tx = action_tx.clone();
                            tokio::spawn(async move {
                                if let Ok(v) = api.version().await {
                                    let _ = tx.send(Action::CoreStarted {
                                        version: Some(v.version),
                                        binary_path: None,
                                        binary_source: None,
                                    });
                                }
                            });
                        }

                        let pid = app.core_pid.map(|p| p.to_string()).unwrap_or_else(|| "-".into());
                        app.status_msg = Some(match binary_source.as_deref() {
                            Some("downloaded") => format!(
                                "{} · {} · {} · pid {pid}",
                                app.tr("home.core_started"),
                                version.as_deref().unwrap_or("mihomo"),
                                app.tr("home.core_downloaded"),
                            ),
                            Some("cached") => format!(
                                "{} · {} · {} · pid {pid}",
                                app.tr("home.core_started"),
                                version.as_deref().unwrap_or("mihomo"),
                                app.tr("home.core_cached"),
                            ),
                            Some("system") => format!(
                                "{} · {} · {} · pid {pid}",
                                app.tr("home.core_started"),
                                version.as_deref().unwrap_or("mihomo"),
                                app.tr("home.core_system"),
                            ),
                            _ if version.is_some() => format!(
                                "{} · {} · pid {pid}",
                                app.tr("home.core_started"),
                                version.as_deref().unwrap_or("mihomo"),
                            ),
                            _ => app.tr("home.core_attached").into(),
                        });
                        if let Some(path) = binary_path {
                            tracing::info!(target: "mihomo", "core binary: {path}");
                        }
                        let _ = action_tx.send(Action::ProxiesRefresh);
                        let _ = action_tx.send(Action::TrafficRefresh);
                        let _ = action_tx.send(Action::ConnectionsRefresh);
                        let _ = action_tx.send(Action::LogsRefresh);
                    }
                    Some(Action::CoreExited(0)) => {
                        app.core_state = CoreState::Stopped;
                        app.core_pid = None;
                        app.clear_runtime_caches();
                    }
                    Some(Action::CoreError(msg)) => {
                        app.core_state = CoreState::Error(msg.clone());
                        app.core_pid = None;
                        app.status_msg = Some(format!("{}: {msg}", app.tr("status.error")));
                        app.clear_runtime_caches();
                    }
                    Some(Action::ProfileImported) => {
                        app.status_msg = Some("Profile imported successfully".into());
                        if let Ok(store) = crate::profile_store::store::ProfileStore::load().await {
                            app.profiles = store.items();
                            // Auto-select last (newly imported) profile
                            if !app.profiles.is_empty() {
                                let last = app.profiles.len().saturating_sub(1);
                                app.selected_index = last;
                                // Auto-start mihomo if not running, then switch
                                if app.core_state != CoreState::Running {
                                    let m = manager.clone();
                                    tokio::spawn(async move { let _ = m.start().await; });
                                }
                                // Trigger config reload for the new profile
                                if let Some(item) = app.profiles.get(last)
                                    && let Some(ref uid) = item.uid
                                {
                                    let api = manager.api();
                                    let u = uid.clone();
                                    let tx = action_tx.clone();
                                    tokio::spawn(async move {
                                        let body = format!("{{\"path\":\"{u}\"}}");
                                        let _ = api.client
                                            .put("http://localhost/configs")
                                            .header("Content-Type", "application/json")
                                            .body(body)
                                            .send()
                                            .await;
                                        let _ = tx.send(Action::ProxiesRefresh);
                                    });
                                }
                            }
                        }
                    }
                    Some(Action::ProxiesFetched(groups)) => {
                        app.runtime_loading.proxies = false;
                        app.runtime_errors.proxies = None;
                        app.proxy_groups = groups;
                        let expanded_is_available = app
                            .expanded_proxy_group
                            .as_deref()
                            .and_then(|name| app.proxy_groups.get(name))
                            .and_then(|group| group.all.as_ref())
                            .is_some_and(|nodes| !nodes.is_empty());
                        if !expanded_is_available {
                            app.expanded_proxy_group = first_selectable_proxy_group(&app.proxy_groups);
                            app.node_selected_index = 0;
                        }
                        let rows = proxy_display_rows(&app.proxy_groups, app.expanded_proxy_group.as_deref());
                        app.node_selected_index = app.node_selected_index.min(rows.len().saturating_sub(1));
                        let group_count = rows
                            .iter()
                            .filter(|row| matches!(row, ProxyDisplayRow::Group { .. }))
                            .count();
                        let choice_count: usize = app
                            .proxy_groups
                            .values()
                            .filter_map(|group| group.all.as_ref().filter(|nodes| !nodes.is_empty()))
                            .map(Vec::len)
                            .sum();
                        app.status_msg = Some(format!(
                            "{group_count} selectable groups, {choice_count} choices loaded"
                        ));
                    }
                    Some(Action::ProxiesRefresh)
                        if !app.runtime_loading.proxies => {
                            app.runtime_loading.proxies = true;
                            app.runtime_errors.proxies = None;
                            let api = manager.api();
                            let tx = action_tx.clone();
                            tokio::spawn(async move {
                                match api.get_proxies().await {
                                    Ok(data) => {
                                        let _ = tx.send(Action::ProxiesFetched(data.proxies));
                                    }
                                    Err(error) => {
                                        let _ = tx.send(Action::ProxiesFailed(error.to_string()));
                                    }
                                }
                            });
                        }
                    Some(Action::ProxiesFailed(error)) => {
                        app.runtime_loading.proxies = false;
                        app.runtime_errors.proxies = Some(error);
                    }
                    Some(Action::DelayResult(name, delay)) => {
                        app.delay_map.insert(name, delay);
                        if let Some(delay) = delay {
                            app.status_msg = Some(format!("Delay: {delay}ms"));
                        }
                    }
                    Some(Action::DelayFailed(name, error)) => {
                        app.delay_map.insert(name.clone(), None);
                        app.status_msg = Some(format!("Delay failed for {name}: {error}"));
                    }
                    Some(Action::ChainApplied(nodes)) => {
                        app.chain_mode = false;
                        app.chain_nodes.clear();
                        app.status_msg = Some(format!("Chain applied: {}", nodes.join(" -> ")));
                    }
                    Some(Action::ChainFailed(error)) => {
                        app.status_msg = Some(format!("Chain not applied: {error}"));
                    }
                    Some(Action::TrafficRefresh)
                        if !app.runtime_loading.traffic => {
                            app.runtime_loading.traffic = true;
                            app.runtime_errors.traffic = None;
                            let api = manager.api();
                            let tx = action_tx.clone();
                            tokio::spawn(async move {
                                if let Err(error) = receive_traffic_stream(api, tx.clone()).await {
                                    let _ = tx.send(Action::TrafficFailed(error));
                                }
                            });
                        }
                    Some(Action::TrafficFetched(traffic)) => {
                        app.runtime_errors.traffic = None;
                        app.traffic = Some(traffic);
                    }
                    Some(Action::TrafficFailed(error)) => {
                        app.runtime_loading.traffic = false;
                        app.runtime_errors.traffic = Some(error);
                    }
                    Some(Action::ConnectionsRefresh)
                        if !app.runtime_loading.connections => {
                            app.runtime_loading.connections = true;
                            app.runtime_errors.connections = None;
                            let api = manager.api();
                            let tx = action_tx.clone();
                            tokio::spawn(async move {
                                match api.get_connections().await {
                                    Ok(data) => {
                                        let _ = tx.send(Action::ConnectionsFetched(data.connections));
                                    }
                                    Err(error) => {
                                        let _ = tx.send(Action::ConnectionsFailed(error.to_string()));
                                    }
                                }
                            });
                        }
                    Some(Action::ConnectionsFetched(connections)) => {
                        app.runtime_loading.connections = false;
                        app.runtime_errors.connections = None;
                        if app
                            .selected_connection_id
                            .as_ref()
                            .is_some_and(|id| !connections.iter().any(|connection| &connection.id == id))
                        {
                            app.selected_connection_id = None;
                            app.pending_connection_close = None;
                        }
                        app.connections = connections;
                        let ids = visible_connection_ids(&app);
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
                    Some(Action::ConnectionsFailed(error)) => {
                        app.runtime_loading.connections = false;
                        app.runtime_errors.connections = Some(error);
                    }
                    Some(Action::LogsRefresh)
                        if !app.runtime_loading.logs => {
                            app.runtime_loading.logs = true;
                            app.runtime_errors.logs = None;
                            let api = manager.api();
                            let tx = action_tx.clone();
                            tokio::spawn(async move {
                                if let Err(error) = receive_log_stream(api, tx.clone()).await {
                                    let _ = tx.send(Action::LogsFailed(error));
                                }
                            });
                        }
                    Some(Action::LogReceived(log)) => {
                        app.runtime_errors.logs = None;
                        app.logs.push(log);
                        const MAX_LOG_ENTRIES: usize = 1_000;
                        if app.logs.len() > MAX_LOG_ENTRIES {
                            let excess = app.logs.len() - MAX_LOG_ENTRIES;
                            app.logs.drain(..excess);
                        }
                        app.log_selected_index = app
                            .log_selected_index
                            .min(visible_log_count(&app).saturating_sub(1));
                    }
                    Some(Action::LogsFailed(error)) => {
                        app.runtime_loading.logs = false;
                        app.runtime_errors.logs = Some(error);
                    }
                    Some(Action::ConfirmCloseConnection(id))
                        if close_confirmation_is_current(&app, &id) => {
                            app.pending_connection_close = None;
                            app.overlay = None;
                            let api = manager.api();
                            let tx = action_tx.clone();
                            tokio::spawn(async move {
                                match api.close_connection(&id).await {
                                    Ok(()) => {
                                        let _ = tx.send(Action::ConnectionClosed(id));
                                    }
                                    Err(error) => {
                                        let _ = tx.send(Action::CloseConnectionFailed {
                                            id,
                                            error: error.to_string(),
                                        });
                                    }
                                }
                            });
                        }
                    Some(Action::ConnectionClosed(id)) => {
                        app.status_msg = Some(format!("Closed connection {id}"));
                        app.selected_connection_id = None;
                        let _ = action_tx.send(Action::ConnectionsRefresh);
                    }
                    Some(Action::CloseConnectionFailed { id, error }) => {
                        app.runtime_errors.connections = Some(format!("Could not close {id}: {error}"));
                    }
                    Some(Action::ConfirmImport(url)) => {
                        let tx = action_tx.clone();
                        tokio::spawn(async move {
                            match crate::profile_store::store::ProfileStore::load().await {
                                Ok(mut store) => match store.import_url(&url, None).await {
                                    Ok(_) => {
                                        let _ = tx.send(Action::ProfileImported);
                                    }
                                    Err(e) => {
                                        let _ = tx.send(Action::ProfileImportFailed(e.to_string()));
                                    }
                                },
                                Err(e) => {
                                    let _ = tx.send(Action::ProfileImportFailed(e.to_string()));
                                }
                            }
                        });
                    }
                    Some(Action::ProfileImportFailed(error)) => {
                        app.status_msg = Some(format!("Import failed: {error}"));
                    }
                    Some(Action::ProfileUpdated { uid, is_current }) => {
                        app.status_msg = Some(if uid.is_empty() {
                            "Subscriptions updated".into()
                        } else {
                            format!("Updated profile {uid}")
                        });
                        if let Ok(store) = crate::profile_store::store::ProfileStore::load().await {
                            app.profiles = store.items();
                        }
                        if is_current && app.core_state == CoreState::Running {
                            let api = manager.api();
                            let tx = action_tx.clone();
                            let reload_uid = uid.clone();
                            tokio::spawn(async move {
                                if !reload_uid.is_empty() {
                                    let body = format!("{{\"path\":\"{reload_uid}\"}}");
                                    let _ = api
                                        .client
                                        .put("http://localhost/configs?force=true")
                                        .header("Content-Type", "application/json")
                                        .body(body)
                                        .send()
                                        .await;
                                }
                                let _ = tx.send(Action::ProxiesRefresh);
                            });
                        }
                    }
                    Some(Action::ProfileUpdateFailed(error)) => {
                        app.status_msg = Some(format!("Update failed: {error}"));
                    }
                    None => break,
                    _ => {}
                }
            }

            _ = render_tick.tick() => {
                if app.view != rendered_view {
                    // Orca's terminal renderer can retain differential cells across
                    // alternate-screen view changes. Force one clean repaint per route.
                    guard.reset_screen()?;
                    rendered_view = app.view;
                }
                guard.terminal_mut().draw(|f| crate::ui::draw(f, &app))?;
            }

            _ = runtime_refresh_tick.tick(), if app.core_state == CoreState::Running => {
                let _ = action_tx.send(Action::TrafficRefresh);
                match app.view {
                    View::Connections => {
                        let _ = action_tx.send(Action::ConnectionsRefresh);
                    }
                    View::Logs => {
                        let _ = action_tx.send(Action::LogsRefresh);
                    }
                    _ => {}
                }
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    #[test]
    fn all_node_names_deduplicates_choices_shared_by_multiple_groups() {
        let mut groups = HashMap::new();
        groups.insert(
            "first".to_string(),
            crate::mihomo_api::types::ProxyGroup {
                group_type: "Selector".to_string(),
                now: None,
                all: Some(vec!["Tokyo".to_string(), "Singapore".to_string()]),
                history: None,
            },
        );
        groups.insert(
            "second".to_string(),
            crate::mihomo_api::types::ProxyGroup {
                group_type: "Selector".to_string(),
                now: None,
                all: Some(vec!["Tokyo".to_string(), "Los Angeles".to_string()]),
                history: None,
            },
        );

        assert_eq!(all_node_names(&groups), vec!["Los Angeles", "Singapore", "Tokyo"]);
    }

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

        let initial = drain_ndjson::<TrafficData>(&mut buffer, br#"{"up":1,"down":2}"#).expect("partial record");
        assert!(initial.is_empty());

        let entries = drain_ndjson::<TrafficData>(&mut buffer, b"\n{\"up\":3,\"down\":4}\n").expect("complete records");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].up, 1);
        assert_eq!(entries[1].down, 4);
    }

    #[test]
    fn ndjson_parser_ignores_blank_lines() {
        let mut buffer = Vec::new();
        let entries = drain_ndjson::<LogEntry>(&mut buffer, b"\n {\"type\":\"info\",\"payload\":\"ready\"}\n\n")
            .expect("valid log record");

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].level, "info");
        assert_eq!(entries[0].payload, "ready");
    }
}
