/// Replace terminal-unsafe country-flag emoji with fixed-width ASCII badges.
///
/// Orca currently advances its terminal cursor inconsistently for regional
/// indicator pairs. Rendering `[US]` instead of `🇺🇸` preserves the country
/// information without shifting later cells, including panel borders.
pub fn display(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut chars = value.chars().peekable();

    while let Some(character) = chars.next() {
        let Some(first) = regional_indicator_letter(character) else {
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
    fn replaces_country_flags_without_touching_other_symbols() {
        assert_eq!(display("🇦🇪13迪拜"), "[AE]13迪拜");
        assert_eq!(display("♻️自动选择"), "♻️自动选择");
        assert_eq!(display("plain text"), "plain text");
    }
}
