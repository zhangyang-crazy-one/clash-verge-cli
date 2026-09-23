//! `connections [list|close|close-all]`.

use serde::Serialize;

use crate::mihomo_api::types::ConnectionInfo;
use crate::mihomo_manager::manager::MihomoManager;

#[derive(Serialize)]
struct Row<'a> {
    id: &'a str,
    host: &'a str,
    network: &'a str,
    rule: &'a str,
    chains: String,
    upload: u64,
    download: u64,
    start: &'a str,
}

impl<'a> Row<'a> {
    fn of(connection: &'a ConnectionInfo) -> Self {
        let meta = connection.metadata.as_ref();
        Self {
            id: &connection.id,
            host: meta
                .and_then(|m| m.host.as_deref())
                .filter(|h| !h.is_empty())
                .unwrap_or("-"),
            network: meta.and_then(|m| m.network.as_deref()).unwrap_or("-"),
            rule: connection.rule.as_deref().unwrap_or("-"),
            chains: connection.chains.as_deref().unwrap_or_default().join(" → "),
            upload: connection.upload,
            download: connection.download,
            start: &connection.start,
        }
    }
}

pub async fn list(manager: &MihomoManager, json: bool) -> anyhow::Result<()> {
    let api = super::running_api(manager).await?;
    let connections = api.get_connections().await?.connections;
    let rows: Vec<Row<'_>> = connections.iter().map(Row::of).collect();
    if json {
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }
    if rows.is_empty() {
        println!("(no connections)");
        return Ok(());
    }
    let cells = rows.iter().map(|row| {
        vec![
            row.id.to_string(),
            row.network.to_string(),
            row.host.to_string(),
            row.rule.to_string(),
            if row.chains.is_empty() {
                "-".into()
            } else {
                row.chains.clone()
            },
            super::format_bytes(row.upload),
            super::format_bytes(row.download),
        ]
    });
    print!(
        "{}",
        super::table(&["ID", "NET", "HOST", "RULE", "CHAIN", "UP", "DOWN"], cells)
    );
    Ok(())
}

pub async fn close(manager: &MihomoManager, id: &str) -> anyhow::Result<()> {
    let api = super::running_api(manager).await?;
    api.close_connection(id).await?;
    println!("closed {id}");
    Ok(())
}

pub async fn close_all(manager: &MihomoManager) -> anyhow::Result<()> {
    let api = super::running_api(manager).await?;
    api.close_all_connections().await?;
    println!("closed all connections");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mihomo_api::types::ConnectionMeta;

    #[test]
    fn rows_fill_missing_metadata_with_dashes() {
        let bare = ConnectionInfo {
            id: "c1".into(),
            metadata: None,
            upload: 1,
            download: 2,
            start: "now".into(),
            rule: None,
            chains: None,
        };
        let row = Row::of(&bare);
        assert_eq!(
            (row.host, row.network, row.rule, row.chains.as_str()),
            ("-", "-", "-", "")
        );

        let full = ConnectionInfo {
            metadata: Some(ConnectionMeta {
                host: Some("example.com".into()),
                network: Some("tcp".into()),
            }),
            rule: Some("Match".into()),
            chains: Some(vec!["Tokyo".into(), "Proxy".into()]),
            ..bare
        };
        let row = Row::of(&full);
        assert_eq!(row.host, "example.com");
        assert_eq!(row.chains, "Tokyo → Proxy");
    }
}
