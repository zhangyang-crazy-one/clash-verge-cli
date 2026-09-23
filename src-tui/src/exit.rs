//! Process exit codes, stable for scripts (documented in the README).
//!
//! | code | meaning |
//! | --- | --- |
//! | 0 | success (`status`: the core is running) |
//! | 1 | any other failure (`status`: the core is in an error state) |
//! | 2 | invalid command line (clap) |
//! | 3 | the core is not running (`status`: stopped; commands that need it) |
//! | 4 | missing privilege (TUN capability) |
//! | 5 | network or subscription failure (fetch, DNS, SSRF block, download) |

pub const SUCCESS: i32 = 0;
pub const FAILURE: i32 = 1;
pub const NOT_RUNNING: i32 = 3;
pub const PERMISSION: i32 = 4;
pub const NETWORK: i32 = 5;

/// The command needs a running core and none answers.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct CoreNotRunning(pub String);

/// A privilege the operation needs is missing (e.g. the TUN capability).
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct PermissionDenied(pub String);

/// Exit code for a failed command, from the most specific cause in its chain.
pub fn code_for(error: &anyhow::Error) -> i32 {
    for cause in error.chain() {
        if cause.is::<CoreNotRunning>() {
            return NOT_RUNNING;
        }
        if cause.is::<PermissionDenied>() {
            return PERMISSION;
        }
        if cause.is::<reqwest::Error>() || cause.is::<crate::subscribe::ssrf::CheckError>() {
            return NETWORK;
        }
    }
    FAILURE
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Context as _;

    #[test]
    fn codes_follow_the_most_specific_cause() {
        let not_running = anyhow::Error::new(CoreNotRunning("down".into())).context("proxy list");
        assert_eq!(code_for(&not_running), NOT_RUNNING);

        let denied: anyhow::Result<()> = Err(PermissionDenied("no cap".into()).into());
        assert_eq!(code_for(&denied.context("tun on").unwrap_err()), PERMISSION);

        let blocked = anyhow::Error::new(crate::subscribe::ssrf::CheckError::InvalidUrl("not a url".into()));
        assert_eq!(code_for(&blocked.context("import")), NETWORK);

        assert_eq!(code_for(&anyhow::anyhow!("something else")), FAILURE);
    }
}
