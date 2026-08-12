//! `askpass` hidden subcommand: renders a bordered password popup on
//! `/dev/tty` and prints the entered password to stdout for `sudo -A`.
//!
//! Works on headless servers (no DISPLAY, over SSH) because it only needs a
//! terminal. The TUI suspends its own rendering before invoking it.

use std::io::{Read, Write};
use std::os::fd::AsRawFd;

use anyhow::Context;
use nix::sys::termios::{LocalFlags, SetArg, tcgetattr, tcsetattr};

/// ANSI palette mirrors for the semantic TUI palette. The askpass subprocess
/// runs before config load (and over plain `/dev/tty`), so it styles with raw
/// escape sequences instead of the ratatui theme: cyan borders, bold yellow
/// prompt, bold white password field.
const RESET: &str = "\x1b[0m";
const CYAN: &str = "\x1b[36m";
const BOLD_YELLOW: &str = "\x1b[1;33m";
const BOLD_WHITE: &str = "\x1b[1;37m";

/// Run the askpass popup. `SUDO_PROMPT` (when set) becomes the popup label.
pub fn run() -> anyhow::Result<()> {
    // sudo passes the prompt as the first argument ("[sudo] password for x:").
    // The fallback is neutral English: this subprocess runs before config load,
    // so it cannot localize; sudo normally supplies the prompt anyway.
    let prompt = std::env::args()
        .nth(1)
        .or_else(|| std::env::var("SUDO_PROMPT").ok())
        .unwrap_or_else(|| "Administrator password required".to_string());
    let password = read_password(&prompt)?;
    println!("{password}");
    Ok(())
}

/// Open `/dev/tty`, switch to raw mode, draw the popup, collect the masked
/// password, restore the terminal, and return it.
fn read_password(prompt: &str) -> anyhow::Result<String> {
    let mut tty = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
        .context("cannot open /dev/tty (no terminal available)")?;
    let _fd = tty.as_raw_fd();

    let original = tcgetattr(&tty).context("tcgetattr failed")?;
    let mut raw = original.clone();
    raw.local_flags
        .remove(LocalFlags::ICANON | LocalFlags::ECHO | LocalFlags::ISIG);
    tcsetattr(&tty, SetArg::TCSANOW, &raw).context("tcsetattr(raw) failed")?;

    let result = draw_and_read(&mut tty, prompt, &original);
    // Restore the terminal after submission or abort.
    let _ = tcsetattr(&tty, SetArg::TCSANOW, &original);
    result
}

fn draw_and_read(
    tty: &mut std::fs::File,
    prompt: &str,
    original: &nix::sys::termios::Termios,
) -> anyhow::Result<String> {
    let (cols, rows) = terminal_size(tty.as_raw_fd());
    // Bordered popup, 5 lines tall, centered.
    let width = (cols.min(60) as usize).max(24);
    let top = rows.saturating_sub(5) / 2;
    let left = cols.saturating_sub(width as u16) / 2;

    let mut out = String::new();
    out.push_str("\x1b[2J"); // clear screen
    out.push_str(&format!("\x1b[{};{}H", top + 1, left + 1));
    out.push_str(CYAN);
    out.push('┌');
    out.push_str(&"─".repeat(width - 2));
    out.push('┐');
    out.push_str(RESET);
    out.push_str(&format!("\x1b[{};{}H", top + 2, left + 1));
    out.push_str(CYAN);
    out.push('│');
    out.push_str(RESET);
    out.push(' ');
    out.push_str(BOLD_YELLOW);
    out.push_str(&pad(&truncate(prompt, width - 4), width - 4));
    out.push_str(RESET);
    out.push(' ');
    out.push_str(CYAN);
    out.push('│');
    out.push_str(RESET);
    out.push_str(&format!("\x1b[{};{}H", top + 3, left + 1));
    out.push_str(CYAN);
    out.push('│');
    out.push_str(RESET);
    out.push(' ');
    out.push_str(BOLD_WHITE);
    out.push_str("Password: ");
    out.push_str(RESET);
    out.push_str(&" ".repeat(width - 14));
    out.push(' ');
    out.push_str(CYAN);
    out.push('│');
    out.push_str(RESET);
    out.push_str(&format!("\x1b[{};{}H", top + 4, left + 1));
    out.push_str(CYAN);
    out.push('│');
    out.push_str(RESET);
    out.push_str(&" ".repeat(width - 2));
    out.push_str(CYAN);
    out.push('│');
    out.push_str(RESET);
    out.push_str(&format!("\x1b[{};{}H", top + 5, left + 1));
    out.push_str(CYAN);
    out.push('└');
    out.push_str(&"─".repeat(width - 2));
    out.push('┘');
    out.push_str(RESET);
    // Cursor to the password field (border + space + "Password: " label).
    out.push_str(&format!("\x1b[{};{}H", top + 3, left + 13));
    out.push_str(BOLD_WHITE);
    write!(tty, "{out}").ok();

    // Collect raw bytes and decode once at the end: a per-byte `char::from`
    // would mangle multibyte UTF-8 passwords into garbage and make sudo
    // retry authentication three times.
    let mut password_bytes: Vec<u8> = Vec::new();
    let mut buf = [0u8; 1];
    loop {
        match tty.read(&mut buf) {
            Ok(0) => break,
            Ok(_) => match buf[0] {
                b'\n' | b'\r' => break,
                0x03 => {
                    // Ctrl-C aborts with 130 (SIGINT convention); restore the
                    // terminal before exiting so the abort leaves a usable tty.
                    let _ = tcsetattr(&*tty, SetArg::TCSANOW, original);
                    let _ = write!(tty, "{RESET}\x1b[2J\x1b[H");
                    std::process::exit(130);
                }
                0x7f | 0x08 => {
                    pop_char(&mut password_bytes);
                    write!(tty, "\x08 \x08").ok();
                }
                byte if byte >= 0x20 => {
                    password_bytes.push(byte);
                    write!(tty, "•").ok();
                }
                _ => {}
            },
            Err(_) => break,
        }
    }
    let _ = write!(tty, "{RESET}\x1b[2J\x1b[H");
    decode_password(password_bytes)
}

