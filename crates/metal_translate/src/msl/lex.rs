//! Tokens of preprocessed Metal source.

use super::syntax_error;
use crate::TranslateError;

#[derive(Clone, Debug, PartialEq)]
pub(super) enum Kind {
    Ident(String),
    /// An integer literal. `unsigned` is its type after C++'s rules: a `u`
    /// suffix, or a hexadecimal literal too large for `int`.
    Int {
        value: u64,
        unsigned: bool,
    },
    /// A floating literal. `half` for an `h` suffix.
    Float {
        value: f64,
        half: bool,
    },
    Punct(&'static str),
}

#[derive(Clone, Debug)]
pub(super) struct Token {
    pub kind: Kind,
    pub line: usize,
}

const PUNCTUATORS: &[&str] = &[
    "<<=", ">>=", "...", "::", "->", "++", "--", "<<", ">>", "<=", ">=", "==", "!=", "&&", "||",
    "+=", "-=", "*=", "/=", "%=", "&=", "|=", "^=", "{", "}", "[", "]", "(", ")", "<", ">", ";",
    ",", ".", ":", "+", "-", "*", "/", "%", "&", "|", "^", "!", "~", "?", "=", "#",
];

/// Split `text` into tokens; `first_line` is the line `text` starts on.
pub(super) fn lex(text: &str, first_line: usize) -> Result<Vec<Token>, TranslateError> {
    let bytes = text.as_bytes();
    let mut tokens = Vec::new();
    let mut line = first_line;
    let mut at = 0;
    while at < bytes.len() {
        let c = bytes[at];
        if c == b'\n' {
            line += 1;
            at += 1;
            continue;
        }
        if c.is_ascii_whitespace() {
            at += 1;
            continue;
        }
        if c.is_ascii_alphabetic() || c == b'_' {
            let start = at;
            while at < bytes.len() && (bytes[at].is_ascii_alphanumeric() || bytes[at] == b'_') {
                at += 1;
            }
            tokens.push(Token {
                kind: Kind::Ident(text[start..at].to_string()),
                line,
            });
            continue;
        }
        if c.is_ascii_digit() || (c == b'.' && bytes.get(at + 1).is_some_and(u8::is_ascii_digit)) {
            let (kind, end) = number(text, at, line)?;
            tokens.push(Token { kind, line });
            at = end;
            continue;
        }
        let Some(punct) = PUNCTUATORS
            .iter()
            .find(|punct| text[at..].starts_with(**punct))
        else {
            return Err(syntax_error(
                line,
                format!(
                    "unexpected character {:?}",
                    text[at..].chars().next().unwrap_or('?')
                ),
            ));
        };
        tokens.push(Token {
            kind: Kind::Punct(punct),
            line,
        });
        at += punct.len();
    }
    Ok(tokens)
}

fn number(text: &str, start: usize, line: usize) -> Result<(Kind, usize), TranslateError> {
    let bytes = text.as_bytes();
    let mut at = start;
    let hex = text[start..].starts_with("0x") || text[start..].starts_with("0X");
    if hex {
        at += 2;
        while at < bytes.len() && bytes[at].is_ascii_hexdigit() {
            at += 1;
        }
        let digits = &text[start + 2..at];
        let value = u64::from_str_radix(digits, 16)
            .map_err(|_| syntax_error(line, format!("malformed literal 0x{digits}")))?;
        let (unsigned, end) = integer_suffix(text, at);
        // A hexadecimal literal which does not fit `int` is `unsigned int`.
        let unsigned = unsigned || value > i32::MAX as u64;
        return Ok((Kind::Int { value, unsigned }, end));
    }
    let mut floating = false;
    while at < bytes.len() {
        match bytes[at] {
            b'0'..=b'9' => at += 1,
            b'.' => {
                floating = true;
                at += 1;
            }
            b'e' | b'E' => {
                floating = true;
                at += 1;
                if matches!(bytes.get(at), Some(b'+' | b'-')) {
                    at += 1;
                }
            }
            _ => break,
        }
    }
    let digits = &text[start..at];
    match bytes.get(at) {
        Some(b'f' | b'F') => floating = true,
        Some(b'h' | b'H') => {
            let value = digits
                .parse::<f64>()
                .map_err(|_| syntax_error(line, format!("malformed literal {digits}")))?;
            return Ok((Kind::Float { value, half: true }, at + 1));
        }
        _ => {}
    }
    if floating {
        let value = digits
            .parse::<f64>()
            .map_err(|_| syntax_error(line, format!("malformed literal {digits}")))?;
        let end = if matches!(bytes.get(at), Some(b'f' | b'F')) {
            at + 1
        } else {
            at
        };
        return Ok((Kind::Float { value, half: false }, end));
    }
    // An octal literal is a leading zero followed by digits; Metal source
    // spells none, and reading one as decimal would be silently wrong.
    if digits.len() > 1 && digits.starts_with('0') {
        return Err(syntax_error(
            line,
            format!("octal literal {digits} is not supported"),
        ));
    }
    let value = digits
        .parse::<u64>()
        .map_err(|_| syntax_error(line, format!("malformed literal {digits}")))?;
    let (unsigned, end) = integer_suffix(text, at);
    let limit = if unsigned {
        u64::from(u32::MAX)
    } else {
        i32::MAX as u64
    };
    if value > limit {
        // C++ makes such a literal a `long`, which this frontend does not
        // model.
        return Err(syntax_error(
            line,
            format!("literal {digits} does not fit 32 bits"),
        ));
    }
    Ok((Kind::Int { value, unsigned }, end))
}

/// Whether an integer suffix makes the literal unsigned, and where it ends.
fn integer_suffix(text: &str, mut at: usize) -> (bool, usize) {
    let bytes = text.as_bytes();
    let mut unsigned = false;
    while let Some(c) = bytes.get(at) {
        match c {
            b'u' | b'U' => unsigned = true,
            b'l' | b'L' => {}
            _ => break,
        }
        at += 1;
    }
    (unsigned, at)
}
