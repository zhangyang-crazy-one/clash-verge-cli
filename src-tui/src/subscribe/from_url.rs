//! Remote profile import — port of GUI `PrfItem::from_url` (origin/dev).
//! Network stays in src-tui; SSRF checks remain enabled for CLI/SSH use.

use std::collections::HashMap;

use anyhow::{Context, bail};
use clash_verge_core::config::{PrfExtra, PrfItem, PrfOption};
use clash_verge_core::utils::help;
use serde_yaml_ng::Mapping;
use smartstring::alias::String as SmartString;
use url::Url;

use super::fetch;

/// Outcome of building a remote profile: the remote item plus any new
/// enhance-chain fragments that must be appended before it.
#[derive(Debug)]
pub struct RemoteProfileBundle {
    pub item: PrfItem,
    pub fragments: Vec<PrfItem>,
}

/// Create a remote `PrfItem` by fetching and validating a subscription URL.
pub async fn from_url(
    url: &str,
    name: Option<&str>,
    desc: Option<&str>,
    option: Option<&PrfOption>,
) -> anyhow::Result<RemoteProfileBundle> {
    let cleaned = fix_dirty_url(url)?;
    let allowlist = trusted_hosts_allowlist(option);
    let result = fetch::fetch_subscription(cleaned.as_str(), option, &allowlist).await?;

    let allow_auto_update = Some(allow_auto_update_enabled(option));
    let mut merge = option.and_then(|o| o.merge.clone());
    let mut script = option.and_then(|o| o.script.clone());
    let mut rules = option.and_then(|o| o.rules.clone());
    let mut proxies = option.and_then(|o| o.proxies.clone());
    let mut groups = option.and_then(|o| o.groups.clone());
    let mut update_interval = option.and_then(|o| o.update_interval);

    let extra = parse_subscription_userinfo(&result.headers);
    let home = result
        .headers
        .get("profile-web-page-url")
        .map(|s| SmartString::from(s.as_str()));

    if update_interval.is_none()
        && let Some(raw) = result.headers.get("profile-update-interval")
        && let Ok(hours) = raw.parse::<u64>()
    {
        update_interval = Some(hours * 60);
    }

    let filename = parse_content_disposition_name(&result.headers)
        .or_else(|| get_last_part_and_decode(cleaned.as_str()))
        .unwrap_or_else(|| "Remote File".into());

    let resolved_name = name
        .map(SmartString::from)
        .unwrap_or_else(|| SmartString::from(filename.as_str()));

    let data = result.body.trim_start_matches('\u{feff}');
    validate_clash_yaml(data)?;

    let mut fragments = Vec::new();
    ensure_chain_uid("merge", &mut merge, || PrfItem::from_merge(None), &mut fragments)?;
    ensure_chain_uid("script", &mut script, || PrfItem::from_script(None), &mut fragments)?;
    ensure_chain_uid("rules", &mut rules, PrfItem::from_rules, &mut fragments)?;
    ensure_chain_uid("proxies", &mut proxies, PrfItem::from_proxies, &mut fragments)?;
    ensure_chain_uid("groups", &mut groups, PrfItem::from_groups, &mut fragments)?;

    let uid = SmartString::from(help::get_uid("R"));
    let file = SmartString::from(format!("{uid}.yaml"));

    let item = PrfItem {
        uid: Some(uid),
        itype: Some("remote".into()),
        name: Some(resolved_name),
        desc: desc.map(SmartString::from),
        file: Some(file),
        url: Some(SmartString::from(cleaned.as_str())),
        selected: None,
        extra,
        option: Some(PrfOption {
            update_interval,
            merge,
            script,
            rules,
            proxies,
            groups,
            allow_auto_update,
            // Preserve fetch-related options the caller supplied.
            user_agent: option.and_then(|o| o.user_agent.clone()),
            with_proxy: option.and_then(|o| o.with_proxy),
            self_proxy: option.and_then(|o| o.self_proxy),
            timeout_seconds: option.and_then(|o| o.timeout_seconds),
            danger_accept_invalid_certs: option.and_then(|o| o.danger_accept_invalid_certs),
            trusted_hosts: option.and_then(|o| o.trusted_hosts.clone()),
        }),
        home,
        updated: Some(chrono::Local::now().timestamp() as usize),
        file_data: Some(SmartString::from(data)),
    };

    Ok(RemoteProfileBundle { item, fragments })
}

/// Merge a caller's interval/auto-update overrides into a fallback attempt,
/// keeping the attempt's proxy strategy.
fn merge_import_option(base: &PrfOption, user: Option<&PrfOption>) -> PrfOption {
    PrfOption::merge(Some(base), user).unwrap_or_else(|| base.clone())
}

