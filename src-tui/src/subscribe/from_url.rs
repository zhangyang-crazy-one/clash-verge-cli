// Stub for PrfItem::from_url adapter (replaces upstream NetworkManager).
// Full implementation in plan 03-01 task 2.

use clash_verge_core::config::PrfItem;

use super::fetch;

/// Create a PrfItem by fetching and parsing a subscription URL.
pub async fn from_url(url: &str, name: &str) -> anyhow::Result<PrfItem> {
    let result = fetch::fetch_subscription(url, &[]).await?;
    let body = result.body;

    // Try parsing as YAML first, then as base64-encoded profile list
    let items = if let Ok(items) = serde_yaml_ng::from_str::<Vec<PrfItem>>(&body) {
        items
    } else {
        // Try base64 decode for legacy subscription formats
        use base64::Engine as _;
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(body.trim())
            .unwrap_or_default();
        String::from_utf8(decoded)
            .ok()
            .and_then(|s| serde_yaml_ng::from_str::<Vec<PrfItem>>(&s).ok())
            .unwrap_or_default()
    };

    if items.is_empty() {
        anyhow::bail!("no valid proxy nodes found in subscription");
    }

    let mut item = items.into_iter().next().unwrap();
    item.name = Some(name.into());
    item.url = Some(url.into());
    item.uid = Some(uuid::Uuid::new_v4().to_string().into());
    item.itype = Some("remote".into());

    Ok(item)
}
