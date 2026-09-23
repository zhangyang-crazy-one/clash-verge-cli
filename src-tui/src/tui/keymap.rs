//! Optional TUI settings in `<config-dir>/tui.yaml`:
//!
//! ```yaml
//! # Mouse: wheel scrolls the list under the pointer, a click on the menu
//! # switches views. Hold Shift to select text while it is on.
//! mouse: true
//! # Key remaps: the key on the left acts as the built-in key on the right,
//! # or does nothing with "none". Built-in keys keep working unless remapped.
//! keys:
//!   "ctrl+n": "j"
//!   "ctrl+p": "k"
//!   "q": "none"
//!   "ctrl+q": "q"
//! ```
//!
//! Key names: a single character (`j`, `T`, `/`), or `enter`, `esc`, `tab`,
//! `backtab`, `backspace`, `space`, `up`, `down`, `left`, `right`, `pageup`,
//! `pagedown`, `home`, `end`, `delete`, `insert`, `f1`–`f12`, optionally
//! prefixed with `ctrl+`, `alt+`, or `shift+`.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context as _, bail};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde::Deserialize;

/// A key press, normalized so a config entry and a terminal event compare
/// equal: a character's case carries Shift, so Shift is dropped for chars.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct Key {
    code: KeyCode,
    modifiers: KeyModifiers,
}

impl Key {
    fn of(event: KeyEvent) -> Self {
        let mut modifiers = event.modifiers & (KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SHIFT);
        if matches!(event.code, KeyCode::Char(_)) {
            modifiers.remove(KeyModifiers::SHIFT);
        }
        Self {
            code: event.code,
            modifiers,
        }
    }

    /// The event a terminal sends for this key (Shift set for uppercase).
    fn event(self) -> KeyEvent {
        let mut modifiers = self.modifiers;
        if let KeyCode::Char(c) = self.code
            && c.is_uppercase()
        {
            modifiers.insert(KeyModifiers::SHIFT);
        }
        KeyEvent::new(self.code, modifiers)
    }

    fn parse(spec: &str) -> anyhow::Result<Self> {
        let mut modifiers = KeyModifiers::NONE;
        let mut rest = spec.trim();
        // `+` alone (or as the last part, `ctrl++`) is the plus key.
        while let Some((prefix, tail)) = rest.split_once('+').filter(|(_, tail)| !tail.is_empty()) {
            match prefix.to_ascii_lowercase().as_str() {
                "ctrl" | "control" => modifiers.insert(KeyModifiers::CONTROL),
                "alt" | "meta" => modifiers.insert(KeyModifiers::ALT),
                "shift" => modifiers.insert(KeyModifiers::SHIFT),
                _ => bail!("unknown modifier {prefix:?} in key {spec:?}"),
            }
            rest = tail;
        }
        let mut chars = rest.chars();
        let code = match (chars.next(), chars.next()) {
            (Some(c), None) if modifiers.contains(KeyModifiers::SHIFT) => KeyCode::Char(c.to_ascii_uppercase()),
            (Some(c), None) => KeyCode::Char(c),
            _ => named(&rest.to_ascii_lowercase()).with_context(|| format!("unknown key {spec:?}"))?,
        };
        let mut key = Self { code, modifiers };
        if matches!(code, KeyCode::Char(_)) {
            key.modifiers.remove(KeyModifiers::SHIFT);
        }
        Ok(key)
    }
}

fn named(name: &str) -> Option<KeyCode> {
    Some(match name {
        "enter" | "return" => KeyCode::Enter,
        "esc" | "escape" => KeyCode::Esc,
        "tab" => KeyCode::Tab,
        "backtab" => KeyCode::BackTab,
        "backspace" => KeyCode::Backspace,
        "space" => KeyCode::Char(' '),
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "pageup" => KeyCode::PageUp,
        "pagedown" => KeyCode::PageDown,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "delete" | "del" => KeyCode::Delete,
        "insert" | "ins" => KeyCode::Insert,
        _ => {
            let number: u8 = name.strip_prefix('f')?.parse().ok()?;
            return (1..=12).contains(&number).then_some(KeyCode::F(number));
        }
    })
}

/// Key remaps from `tui.yaml`.
#[derive(Debug, Clone, Default)]
pub struct KeyMap {
    /// `None`: the key is disabled.
    remaps: HashMap<Key, Option<Key>>,
}

