//! Bounded DNS/connection injection for issue86, reusing the loopback HTTP fixture.
//! Install only while holding the existing test app-home lock.

use std::net::SocketAddr;

use super::fetch::ProxyMode;

#[derive(Clone)]
pub(crate) struct Fixture {
    pub address: SocketAddr,
    pub direct_success: bool,
}

static ACTIVE: parking_lot::Mutex<Option<Fixture>> = parking_lot::Mutex::new(None);

pub(crate) struct Guard;

pub(crate) fn install(fixture: Fixture) -> Guard {
    assert!(fixture.address.ip().is_loopback());
    assert!(ACTIVE.lock().replace(fixture).is_none());
    Guard
}

impl Drop for Guard {
    fn drop(&mut self) {
        *ACTIVE.lock() = None;
    }
}

pub(crate) fn is_fixture_host(host: &str) -> bool {
    host.ends_with(".issue86.test") && ACTIVE.lock().is_some()
}

pub(crate) fn for_url(url: &url::Url) -> Option<Fixture> {
    url.host_str()
        .filter(|host| host.ends_with(".issue86.test"))
        .and_then(|_| ACTIVE.lock().clone())
}

impl Fixture {
    pub fn configure(
        &self,
        builder: reqwest::ClientBuilder,
        url: &url::Url,
        mode: ProxyMode,
    ) -> anyhow::Result<reqwest::ClientBuilder> {
        if mode == ProxyMode::None {
            // The synthetic DNS answer is public; socket transport either fails
            // deterministically or maps to our loopback HTTP server. No public
            // connection and no environment proxy are ever used by this fixture.
            super::ssrf::check_url_host(url.as_str(), &[])?;
            if !self.direct_success {
                anyhow::bail!("injected issue86 direct connection failure");
            }
            Ok(builder.no_proxy().resolve(url.host_str().unwrap(), self.address))
        } else {
            Ok(builder
                .no_proxy()
                .proxy(reqwest::Proxy::all(format!("http://{}", self.address))?))
        }
    }
}
