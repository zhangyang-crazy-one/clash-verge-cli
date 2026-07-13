use std::path::PathBuf;
use thiserror::Error;

/// Errors produced when communicating with the mihomo REST API.
///
/// Variants are partitioned by recovery path:
/// - `CoreDown` / `Io` — the mihomo process is not reachable; the caller
///   should surface this as a process-state change.
/// - `Unauthorized` / `NotFound` / `HttpStatus` — server responded; the
///   caller can treat as a logic error (bad config, wrong secret, etc).
/// - `Parse` — server returned unparseable JSON; usually a version skew.
/// - `Http` — low-level reqwest error not covered above.
/// - `InvalidUri` — bad URL/path (e.g. secret contains a NUL byte).
#[derive(Debug, Error)]
pub enum MihomoError {
    #[error("mihomo is not running at {path}")]
    CoreDown { path: PathBuf },

    #[error("mihomo returned 401 Unauthorized — check secret in verge.yaml")]
    Unauthorized,

    #[error("mihomo returned 404: {0}")]
    NotFound(String),

    #[error("mihomo returned status {status}: {body}")]
    HttpStatus { status: u16, body: String },

    #[error("failed to parse mihomo response: {0}")]
    Parse(String),

    #[error("HTTP client error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("invalid URL or path: {0}")]
    InvalidUri(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_display() {
        // CoreDown
        let err = MihomoError::CoreDown {
            path: PathBuf::from("/tmp/x.sock"),
        };
        assert!(err.to_string().contains("/tmp/x.sock"));
        assert!(err.to_string().contains("not running"));

        // Unauthorized
        assert!(MihomoError::Unauthorized.to_string().contains("401"));
        assert!(MihomoError::Unauthorized.to_string().contains("verge.yaml"));

        // NotFound
        assert!(
            MihomoError::NotFound("/version".into())
                .to_string()
                .contains("/version")
        );

        // HttpStatus
        let err = MihomoError::HttpStatus {
            status: 500,
            body: "oops".into(),
        };
        let s = err.to_string();
        assert!(s.contains("500"));
        assert!(s.contains("oops"));

        // Parse
        assert!(MihomoError::Parse("bad json".into()).to_string().contains("bad json"));

        // InvalidUri
        assert!(
            MihomoError::InvalidUri("nul byte".into())
                .to_string()
                .contains("invalid URL or path")
        );
    }

    #[test]
    fn test_error_is_std_error() {
        // Compile-time check: MihomoError implements std::error::Error via
        // thiserror. A static assertion via trait object is the runtime
        // mirror of that.
        fn assert_error<E: std::error::Error>(_: &E) {}
        let err = MihomoError::Unauthorized;
        assert_error(&err);
    }
}
