// Stub for PrfItem::from_url adapter (replaces upstream NetworkManager).
// Full implementation in plan 03-01 task 2.

use clash_verge_core::config::PrfItem;

use super::fetch;

/// Create a PrfItem by fetching and parsing a subscription URL.
///
/// Subscription APIs return a complete Clash config YAML (with `proxies`,
/// `proxy-groups`, `rules`, etc.), not a list of PrfItem structs. We save the
/// raw response body to disk so Mihomo can load it, and record the profile
/// metadata in profiles.yaml.
pub async fn from_url(url: &str, name: &str) -> anyhow::Result<PrfItem> {
    let result = fetch::fetch_subscription(url, &[]).await?;
    let body = result.body;

    // Validate that the response looks like a Clash config (not HTML error page
    // or empty body).
    if body.trim().is_empty() {
        anyhow::bail!("subscription returned an empty response");
    }
    if body.trim().starts_with('<') {
        anyhow::bail!(
            "subscription returned HTML (possible 403/block or invalid URL): {}",
            body.trim().chars().take(200).collect::<String>()
        );
    }

    // Try parsing as a Clash config mapping to validate it's legitimate.
    // Most subscription APIs return a full config YAML, not a PrfItem list.
    let is_valid_yaml = serde_yaml_ng::from_str::<serde_yaml_ng::Mapping>(&body).is_ok()
        || {
            // Also try base64-decode for legacy subscription formats (e.g.
            // base64-encoded proxy list).
            use base64::Engine as _;
            let decoded = base64::engine::general_purpose::STANDARD
                .decode(body.trim())
                .unwrap_or_default();
            !decoded.is_empty()
                && String::from_utf8(decoded)
                    .ok()
                    .is_some_and(|s| serde_yaml_ng::from_str::<serde_yaml_ng::Mapping>(&s).is_ok())
        };

    if !is_valid_yaml {
        anyhow::bail!("subscription returned unrecognized content (not YAML nor base64-encoded YAML)");
    }

    // Build the profile item.
    let uid = format!("R{}", uuid::Uuid::new_v4().to_string().replace('-', ""));
    let file = format!("{uid}.yaml");

    let item = PrfItem {
        uid: Some(uid.into()),
        name: Some(name.into()),
        url: Some(url.into()),
        itype: Some("remote".into()),
        file: Some(file.into()),
        file_data: Some(body.into()),
        updated: Some(chrono::Utc::now().timestamp() as usize),
        ..Default::default()
    };

    Ok(item)
}