/// Remove the last complete UTF-8 character from a raw byte buffer
/// (backspace must delete a whole multibyte character, not one byte).
fn pop_char(bytes: &mut Vec<u8>) {
    if bytes.is_empty() {
        return;
    }
    let mut index = bytes.len() - 1;
    while index > 0 && bytes[index] & 0xC0 == 0x80 {
        index -= 1;
    }
    bytes.truncate(index);
}

/// Decode collected raw bytes into the exact UTF-8 password. Fails rather
/// than silently mangling multibyte input.
fn decode_password(bytes: Vec<u8>) -> anyhow::Result<String> {
    String::from_utf8(bytes).map_err(|error| anyhow::anyhow!("password is not valid UTF-8: {error}"))
}

fn terminal_size(fd: i32) -> (u16, u16) {
    // TIOCGWINSZ via nix's libc re-export.
    let mut ws = nix::libc::winsize {
        ws_row: 0,
        ws_col: 0,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let rc = unsafe { nix::libc::ioctl(fd, nix::libc::TIOCGWINSZ, &mut ws) };
    if rc == 0 && ws.ws_col > 0 {
        (ws.ws_col, ws.ws_row)
    } else {
        (80, 24)
    }
}

fn truncate(text: &str, max: usize) -> String {
    let mut out: String = text.chars().take(max).collect();
    if text.chars().count() > max && max > 1 {
        // pop() removes a whole char, so the cut is always on a UTF-8
        // boundary — a byte-index truncate would panic in the middle of a
        // multibyte char.
        out.pop();
        out.push('…');
    }
    out
}

fn pad(text: &str, width: usize) -> String {
    let text = truncate(text, width);
    let padding = width.saturating_sub(text.chars().count());
    format!("{text}{}", " ".repeat(padding))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pop_char_removes_last_multibyte_character() {
        let mut bytes = "密码".as_bytes().to_vec();
        pop_char(&mut bytes);
        assert_eq!(bytes, "密".as_bytes());
        pop_char(&mut bytes);
        assert!(bytes.is_empty());
        pop_char(&mut bytes);
        assert!(bytes.is_empty());
    }

    #[test]
    fn pop_char_handles_ascii_and_mixed_input() {
        let mut bytes = "a你b".as_bytes().to_vec();
        pop_char(&mut bytes);
        assert_eq!(bytes, "a你".as_bytes());
        pop_char(&mut bytes);
        assert_eq!(bytes, "a".as_bytes());
        pop_char(&mut bytes);
        assert!(bytes.is_empty());
    }

    #[test]
    fn decode_password_preserves_multibyte_input_exactly() {
        let password = "pässwörd🔑";
        let decoded = decode_password(password.as_bytes().to_vec()).expect("valid utf-8");
        assert_eq!(decoded, password);
    }

    #[test]
    fn decode_password_rejects_invalid_utf8() {
        assert!(decode_password(vec![0xE4, 0xBD]).is_err());
    }

    #[test]
    fn truncate_and_pad_keep_box_width() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(
            truncate("a very long prompt that exceeds the box", 16),
            "a very long pro…"
        );
        assert_eq!(pad("ab", 6), "ab    ");
        assert_eq!(pad("abcdefgh", 6), "abcde…");
    }

    #[test]
    fn truncate_is_utf8_boundary_safe_for_multibyte_prompts() {
        // Regression: the old byte-index truncate(max - 1) panicked when
        // max - 1 landed inside a multibyte character.
        let prompt = "你你你你你"; // 5 chars × 3 bytes each
        assert_eq!(truncate(prompt, 4), "你你你…");
        assert_eq!(truncate(prompt, 5), "你你你你你");
        assert_eq!(truncate(prompt, 2), "你…");
        // Mixed multibyte + ASCII prompt.
        assert_eq!(truncate("密码 prompt 很长", 6), "密码 pr…");
    }

    #[test]
    fn truncate_small_widths_never_panic() {
        // Width-1 returns the leading char; width-0 returns an empty string.
        assert_eq!(truncate("你你", 1), "你");
        assert_eq!(truncate("你你", 0), "");
        assert_eq!(truncate("", 4), "");
        assert_eq!(truncate("你", 4), "你");
    }
}
