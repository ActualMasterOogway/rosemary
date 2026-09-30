//! Values spelled as JavaScript source.

/// A JSON string literal, which is also a valid JavaScript one.
pub fn quote(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            // Line and paragraph separators end a line in older engines' string
            // literals; control characters are not allowed in one at all.
            '\u{2028}' | '\u{2029}' => out.push_str(&format!("\\u{:04x}", c as u32)),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting_escapes_what_ends_a_string_literal() {
        assert_eq!(
            quote("a\"b\\c\nd\u{2028}\u{1}"),
            "\"a\\\"b\\\\c\\nd\\u2028\\u0001\""
        );
    }
}
