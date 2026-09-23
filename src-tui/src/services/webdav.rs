//! Backups on a WebDAV server, in the `clash-verge-rev-backup` folder the
//! GUI uses, with the `webdav_url` / `webdav_username` / `webdav_password`
//! settings from `verge.yaml`.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context as _, bail};
use clash_verge_core::config::IVerge;
use reqwest::{Method, StatusCode};

use super::backup::Entry;

pub struct WebDav {
    client: reqwest::Client,
    /// The backup folder, ending in `/`.
    folder: url::Url,
    username: Option<String>,
    password: Option<String>,
}

impl WebDav {
    pub fn from_settings(verge: &IVerge) -> anyhow::Result<Self> {
        let Some(base) = verge.webdav_url.as_deref().filter(|url| !url.trim().is_empty()) else {
            bail!("no WebDAV server: set webdav_url (and webdav_username, webdav_password) in verge.yaml");
        };
        let mut base = url::Url::parse(base.trim()).with_context(|| format!("invalid webdav_url {base:?}"))?;
        if !matches!(base.scheme(), "http" | "https") {
            bail!("webdav_url must be http:// or https://");
        }
        if !base.path().ends_with('/') {
            base.set_path(&format!("{}/", base.path()));
        }
        let folder = base.join(&format!("{}/", clash_verge_core::utils::dirs::BACKUP_DIR))?;
        Ok(Self {
            client: reqwest::Client::builder().timeout(Duration::from_secs(60)).build()?,
            folder,
            username: verge
                .webdav_username
                .as_deref()
                .filter(|user| !user.is_empty())
                .map(str::to_string),
            password: verge.webdav_password.as_deref().map(str::to_string),
        })
    }

    fn request(&self, method: Method, url: url::Url) -> reqwest::RequestBuilder {
        let request = self.client.request(method, url);
        match &self.username {
            Some(user) => request.basic_auth(user, self.password.as_deref()),
            None => request,
        }
    }

    fn file_url(&self, name: &str) -> anyhow::Result<url::Url> {
        if !super::backup::is_backup_name(name) {
            bail!("{name:?} is not a backup file name");
        }
        Ok(self.folder.join(name)?)
    }

    /// Upload `path` as `name` into the backup folder (created if missing).
    pub async fn upload(&self, path: &Path, name: &str) -> anyhow::Result<()> {
        let url = self.file_url(name)?;
        let mkcol = self
            .request(Method::from_bytes(b"MKCOL")?, self.folder.clone())
            .send()
            .await
            .context("cannot reach the WebDAV server")?;
        // 405: it already exists.
        if !(mkcol.status().is_success() || mkcol.status() == StatusCode::METHOD_NOT_ALLOWED) {
            bail!(
                "cannot create {} on the WebDAV server: {}",
                self.folder,
                status_text(mkcol.status())
            );
        }
        let data = tokio::fs::read(path).await?;
        let response = self.request(Method::PUT, url).body(data).send().await?;
        if !response.status().is_success() {
            bail!("WebDAV upload failed: {}", status_text(response.status()));
        }
        Ok(())
    }

    /// Backups in the folder, newest first; empty if it does not exist yet.
    pub async fn list(&self) -> anyhow::Result<Vec<Entry>> {
        let response = self
            .request(Method::from_bytes(b"PROPFIND")?, self.folder.clone())
            .header("Depth", "1")
            .header("Content-Type", "application/xml")
            .body(
                r#"<?xml version="1.0" encoding="utf-8"?><propfind xmlns="DAV:"><prop><getcontentlength/><getlastmodified/></prop></propfind>"#,
            )
            .send()
            .await
            .context("cannot reach the WebDAV server")?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(Vec::new());
        }
        if response.status() != StatusCode::MULTI_STATUS && !response.status().is_success() {
            bail!("WebDAV listing failed: {}", status_text(response.status()));
        }
        let mut entries = parse_propfind(&response.text().await?);
        entries.sort_by(|left, right| right.modified.cmp(&left.modified).then(right.name.cmp(&left.name)));
        Ok(entries)
    }

    /// Download `name` from the backup folder to `dest`.
    pub async fn download(&self, name: &str, dest: &Path) -> anyhow::Result<()> {
        let response = self.request(Method::GET, self.file_url(name)?).send().await?;
        if !response.status().is_success() {
            bail!("WebDAV download of {name} failed: {}", status_text(response.status()));
        }
        let data = response.bytes().await?;
        super::backup::write_private(dest, &data)?;
        Ok(())
    }
}

/// `HTTP 401 Unauthorized`, with a hint where the fix is.
fn status_text(status: StatusCode) -> String {
    match status {
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
            format!("HTTP {status} (check webdav_username and webdav_password in verge.yaml)")
        }
        _ => format!("HTTP {status}"),
    }
}

