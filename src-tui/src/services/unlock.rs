//! Streaming and AI service availability ("unlock") checks.
//!
//! Every request goes through the running core's HTTP proxy port, so a
//! result describes the current exit node, not this machine's own network.
//! Each check fetches a page or two and classifies the response with a pure
//! function below; the classifiers are what the tests pin down. Sites change
//! their pages without notice, so an unrecognized answer is reported as a
//! failure with the reason instead of a guess.

use std::time::Duration;

use serde::Serialize;

/// Browser-like: several services serve bots a different page.
const USER_AGENT: &str =
    "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/132.0.0.0 Safari/537.36";
const TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, clap::ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum Service {
    Netflix,
    #[value(name = "youtube")]
    #[serde(rename = "youtube")]
    YoutubePremium,
    #[value(name = "disney")]
    #[serde(rename = "disney")]
    DisneyPlus,
    #[value(name = "chatgpt")]
    #[serde(rename = "chatgpt")]
    ChatGpt,
    Claude,
    Gemini,
    #[value(name = "tiktok")]
    #[serde(rename = "tiktok")]
    TikTok,
}

impl Service {
    pub const ALL: [Self; 7] = [
        Self::Netflix,
        Self::YoutubePremium,
        Self::DisneyPlus,
        Self::ChatGpt,
        Self::Claude,
        Self::Gemini,
        Self::TikTok,
    ];