pub async fn import_with_fallback(
    url: &str,
    name: Option<&str>,
    user: Option<&PrfOption>,
) -> anyhow::Result<RemoteProfileBundle> {
    let attempts = [
        PrfOption {
            with_proxy: Some(true),
            self_proxy: Some(false),
            ..Default::default()
        },
        PrfOption {
            with_proxy: Some(false),
            self_proxy: Some(true),
            ..Default::default()
        },
        PrfOption::default(),
    ];

    let mut last_err = None;
    for base in &attempts {
        let merged = merge_import_option(base, user);
        match from_url(url, name, None, Some(&merged)).await {
            Ok(bundle) => return Ok(bundle),
            Err(err) => last_err = Some(err),
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("import failed")))
}

/// GUI-style update retries: current options → self_proxy → with_proxy.
pub async fn update_with_fallback(url: &str, option: Option<&PrfOption>) -> anyhow::Result<RemoteProfileBundle> {
    let mut merged = PrfOption::merge(option, None).unwrap_or_default();

    if let Ok(bundle) = from_url(url, None, None, Some(&merged)).await {
        return Ok(bundle);
    }

    merged.self_proxy = Some(true);
    merged.with_proxy = Some(false);
    if let Ok(bundle) = from_url(url, None, None, Some(&merged)).await {
        return Ok(bundle);
    }

    merged.self_proxy = Some(false);
    merged.with_proxy = Some(true);
    from_url(url, None, None, Some(&merged)).await
}

/// Validate that body is Clash YAML containing proxies or proxy-providers.
pub fn validate_clash_yaml(data: &str) -> anyhow::Result<Mapping> {
    let yaml = serde_yaml_ng::from_str::<Mapping>(data).context("the remote profile data is invalid yaml")?;
    if !yaml.contains_key("proxies") && !yaml.contains_key("proxy-providers") {
        bail!("profile does not contain `proxies` or `proxy-providers`");
    }
    Ok(yaml)
}

pub fn parse_subscription_userinfo(headers: &HashMap<String, String>) -> Option<PrfExtra> {
    for (key, value) in headers {
        let key_lower = key.to_ascii_lowercase();
        if key_lower
            .strip_suffix("subscription-userinfo")
            .is_some_and(|prefix| prefix.is_empty() || prefix.ends_with('-'))
        {
            return Some(PrfExtra {
                upload: help::parse_str(value, "upload").unwrap_or(0),
                download: help::parse_str(value, "download").unwrap_or(0),
                total: help::parse_str(value, "total").unwrap_or(0),
                expire: help::parse_str(value, "expire").unwrap_or(0),
            });
        }
    }
    None
}

fn ensure_chain_uid<F>(
    _label: &str,
    slot: &mut Option<SmartString>,
    factory: F,
    fragments: &mut Vec<PrfItem>,
) -> anyhow::Result<()>
where
    F: FnOnce() -> anyhow::Result<PrfItem>,
{
    if slot.is_some() {
        return Ok(());
    }
    let item = factory()?;
    *slot = item.uid.clone();
    fragments.push(item);
    Ok(())
}

/// Build the SSRF allowlist from the caller's `trusted_hosts` option.
///
/// Entries are normalized to the bare-host form `ssrf::check_url_host`
/// compares against (`url::Url::host_str`): values that parse as URLs are
/// reduced to their host, everything else is trimmed and passed through
/// unchanged. An absent option yields an empty allowlist, preserving the
/// default SSRF protection.
pub fn trusted_hosts_allowlist(option: Option<&PrfOption>) -> Vec<std::string::String> {
    option
        .and_then(|o| o.trusted_hosts.as_ref())
        .map(|hosts| {
            hosts
                .iter()
                .filter_map(|raw| normalize_trusted_host(raw.as_str()))
                .collect()
        })
        .unwrap_or_default()
}

/// Normalize one user-supplied trusted-host entry into the bare host form
/// the SSRF checker compares against (its `extract_host` returns
/// `Url::host_str`). Full URLs become their host; bare hostnames/IPs pass
/// through trimmed.
pub fn normalize_trusted_host(raw: &str) -> Option<std::string::String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Ok(parsed) = Url::parse(trimmed) {
        return parsed.host_str().map(str::to_string);
    }
    Some(trimmed.to_string())
}