/// Backup files in a PROPFIND multistatus body. Tolerant of namespace
/// prefixes (`D:`, `d:`, none) since servers differ.
fn parse_propfind(xml: &str) -> Vec<Entry> {
    let mut entries = Vec::new();
    for response in split_elements(xml, "response") {
        let Some(href) = element_text(response, "href") else {
            continue;
        };
        let decoded = percent_encoding::percent_decode_str(href.trim()).decode_utf8_lossy();
        let Some(name) = decoded.trim_end_matches('/').rsplit('/').next().map(str::to_string) else {
            continue;
        };
        if decoded.ends_with('/') || !super::backup::is_backup_name(&name) {
            continue;
        }
        let bytes = element_text(response, "getcontentlength")
            .and_then(|length| length.trim().parse().ok())
            .unwrap_or(0);
        let modified = element_text(response, "getlastmodified")
            .and_then(|date| chrono::DateTime::parse_from_rfc2822(date.trim()).ok())
            .map(|date| date.timestamp());
        entries.push(Entry { name, bytes, modified });
    }
    entries
}

/// The bodies of every `<prefix:local ...>…</prefix:local>` element.
fn split_elements<'a>(xml: &'a str, local: &str) -> Vec<&'a str> {
    let mut parts = Vec::new();
    let mut rest = xml;
    while let Some((_, open_end)) = find_open_tag(rest, local) {
        let body = &rest[open_end..];
        let Some(close) = find_close_tag(body, local) else {
            break;
        };
        parts.push(&body[..close]);
        rest = &body[close..];
    }
    parts
}

fn element_text<'a>(xml: &'a str, local: &str) -> Option<&'a str> {
    let (_, open_end) = find_open_tag(xml, local)?;
    let body = &xml[open_end..];
    Some(&body[..find_close_tag(body, local)?])
}

/// Start and end offsets of the first `<local` or `<prefix:local` open tag.
fn find_open_tag(xml: &str, local: &str) -> Option<(usize, usize)> {
    let mut offset = 0;
    while let Some(found) = xml[offset..].find('<') {
        let start = offset + found;
        let end = start + xml[start..].find('>')? + 1;
        let tag = &xml[start + 1..end - 1];
        let name = tag
            .split(|c: char| c.is_whitespace() || c == '/')
            .next()
            .unwrap_or_default();
        if !tag.starts_with('/') && name.rsplit(':').next() == Some(local) && !tag.ends_with('/') {
            return Some((start, end));
        }
        offset = end;
    }
    None
}

/// Offset of the first `</local>` or `</prefix:local>`.
fn find_close_tag(xml: &str, local: &str) -> Option<usize> {
    let mut offset = 0;
    while let Some(found) = xml[offset..].find("</") {
        let start = offset + found;
        let end = start + xml[start..].find('>')?;
        if xml[start + 2..end].trim().rsplit(':').next() == Some(local) {
            return Some(start);
        }
        offset = end;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn propfind_lists_backup_files_from_common_servers() {
        // Apache mod_dav / Nextcloud style: `D:` prefix, collection first.
        let apache = r#"<?xml version="1.0" encoding="utf-8"?>
<D:multistatus xmlns:D="DAV:">
<D:response><D:href>/dav/clash-verge-rev-backup/</D:href><D:propstat><D:prop><D:getlastmodified>Tue, 22 Sep 2026 10:00:00 GMT</D:getlastmodified></D:prop></D:propstat></D:response>
<D:response><D:href>/dav/clash-verge-rev-backup/linux-backup-2026-09-22_10-00-00.zip</D:href><D:propstat><D:prop><D:getcontentlength>2048</D:getcontentlength><D:getlastmodified>Tue, 22 Sep 2026 10:00:00 GMT</D:getlastmodified></D:prop></D:propstat></D:response>
<D:response><D:href>/dav/clash-verge-rev-backup/notes.txt</D:href></D:response>
</D:multistatus>"#;
        let entries = parse_propfind(apache);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "linux-backup-2026-09-22_10-00-00.zip");
        assert_eq!(entries[0].bytes, 2048);
        assert!(entries[0].modified.is_some());

        // Lower-case prefix, attributes, percent-encoded href, no prefix.
        let other = r#"<d:multistatus xmlns:d="DAV:"><d:response>
<d:href>https://dav.example/x/clash-verge-rev-backup/macos-backup%202026.zip</d:href>
<d:propstat><d:prop><d:getcontentlength>10</d:getcontentlength></d:prop></d:propstat></d:response>
<response xmlns="DAV:"><href>/b/windows-backup-1.zip</href><propstat><prop><getcontentlength>5</getcontentlength></prop></propstat></response>
</d:multistatus>"#;
        let names: Vec<_> = parse_propfind(other).into_iter().map(|entry| entry.name).collect();
        // A space is not a valid backup name; the plain one is kept.
        assert_eq!(names, ["windows-backup-1.zip"]);
    }

    #[test]
    fn settings_need_a_url_and_name_the_gui_folder() {
        let mut verge = IVerge::default();
        assert!(WebDav::from_settings(&verge).is_err());
        verge.webdav_url = Some("ftp://x".into());
        assert!(WebDav::from_settings(&verge).is_err());
        verge.webdav_url = Some("https://dav.example/remote.php/dav/files/me".into());
        let dav = WebDav::from_settings(&verge).unwrap();
        assert_eq!(
            dav.folder.as_str(),
            "https://dav.example/remote.php/dav/files/me/clash-verge-rev-backup/"
        );
        assert!(dav.file_url("../x.zip").is_err());
        assert!(dav.file_url("linux-backup-1.zip").is_ok());
    }
}