    pub const fn name(self) -> &'static str {
        match self {
            Self::Netflix => "Netflix",
            Self::YoutubePremium => "YouTube Premium",
            Self::DisneyPlus => "Disney+",
            Self::ChatGpt => "ChatGPT",
            Self::Claude => "Claude",
            Self::Gemini => "Gemini",
            Self::TikTok => "TikTok",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Available,
    /// Netflix: only Netflix originals are available.
    OriginalsOnly,
    Unavailable,
    /// The check could not reach the service or did not recognize its answer.
    Failed,
}

impl Verdict {
    pub const fn label_key(self) -> &'static str {
        match self {
            Self::Available => "unlock.available",
            Self::OriginalsOnly => "unlock.originals_only",
            Self::Unavailable => "unlock.unavailable",
            Self::Failed => "unlock.failed",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CheckResult {
    pub service: Service,
    pub verdict: Verdict,
    /// Region the service sees, when it tells (ISO code as the site gives it).
    pub region: Option<String>,
    /// Why, for anything but a plain answer.
    pub detail: Option<String>,
}

impl CheckResult {
    const fn new(service: Service, verdict: Verdict) -> Self {
        Self {
            service,
            verdict,
            region: None,
            detail: None,
        }
    }

    fn region(mut self, region: Option<String>) -> Self {
        self.region = region;
        self
    }

    fn detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    fn failed(service: Service, detail: impl Into<String>) -> Self {
        Self::new(service, Verdict::Failed).detail(detail)
    }
}

/// Results of one run, with the exit route the requests took.
#[derive(Debug, Clone, Serialize)]
pub struct Report {
    /// `["Proxy", "Auto", "Tokyo 01"]`: from the mode's start group to the
    /// node; empty when the core did not report its proxies.
    pub exit: Vec<String>,
    pub results: Vec<CheckResult>,
}

/// Check `services` (all when empty) through the running core behind `api`.
pub async fn run(api: &crate::mihomo_api::MihomoApi, services: &[Service]) -> anyhow::Result<Report> {
    let services = if services.is_empty() {
        &Service::ALL[..]
    } else {
        services
    };
    let port = api.http_proxy_port().await?;
    let exit = match (api.get_proxies().await, api.get_mode().await) {
        (Ok(proxies), Ok(mode)) => crate::app::outbound_chain(&proxies.proxies, &mode),
        _ => Vec::new(),
    };
    let client = proxied_client(port)?;
    Ok(Report {
        exit,
        results: check_all(&client, services).await,
    })
}

/// An HTTP client that sends everything through the core's proxy port.
pub fn proxied_client(proxy_port: u16) -> anyhow::Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .proxy(reqwest::Proxy::all(format!("http://127.0.0.1:{proxy_port}"))?)
        .user_agent(USER_AGENT)
        .timeout(TIMEOUT)
        .build()?)
}

/// Run `services` concurrently; results come back in the order asked.
pub async fn check_all(client: &reqwest::Client, services: &[Service]) -> Vec<CheckResult> {
    let mut tasks = tokio::task::JoinSet::new();
    for (index, service) in services.iter().copied().enumerate() {
        let client = client.clone();
        tasks.spawn(async move { (index, check(&client, service).await) });
    }
    let mut results: Vec<Option<CheckResult>> = vec![None; services.len()];
    while let Some(joined) = tasks.join_next().await {
        if let Ok((index, result)) = joined {
            results[index] = Some(result);
        }
    }
    results
        .into_iter()
        .zip(services)
        .map(|(result, service)| result.unwrap_or_else(|| CheckResult::failed(*service, "check aborted")))
        .collect()
}

pub async fn check(client: &reqwest::Client, service: Service) -> CheckResult {
    match service {
        Service::Netflix => netflix(client).await,
        Service::YoutubePremium => youtube(client).await,
        Service::DisneyPlus => disney(client).await,
        Service::ChatGpt => chatgpt(client).await,
        Service::Claude => claude(client).await,
        Service::Gemini => gemini(client).await,
        Service::TikTok => tiktok(client).await,
    }
    .unwrap_or_else(|error| CheckResult::failed(service, describe(&error)))
}

/// A fetched page: status, final URL after redirects, and body.
struct Page {
    status: u16,
    url: String,
    body: String,
}

async fn get(client: &reqwest::Client, url: &str) -> Result<Page, reqwest::Error> {
    let response = client
        .get(url)
        .header("Accept-Language", "en-US,en;q=0.9")
        .send()
        .await?;
    let status = response.status().as_u16();
    let url = response.url().to_string();
    let body = response.text().await?;
    Ok(Page { status, url, body })
}

/// A short reason for a request error, without the full URL.
fn describe(error: &reqwest::Error) -> String {
    if error.is_timeout() {
        "timed out".into()
    } else if error.is_connect() {
        "could not connect through the proxy".into()
    } else if error.is_redirect() {
        "too many redirects".into()
    } else {
        let mut message = error.to_string();
        if let Some(source) = std::error::Error::source(error) {
            message = format!("{message}: {source}");
        }
        message
    }
}

/// Region from a Cloudflare `/cdn-cgi/trace` body (`loc=JP`).
fn trace_region(body: &str) -> Option<String> {
    body.lines()
        .find_map(|line| line.strip_prefix("loc="))
        .map(str::trim)
        .filter(|loc| !loc.is_empty())
        .map(str::to_uppercase)
}

/// The quoted value after `key` (`"key":"VALUE"` style), up to `max` chars.
fn quoted_after(body: &str, key: &str, max: usize) -> Option<String> {
    let rest = &body[body.find(key)? + key.len()..];
    let value: String = rest.chars().take_while(|c| *c != '"').take(max + 1).collect();
    (!value.is_empty() && value.len() <= max).then_some(value)
}

async fn netflix(client: &reqwest::Client) -> Result<CheckResult, reqwest::Error> {
    // A licensed title (LEGO Ninjago) and a Netflix original (Breaking Bad).
    let licensed = get(client, "https://www.netflix.com/title/81280792").await?;
    let original = get(client, "https://www.netflix.com/title/70143836").await?;
    Ok(classify_netflix(&licensed, &original))
}

fn classify_netflix(licensed: &Page, original: &Page) -> CheckResult {
    let service = Service::Netflix;
    // Region from a localized URL: https://www.netflix.com/jp/title/...
    let region = [licensed, original].iter().find_map(|page| {
        let path = page.url.split("netflix.com/").nth(1)?;
        let segment = path.split('/').next()?;
        let code = segment.split('-').next()?;
        (code.len() == 2 && code.chars().all(|c| c.is_ascii_alphabetic())).then(|| code.to_uppercase())
    });
    match (licensed.status, original.status) {
        (200, _) => CheckResult::new(service, Verdict::Available).region(region),
        (404, 200) => CheckResult::new(service, Verdict::OriginalsOnly).region(region),
        (403, _) | (_, 403) | (404, 404) => CheckResult::new(service, Verdict::Unavailable),
        (licensed, original) => CheckResult::failed(service, format!("HTTP {licensed}/{original}")),
    }
}

async fn youtube(client: &reqwest::Client) -> Result<CheckResult, reqwest::Error> {
    Ok(classify_youtube(&get(client, "https://www.youtube.com/premium").await?))
}

fn classify_youtube(page: &Page) -> CheckResult {
    let service = Service::YoutubePremium;
    if page.url.contains("google.cn") || page.body.contains("www.google.cn") {
        return CheckResult::new(service, Verdict::Unavailable).region(Some("CN".into()));
    }
    let region = quoted_after(&page.body, "\"INNERTUBE_CONTEXT_GL\":\"", 2)
        .or_else(|| quoted_after(&page.body, "\"countryCode\":\"", 2));
    if page.body.contains("Premium is not available in your country") {
        return CheckResult::new(service, Verdict::Unavailable).region(region);
    }
    if page.body.to_lowercase().contains("ad-free") {
        return CheckResult::new(service, Verdict::Available).region(region);
    }
    CheckResult::failed(service, format!("unrecognized page (HTTP {})", page.status))
}

async fn disney(client: &reqwest::Client) -> Result<CheckResult, reqwest::Error> {
    Ok(classify_disney(&get(client, "https://www.disneyplus.com/").await?))
}

/// Best effort from the home page: Disney+ redirects to a localized path
/// (`/en-gb/`) where it is offered and to an "unavailable" page elsewhere.
fn classify_disney(page: &Page) -> CheckResult {
    let service = Service::DisneyPlus;
    let url = page.url.to_lowercase();
    if page.status == 403 || url.contains("unavailable") || url.contains("preview") {
        return CheckResult::new(service, Verdict::Unavailable);
    }
    if page.status != 200 {
        return CheckResult::failed(service, format!("HTTP {}", page.status));
    }
    let region = url
        .split("disneyplus.com/")
        .nth(1)
        .and_then(|path| path.split('/').next())
        .and_then(|locale| locale.split_once('-'))
        .map(|(_, country)| country.to_uppercase())
        .filter(|country| country.len() == 2);
    CheckResult::new(service, Verdict::Available).region(region)
}

async fn chatgpt(client: &reqwest::Client) -> Result<CheckResult, reqwest::Error> {
    let web = get(client, "https://api.openai.com/compliance/cookie_requirements").await?;
    let ios = get(client, "https://ios.chat.openai.com/").await?;
    let region = get(client, "https://chatgpt.com/cdn-cgi/trace")
        .await
        .ok()
        .and_then(|trace| trace_region(&trace.body));
    Ok(classify_chatgpt(&web, &ios, region))
}

fn classify_chatgpt(web: &Page, ios: &Page, region: Option<String>) -> CheckResult {
    let service = Service::ChatGpt;
    if web.body.contains("unsupported_country") || ios.body.contains("unsupported_country") {
        return CheckResult::new(service, Verdict::Unavailable).region(region);
    }
    if ios.body.contains("VPN") {
        return CheckResult::new(service, Verdict::Unavailable)
            .region(region)
            .detail("the exit IP is flagged as a VPN");
    }
    // Without a marker, only clean answers count: an error page (rate limit,
    // outage) says nothing about availability.
    if web.status >= 400 || ios.status >= 400 {
        return CheckResult::failed(service, format!("HTTP {}/{}", web.status, ios.status)).region(region);
    }
    CheckResult::new(service, Verdict::Available).region(region)
}

async fn claude(client: &reqwest::Client) -> Result<CheckResult, reqwest::Error> {
    let login = get(client, "https://claude.ai/login").await?;
    let region = get(client, "https://claude.ai/cdn-cgi/trace")
        .await
        .ok()
        .and_then(|trace| trace_region(&trace.body));
    Ok(classify_claude(&login, region))
}

fn classify_claude(page: &Page, region: Option<String>) -> CheckResult {
    let service = Service::Claude;
    if page.url.contains("unavailable-in-region") {
        return CheckResult::new(service, Verdict::Unavailable).region(region);
    }
    // A bot challenge (403) still means the site is served in this region.
    if page.status < 500 {
        return CheckResult::new(service, Verdict::Available).region(region);
    }
    CheckResult::failed(service, format!("HTTP {}", page.status))
}

async fn gemini(client: &reqwest::Client) -> Result<CheckResult, reqwest::Error> {
    Ok(classify_gemini(&get(client, "https://gemini.google.com/").await?))
}

fn classify_gemini(page: &Page) -> CheckResult {
    let service = Service::Gemini;
    // Google marks availability with this flag in the page's bootstrap data.
    let region = quoted_after(&page.body, ",2,1,200,\"", 3);
    if page.body.contains("45631641,null,true") {
        CheckResult::new(service, Verdict::Available).region(region)
    } else if page.status == 200 {
        CheckResult::new(service, Verdict::Unavailable).region(region)
    } else {
        CheckResult::failed(service, format!("HTTP {}", page.status))
    }
}

async fn tiktok(client: &reqwest::Client) -> Result<CheckResult, reqwest::Error> {
    Ok(classify_tiktok(&get(client, "https://www.tiktok.com/").await?))
}

fn classify_tiktok(page: &Page) -> CheckResult {
    let service = Service::TikTok;
    if page.status == 403 || page.body.contains("not available in your region") {
        return CheckResult::new(service, Verdict::Unavailable);
    }
    match quoted_after(&page.body, "\"region\":\"", 2) {
        Some(region) => CheckResult::new(service, Verdict::Available).region(Some(region)),
        None => CheckResult::failed(service, format!("unrecognized page (HTTP {})", page.status)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(status: u16, url: &str, body: &str) -> Page {
        Page {
            status,
            url: url.into(),
            body: body.into(),
        }
    }

    fn verdict_and_region(result: &CheckResult) -> (Verdict, Option<&str>) {
        (result.verdict, result.region.as_deref())
    }

    #[test]
    fn netflix_tells_full_originals_only_and_blocked_apart() {
        let jp = page(200, "https://www.netflix.com/jp/title/81280792", "");
        let original = page(200, "https://www.netflix.com/title/70143836", "");
        let missing = page(404, "https://www.netflix.com/title/81280792", "");
        let blocked = page(403, "https://www.netflix.com/title/81280792", "");

        assert_eq!(
            verdict_and_region(&classify_netflix(&jp, &original)),
            (Verdict::Available, Some("JP"))
        );
        assert_eq!(classify_netflix(&missing, &original).verdict, Verdict::OriginalsOnly);
        assert_eq!(classify_netflix(&missing, &missing).verdict, Verdict::Unavailable);
        assert_eq!(classify_netflix(&blocked, &original).verdict, Verdict::Unavailable);
        assert_eq!(classify_netflix(&page(502, "", ""), &original).verdict, Verdict::Failed);
    }

    #[test]
    fn youtube_premium_reads_the_country_and_the_offer() {
        let offered = page(
            200,
            "https://www.youtube.com/premium",
            r#"{"INNERTUBE_CONTEXT_GL":"SG"} YouTube and YouTube Music ad-free"#,
        );
        assert_eq!(
            verdict_and_region(&classify_youtube(&offered)),
            (Verdict::Available, Some("SG"))
        );
        let refused = page(
            200,
            "https://www.youtube.com/premium",
            r#""countryCode":"RU" Premium is not available in your country"#,
        );
        assert_eq!(
            verdict_and_region(&classify_youtube(&refused)),
            (Verdict::Unavailable, Some("RU"))
        );
        let china = page(200, "https://www.google.cn/", "");
        assert_eq!(
            verdict_and_region(&classify_youtube(&china)),
            (Verdict::Unavailable, Some("CN"))
        );
        assert_eq!(classify_youtube(&page(200, "", "<html>")).verdict, Verdict::Failed);
    }

    #[test]
    fn disney_uses_the_localized_home_page() {
        let uk = page(200, "https://www.disneyplus.com/en-gb/", "");
        assert_eq!(
            verdict_and_region(&classify_disney(&uk)),
            (Verdict::Available, Some("GB"))
        );
        let unavailable = page(200, "https://www.disneyplus.com/unavailable", "");
        assert_eq!(classify_disney(&unavailable).verdict, Verdict::Unavailable);
        assert_eq!(classify_disney(&page(403, "", "")).verdict, Verdict::Unavailable);
        assert_eq!(classify_disney(&page(500, "", "")).verdict, Verdict::Failed);
    }

    #[test]
    fn chatgpt_detects_unsupported_countries_and_vpn_blocks() {
        let fine = page(200, "", "{}");
        let unsupported = page(403, "", r#"{"cf_details":"unsupported_country"}"#);
        let vpn = page(
            403,
            "",
            "You may be connected to a disallowed ISP. Please disable your VPN.",
        );
        assert_eq!(
            verdict_and_region(&classify_chatgpt(&fine, &fine, Some("JP".into()))),
            (Verdict::Available, Some("JP"))
        );
        assert_eq!(
            classify_chatgpt(&unsupported, &fine, None).verdict,
            Verdict::Unavailable
        );
        let blocked = classify_chatgpt(&fine, &vpn, None);
        assert_eq!(blocked.verdict, Verdict::Unavailable);
        assert!(blocked.detail.as_deref().is_some_and(|detail| detail.contains("VPN")));
        for status in [429, 451, 502] {
            let error = page(status, "", "<html>error</html>");
            assert_eq!(
                classify_chatgpt(&error, &fine, None).verdict,
                Verdict::Failed,
                "{status}"
            );
            assert_eq!(
                classify_chatgpt(&fine, &error, None).verdict,
                Verdict::Failed,
                "{status}"
            );
        }
    }

    #[test]
    fn claude_follows_the_region_redirect() {
        let blocked = page(200, "https://www.anthropic.com/app-unavailable-in-region", "");
        assert_eq!(
            classify_claude(&blocked, Some("CN".into())).verdict,
            Verdict::Unavailable
        );
        let challenge = page(403, "https://claude.ai/login", "Just a moment...");
        assert_eq!(
            verdict_and_region(&classify_claude(&challenge, Some("US".into()))),
            (Verdict::Available, Some("US"))
        );
        assert_eq!(classify_claude(&page(503, "", ""), None).verdict, Verdict::Failed);
    }

    #[test]
    fn gemini_and_tiktok_read_their_page_data() {
        let gemini = page(
            200,
            "https://gemini.google.com/",
            r#"[45631641,null,true],2,1,200,"USA""#,
        );
        assert_eq!(
            verdict_and_region(&classify_gemini(&gemini)),
            (Verdict::Available, Some("USA"))
        );
        assert_eq!(classify_gemini(&page(200, "", "<html>")).verdict, Verdict::Unavailable);

        let tiktok = page(200, "https://www.tiktok.com/", r#"{"region":"JP","lang":"en"}"#);
        assert_eq!(
            verdict_and_region(&classify_tiktok(&tiktok)),
            (Verdict::Available, Some("JP"))
        );
        assert_eq!(classify_tiktok(&page(403, "", "")).verdict, Verdict::Unavailable);
        assert_eq!(classify_tiktok(&page(200, "", "<html>")).verdict, Verdict::Failed);
    }

    #[test]
    fn helpers_parse_trace_and_quoted_values() {
        assert_eq!(trace_region("fl=1\nloc=jp\nip=1.2.3.4").as_deref(), Some("JP"));
        assert_eq!(trace_region("fl=1"), None);
        assert_eq!(quoted_after(r#""region":"TOO_LONG""#, "\"region\":\"", 2), None);
    }

    #[tokio::test]
    async fn an_unreachable_proxy_fails_every_check_in_order() {
        // Nothing listens on port 9; each check reports the failure.
        let client = proxied_client(9).unwrap();
        let results = check_all(&client, &[Service::Claude, Service::Netflix]).await;
        assert_eq!(
            results.iter().map(|result| result.service).collect::<Vec<_>>(),
            [Service::Claude, Service::Netflix]
        );
        assert!(results.iter().all(|result| result.verdict == Verdict::Failed));
        assert!(results[0].detail.is_some());
    }
}