impl KeyMap {
    /// The event the key map turns `event` into; `None` if it is disabled.
    pub fn translate(&self, event: KeyEvent) -> Option<KeyEvent> {
        match self.remaps.get(&Key::of(event)) {
            Some(target) => target.map(Key::event),
            None => Some(event),
        }
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.remaps.is_empty()
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    #[serde(default)]
    mouse: bool,
    #[serde(default)]
    keys: HashMap<String, String>,
}

/// Parsed `tui.yaml`.
#[derive(Debug, Clone, Default)]
pub struct TuiConfig {
    pub mouse: bool,
    pub keys: KeyMap,
}

impl TuiConfig {
    /// `tui.yaml` in the config directory.
    pub fn path() -> Option<std::path::PathBuf> {
        clash_verge_core::utils::dirs::app_home_dir()
            .ok()
            .map(|dir| dir.join("tui.yaml"))
    }

    /// The settings in `path`; defaults when it does not exist.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => Self::parse(&text).with_context(|| format!("invalid {}", path.display())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error).with_context(|| format!("cannot read {}", path.display())),
        }
    }

    #[cfg(test)]
    pub fn parse_for_test(text: &str) -> Self {
        Self::parse(text).expect("valid test config")
    }

    fn parse(text: &str) -> anyhow::Result<Self> {
        let raw: RawConfig = if text.trim().is_empty() {
            RawConfig::default()
        } else {
            serde_yaml_ng::from_str(text)?
        };
        let mut remaps = HashMap::new();
        for (from, to) in &raw.keys {
            let target = if to.trim().eq_ignore_ascii_case("none") {
                None
            } else {
                Some(Key::parse(to)?)
            };
            remaps.insert(Key::parse(from)?, target);
        }
        Ok(Self {
            mouse: raw.mouse,
            keys: KeyMap { remaps },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    #[test]
    fn remaps_disable_and_pass_through_keys() {
        let config = TuiConfig::parse(
            r#"
mouse: true
keys:
  "ctrl+n": "j"
  "q": "none"
  "ctrl+q": "q"
  "f5": "T"
  "shift+x": "pagedown"
"#,
        )
        .unwrap();
        assert!(config.mouse);
        let keys = &config.keys;

        let j = keys
            .translate(press(KeyCode::Char('n'), KeyModifiers::CONTROL))
            .unwrap();
        assert_eq!((j.code, j.modifiers), (KeyCode::Char('j'), KeyModifiers::NONE));
        assert!(keys.translate(press(KeyCode::Char('q'), KeyModifiers::NONE)).is_none());
        let quit = keys
            .translate(press(KeyCode::Char('q'), KeyModifiers::CONTROL))
            .unwrap();
        assert_eq!(quit.code, KeyCode::Char('q'));
        // An uppercase target is sent the way a terminal sends it.
        let batch = keys.translate(press(KeyCode::F(5), KeyModifiers::NONE)).unwrap();
        assert_eq!((batch.code, batch.modifiers), (KeyCode::Char('T'), KeyModifiers::SHIFT));
        // Terminals report `X` with Shift; the config may say shift+x.
        let page = keys.translate(press(KeyCode::Char('X'), KeyModifiers::SHIFT)).unwrap();
        assert_eq!(page.code, KeyCode::PageDown);

        // Everything else is untouched.
        let k = keys.translate(press(KeyCode::Char('k'), KeyModifiers::NONE)).unwrap();
        assert_eq!(k.code, KeyCode::Char('k'));
    }

    #[test]
    fn plus_and_named_keys_parse() {
        assert_eq!(Key::parse("+").unwrap().code, KeyCode::Char('+'));
        let key = Key::parse("ctrl++").unwrap();
        assert_eq!((key.code, key.modifiers), (KeyCode::Char('+'), KeyModifiers::CONTROL));
        assert_eq!(Key::parse("F12").unwrap().code, KeyCode::F(12));
        assert_eq!(Key::parse("space").unwrap().code, KeyCode::Char(' '));
    }

    #[test]
    fn mistakes_are_reported_not_ignored() {
        assert!(TuiConfig::parse("keys:\n  \"hyper+x\": j\n").is_err());
        assert!(TuiConfig::parse("keys:\n  x: \"f13\"\n").is_err());
        assert!(TuiConfig::parse("mouse: true\nkeymap: {}\n").is_err(), "unknown field");
        assert!(TuiConfig::parse("").unwrap().keys.is_empty());
    }

    #[test]
    fn a_missing_file_means_defaults() {
        let config = TuiConfig::load(Path::new("/nonexistent/clash-verge-cli/tui.yaml")).unwrap();
        assert!(!config.mouse);
        assert!(config.keys.is_empty());
    }
}
