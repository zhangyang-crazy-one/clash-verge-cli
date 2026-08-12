//! Semantic color palette and shared panel builders for the TUI.
//!
//! Every raw `Color` literal in the shell lives here. Views, lists, and
//! dialogs style themselves exclusively through these functions so the whole
//! palette can be retuned from one place.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::Block;

// --- semantic palette ------------------------------------------------------

/// Primary interactive accent (selection, active routes, focus borders).
pub fn accent() -> Color {
    Color::Cyan
}

/// Positive outcome (running, selected, saved).
pub fn ok() -> Color {
    Color::Green
}

/// Caution (starting, awaiting input, muted warnings).
pub fn warn() -> Color {
    Color::Yellow
}

/// Destructive or failed outcome (errors, close confirmations).
pub fn danger() -> Color {
    Color::Red
}

/// Primary body text.
pub fn text() -> Color {
    Color::White
}

/// De-emphasized / metadata text.
pub fn dim() -> Color {
    Color::DarkGray
}

/// Bold variant of a semantic color for headings and emphasized values.
pub fn bold(color: Color) -> Style {
    Style::new().fg(color).add_modifier(Modifier::BOLD)
}

/// Unified focus emphasis: a filled accent chip while the pane owns focus,
/// accent-bold otherwise (nav resting state, unfocused panes).
pub fn highlight(focused: bool) -> Style {
    if focused {
        Style::new().fg(Color::Black).bg(accent()).add_modifier(Modifier::BOLD)
    } else {
        Style::new().fg(accent()).add_modifier(Modifier::BOLD)
    }
}

/// Bordered panel whose border turns accent-colored while the pane owns focus.
pub fn panel_block<'a, T: Into<Line<'a>>>(title: T, focused: bool) -> Block<'a> {
    let border = if focused {
        Style::new().fg(accent())
    } else {
        Style::new()
    };
    Block::bordered().title(title).border_style(border)
}

/// Severity color for a status message.
///
/// The event loop writes plain strings; the shell colors them heuristically
/// from the message prefix so no state plumbing is needed. Recognized failure
/// and success prefixes map to `danger`/`ok`; everything else stays neutral.
pub fn status_color(message: &str) -> Color {
    let lower = message.to_ascii_lowercase();
    let danger_prefixes = [
        "import failed",
        "delay failed",
        "could not",
        "editor error",
        "invalid yaml",
        "select a connection before",
        "need ",
        "import cancelled",
        "config file path not available",
        "connection close confirmation expired",
        "error:",
    ];
    let ok_prefixes = [
        "language saved",
        "profiles loaded",
        "profile imported",
        "chain applied",
        "chain cleared",
        "config saved",
        "mode set",
        "delay:",
        "system proxy enabled",
        "system proxy disabled",
        "tun enabled",
        "tun disabled",
        "tun saved",
        "no testable",
    ];
    if danger_prefixes.iter().any(|prefix| lower.starts_with(prefix)) {
        danger()
    } else if ok_prefixes.iter().any(|prefix| lower.starts_with(prefix)) {
        ok()
    } else {
        text()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Modifier;

    #[test]
    fn palette_roles_map_to_the_designer_semantics() {
        assert_eq!(accent(), Color::Cyan);
        assert_eq!(ok(), Color::Green);
        assert_eq!(warn(), Color::Yellow);
        assert_eq!(danger(), Color::Red);
        assert_eq!(text(), Color::White);
        assert_eq!(dim(), Color::DarkGray);
    }

    #[test]
    fn highlight_focused_is_a_filled_accent_chip() {
        let style = highlight(true);
        assert_eq!(style.fg, Some(Color::Black));
        assert_eq!(style.bg, Some(Color::Cyan));
        assert!(style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn highlight_unfocused_is_accent_bold_only() {
        let style = highlight(false);
        assert_eq!(style.fg, Some(Color::Cyan));
        assert_eq!(style.bg, None);
        assert!(style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn status_color_recognizes_failure_and_success_prefixes() {
        assert_eq!(status_color("Import failed: SSRF blocked"), danger());
        assert_eq!(status_color("Delay failed for node: timeout"), danger());
        assert_eq!(status_color("Could not save language"), danger());
        assert_eq!(status_color("Profile imported successfully"), ok());
        assert_eq!(status_color("Language saved"), ok());
        assert_eq!(status_color("Connected to mihomo API"), text());
        assert_eq!(status_color("Switching to Tokyo..."), text());
    }
}
