use serde::Serialize;
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum CoreState {
    Stopped,
    Starting,
    Running,
    Error(String),
}

impl fmt::Display for CoreState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Stopped => f.write_str("Stopped"),
            Self::Starting => f.write_str("Starting"),
            Self::Running => f.write_str("Running"),
            Self::Error(msg) => write!(f, "Error({msg})"),
        }
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn test_core_state_construct() {
        let stopped = CoreState::Stopped;
        let starting = CoreState::Starting;
        let running = CoreState::Running;
        let err = CoreState::Error("boom".to_string());

        assert_eq!(stopped, CoreState::Stopped);
        assert_eq!(starting, CoreState::Starting);
        assert_eq!(running, CoreState::Running);
        assert_eq!(err, CoreState::Error("boom".to_string()));
        assert_ne!(stopped, running);
        assert_ne!(starting, CoreState::Error("x".to_string()));
    }

    #[test]
    fn test_core_state_match_exhaustive() {
        let label = |s: CoreState| -> String {
            match s {
                CoreState::Stopped => "S".to_string(),
                CoreState::Starting => "T".to_string(),
                CoreState::Running => "R".to_string(),
                CoreState::Error(_) => "E".to_string(),
            }
        };

        assert_eq!(label(CoreState::Stopped), "S");
        assert_eq!(label(CoreState::Starting), "T");
        assert_eq!(label(CoreState::Running), "R");
        assert_eq!(label(CoreState::Error("x".into())), "E");
    }
}
