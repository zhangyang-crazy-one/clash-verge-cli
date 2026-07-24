/// Replace terminal-unsafe emoji with fixed-width ASCII badges.
///
/// Terminal emulators do not agree on the cell width of emoji, variation
/// selectors, and joined emoji sequences. Ratatui can reserve a `Rect`, but it
/// cannot stop the terminal from advancing its physical cursor by a different
/// number of cells while it renders that `Rect`. Keep untrusted labels ASCII so
/// their measured and rendered widths always agree.
pub fn display(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut chars = value.chars().peekable();

    while let Some(character) = chars.next() {
        let Some(first) = regional_indicator_letter(character) else {
            if is_keycap_start(character, &chars) || is_emoji_base(character) {
                output.push_str("[emoji]");
                skip_emoji_suffix(&mut chars);
                continue;
            }
            output.push(character);
            continue;
        };
        let Some(second) = chars.peek().copied().and_then(regional_indicator_letter) else {
            output.push(character);
            continue;
        };

        chars.next();
        output.push('[');
        output.push(first);
        output.push(second);
        output.push(']');
    }

    output
}

fn is_keycap_start(character: char, chars: &std::iter::Peekable<std::str::Chars<'_>>) -> bool {
    if !matches!(character, '#' | '*' | '0'..='9') {
        return false;
    }

    let mut following = chars.clone();
    if following.peek() == Some(&'\u{FE0F}') {
        following.next();
    }
    following.next() == Some('\u{20E3}')
}

fn skip_emoji_suffix(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    while chars.peek().is_some_and(|character| is_emoji_suffix(*character)) {
        chars.next();
    }

    while chars.peek() == Some(&'\u{200D}') {
        chars.next();
        if chars.next().is_none() {
            return;
        }
        while chars.peek().is_some_and(|character| is_emoji_suffix(*character)) {
            chars.next();
        }
    }
}

fn is_emoji_suffix(character: char) -> bool {
    matches!(character, '\u{FE0F}' | '\u{FE0E}' | '\u{20E3}') || is_emoji_modifier(character)
}

fn is_emoji_modifier(character: char) -> bool {
    matches!(character as u32, 0x1F3FB..=0x1F3FF)
}

fn is_emoji_base(character: char) -> bool {
    matches!(character as u32,
        0x00A9 | 0x00AE | 0x203C | 0x2049 | 0x2122 | 0x2139 | 0x3030 | 0x303D | 0x3297 | 0x3299
            | 0x2194..=0x21FF | 0x2300..=0x23FF | 0x2600..=0x27BF | 0x2934..=0x2935 | 0x2B00..=0x2BFF
            | 0x1F000..=0x1FAFF
    )
}

fn regional_indicator_letter(character: char) -> Option<char> {
    let value = character as u32;
    let offset = value.checked_sub(0x1F1E6)?;
    (offset < 26)
        .then(|| char::from_u32(u32::from(b'A') + offset))
        .flatten()
}

#[cfg(test)]
mod tests {
    use super::display;

    #[test]
    fn replaces_terminal_unsafe_emoji_with_ascii_badges() {
        assert_eq!(display("🇦🇪13迪拜"), "[AE]13迪拜");
        assert_eq!(display("♻️自动选择"), "[emoji]自动选择");
        assert_eq!(display("A👩‍💻B1️⃣C"), "A[emoji]B[emoji]C");
        assert_eq!(display("plain text"), "plain text");
    }
}