/// Merge a normalized trusted host into an existing allowlist, preserving
/// every existing entry (deduplicated) and appending the new host.
///
/// Used by the refresh trust flow: confirming persists the host into the
/// profile's stored `option.trusted_hosts` without dropping hosts the user
/// already trusted. Returns `None` when the entry does not normalize (the
/// caller should not persist it). Accepts any string-ish element so both the
/// stored `SmartString` option and the std-String allowlist can merge.
pub fn merge_trusted_host<S: AsRef<str>>(existing: Option<&[S]>, raw: &str) -> Option<Vec<std::string::String>> {
    let host = normalize_trusted_host(raw)?;
    let mut hosts: Vec<std::string::String> = existing
        .map(|list| {
            list.iter()
                .filter_map(|entry| normalize_trusted_host(entry.as_ref()))
                .collect()
        })
        .unwrap_or_default();
    if !hosts.iter().any(|entry| entry == &host) {
        hosts.push(host);
    }
    Some(hosts)
}

fn allow_auto_update_enabled(option: Option<&PrfOption>) -> bool {
    option.and_then(|o| o.allow_auto_update).unwrap_or(true)
}

/// Fix URLs where query parameters were incorrectly appended to the path.
pub fn fix_dirty_url(input: &str) -> anyhow::Result<Url> {
    let mut url = Url::parse(input).with_context(|| format!("failed to parse subscription URL: {input}"))?;

    if url.query().is_none() && url.path().contains('&') {
        let path = url.path().to_string();
        if let Some((clean_path, dirty_params)) = path.split_once('&') {
            url.set_path(clean_path);
            url.query_pairs_mut()
                .extend_pairs(url::form_urlencoded::parse(dirty_params.as_bytes()));
        }
    }

    Ok(url)
}

fn parse_content_disposition_name(headers: &HashMap<String, String>) -> Option<std::string::String> {
    let value = headers.get("content-disposition")?;
    let filename = format!("{value:?}");
    let filename = filename.trim_matches('"');
    if let Some(encoded) = help::parse_str::<std::string::String>(filename, "filename*") {
        let decoded = percent_encoding::percent_decode(encoded.as_bytes())
            .decode_utf8()
            .ok()?;
        return decoded.split("''").last().map(|s| s.to_string());
    }
    if let Some(plain) = help::parse_str::<std::string::String>(filename, "filename") {
        return Some(plain.trim_matches('"').to_string());
    }
    None
}

