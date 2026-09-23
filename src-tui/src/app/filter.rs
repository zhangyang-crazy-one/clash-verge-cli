//! Text filter shared by every `/` filter in the TUI.

/// Whether every whitespace-separated word of `query` occurs, ignoring case,
/// in at least one of `fields`. An empty query matches everything, so
/// `tokyo 01` finds `🇯🇵 Tokyo 01` and `jp tokyo` finds a node whose group or
/// name mentions both words.
pub fn matches(query: &str, fields: &[&str]) -> bool {
    let fields: Vec<String> = fields.iter().map(|field| field.to_lowercase()).collect();
    query
        .split_whitespace()
        .map(str::to_lowercase)
        .all(|word| fields.iter().any(|field| field.contains(&word)))
}

/// The filter to apply: `None` for a missing or blank query.
pub fn active(query: Option<&str>) -> Option<&str> {
    query.filter(|query| !query.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_word_must_match_some_field_ignoring_case() {
        assert!(matches("tokyo 01", &["🇯🇵 Tokyo 01"]));
        assert!(matches("TOKYO", &["🇯🇵 tokyo 01"]));
        assert!(matches("jp tokyo", &["JP", "Tokyo 01"]));
        assert!(!matches("tokyo 02", &["🇯🇵 Tokyo 01"]));
        assert!(matches("香港", &["香港 01"]));
        assert!(matches("", &["anything"]));
        assert!(matches("   ", &["anything"]));
    }

    #[test]
    fn blank_queries_are_inactive() {
        assert_eq!(active(None), None);
        assert_eq!(active(Some("  ")), None);
        assert_eq!(active(Some("hk")), Some("hk"));
    }
}