fn get_last_part_and_decode(url: &str) -> Option<std::string::String> {
    let path = url.split('?').next().unwrap_or("");
    let last = path.split('/').next_back().filter(|s| !s.is_empty())?;
    Some(
        percent_encoding::percent_decode_str(last)
            .decode_utf8_lossy()
            .into_owned(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_requires_proxies_or_providers() {
        assert!(validate_clash_yaml("proxies: []\n").is_ok());
        assert!(validate_clash_yaml("proxy-providers: {}\n").is_ok());
        assert!(validate_clash_yaml("port: 7890\n").is_err());
        assert!(validate_clash_yaml("not: [yaml").is_err());
    }

    #[test]
    fn fix_dirty_url_moves_ampersand_query() {
        let fixed = match fix_dirty_url("https://example.com/path&token=abc&flag=1") {
            Ok(url) => url,
            Err(error) => panic!("dirty url with ampersand query is fixable: {error}"),
        };
        assert_eq!(fixed.path(), "/path");
        assert!(fixed.query().unwrap().contains("token=abc"));
    }

    #[test]
    fn userinfo_header_variants_parse() {
        let mut headers = HashMap::new();
        headers.insert(
            "subscription-userinfo".into(),
            "upload=1; download=2; total=3; expire=4".into(),
        );
        let extra = match parse_subscription_userinfo(&headers) {
            Some(extra) => extra,
            None => panic!("subscription-userinfo header present"),
        };
        assert_eq!(extra.upload, 1);
        assert_eq!(extra.download, 2);
        assert_eq!(extra.total, 3);
        assert_eq!(extra.expire, 4);

        headers.clear();
        headers.insert(
            "x-amz-meta-subscription-userinfo".into(),
            "upload=10; download=20; total=30; expire=40".into(),
        );
        let extra = match parse_subscription_userinfo(&headers) {
            Some(extra) => extra,
            None => panic!("x-amz-meta-subscription-userinfo header present"),
        };
        assert_eq!(extra.upload, 10);
    }

    #[test]
    fn auto_update_defaults_to_enabled() {
        assert!(allow_auto_update_enabled(None));
        let disabled = PrfOption {
            allow_auto_update: Some(false),
            ..Default::default()
        };
        assert!(!allow_auto_update_enabled(Some(&disabled)));
    }

    use crate::subscribe::ssrf;

    #[test]
    fn trusted_hosts_option_reaches_ssrf_allowlist() {
        // URL-form, bare-IP, and whitespace-padded entries all normalize to
        // the bare host form the SSRF checker compares against.
        let option = PrfOption {
            trusted_hosts: Some(vec![
                "https://sub.example.com/path?token=abc".into(),
                "192.168.1.1".into(),
                "  trusted.example.org  ".into(),
            ]),
            ..Default::default()
        };

        let allowlist = trusted_hosts_allowlist(Some(&option));
        assert_eq!(
            allowlist,
            vec![
                "sub.example.com".to_string(),
                "192.168.1.1".to_string(),
                "trusted.example.org".to_string(),
            ]
        );
    }

    #[test]
    fn matching_trusted_host_bypasses_ssrf_block() {
        // Without the option the private host is blocked...
        assert!(ssrf::check_url_host("http://192.168.1.1/sub", &[]).is_err());

        // ...and a matching trusted host in the option reaches the allowlist.
        let option = PrfOption {
            trusted_hosts: Some(vec!["192.168.1.1".into()]),
            ..Default::default()
        };
        let allowlist = trusted_hosts_allowlist(Some(&option));
        assert!(ssrf::check_url_host("http://192.168.1.1/sub", &allowlist).is_ok());
    }

    #[test]
    fn unrelated_trusted_host_preserves_ssrf_protection() {
        // Trusting one host must not open up unrelated private/loopback hosts.
        let option = PrfOption {
            trusted_hosts: Some(vec!["trusted.example.org".into()]),
            ..Default::default()
        };
        let allowlist = trusted_hosts_allowlist(Some(&option));

        assert!(ssrf::check_url_host("http://trusted.example.org/sub", &allowlist).is_ok());
        assert!(ssrf::check_url_host("http://192.168.1.1/sub", &allowlist).is_err());
        assert!(ssrf::check_url_host("http://127.0.0.1/sub", &allowlist).is_err());
    }

    #[test]
    fn absent_trusted_hosts_yields_empty_allowlist() {
        assert!(trusted_hosts_allowlist(None).is_empty());
        assert!(trusted_hosts_allowlist(Some(&PrfOption::default())).is_empty());
    }

    #[test]
    fn merge_trusted_host_keeps_existing_entries_and_dedups() {
        let existing = vec!["sub.example.com".to_string(), "192.168.1.1".to_string()];

        // A new host is appended after the existing ones.
        let merged = merge_trusted_host(Some(&existing), "8ry1xfih.doggygosubs.com").expect("merge");
        assert_eq!(
            merged,
            vec![
                "sub.example.com".to_string(),
                "192.168.1.1".to_string(),
                "8ry1xfih.doggygosubs.com".to_string()
            ]
        );

        // A duplicate host (URL-form or bare) is not added twice.
        let again = merge_trusted_host(Some(&merged), "https://sub.example.com/path?token=abc").expect("merge");
        assert_eq!(again.len(), merged.len());
        assert_eq!(again, merged);
    }

    #[test]
    fn merge_trusted_host_starts_empty_and_rejects_blank_input() {
        assert_eq!(
            merge_trusted_host::<std::string::String>(None, "8ry1xfih.doggygosubs.com"),
            Some(vec!["8ry1xfih.doggygosubs.com".to_string()])
        );
        assert_eq!(
            merge_trusted_host::<std::string::String>(None, "  "),
            None,
            "blank entries never persist"
        );
        assert_eq!(merge_trusted_host::<std::string::String>(None, ""), None);
    }

    #[test]
    fn import_option_merge_keeps_strategy_and_takes_user_flags() {
        let base = PrfOption {
            with_proxy: Some(true),
            self_proxy: Some(false),
            ..Default::default()
        };
        // CLI `--update-interval 15 --no-auto-update`.
        let user = PrfOption {
            update_interval: Some(15),
            allow_auto_update: Some(false),
            ..Default::default()
        };

        let merged = merge_import_option(&base, Some(&user));
        // The attempt's proxy strategy survives...
        assert_eq!(merged.with_proxy, Some(true));
        assert_eq!(merged.self_proxy, Some(false));
        // ...while the caller's flags win.
        assert_eq!(merged.update_interval, Some(15));
        assert_eq!(merged.allow_auto_update, Some(false));

        // Without user flags the attempt stands alone.
        let bare = merge_import_option(&base, None);
        assert_eq!(bare.update_interval, None);
        assert_eq!(bare.allow_auto_update, None);
    }
}
