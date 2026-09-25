use super::Invalid;

/// A parsed value. Mappings keep their keys in document order, because the
/// manifest's canonical form is defined by that order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    Str(String),
    Map(Map),
    List(Vec<Value>),
}

/// A mapping, in document order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Map(pub Vec<(String, Value)>);

impl Map {
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.0.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    pub fn contains(&self, key: &str) -> bool {
        self.get(key).is_some()
    }

    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.0.iter().map(|(k, _)| k.as_str())
    }
}

struct Line<'a> {
    number: usize,
    indent: usize,
    text: &'a str,
}

pub fn parse(text: &str, what: &str) -> Result<Value, Invalid> {
    let mut lines = Vec::new();
    for (index, raw) in text.split('\n').enumerate() {
        let number = index + 1;
        let body = raw.trim_end_matches([' ', '\r']);
        let stripped = body.trim_start_matches(' ');
        let indent = body.len() - stripped.len();
        if stripped.starts_with('\t') {
            return Err(Invalid::at(what, number, "tab in indentation"));
        }
        if stripped.is_empty() || stripped.starts_with('#') {
            continue;
        }
        if stripped == "---" && lines.is_empty() {
            continue;
        }
        lines.push(Line {
            number,
            indent,
            text: stripped,
        });
    }
    let Some(first) = lines.first() else {
        return Err(Invalid::new(format!("{what}: empty document")));
    };
    if first.indent != 0 {
        return Err(Invalid::at(what, first.number, "document is indented"));
    }
    let (value, end) = block(&lines, 0, 0, what)?;
    if end != lines.len() {
        return Err(Invalid::at(
            what,
            lines[end].number,
            "unexpected indentation",
        ));
    }
    Ok(value)
}

fn is_item(text: &str) -> bool {
    text == "-" || text.starts_with("- ")
}

fn block(lines: &[Line], i: usize, indent: usize, what: &str) -> Result<(Value, usize), Invalid> {
    if is_item(lines[i].text) {
        return block_list(lines, i, indent, what);
    }
    block_mapping(lines, i, indent, what)
}

/// The length of a `[A-Za-z_][A-Za-z0-9_-]*` key at the start of `text`.
fn key_length(text: &str) -> usize {
    let bytes = text.as_bytes();
    if !bytes
        .first()
        .is_some_and(|b| b.is_ascii_alphabetic() || *b == b'_')
    {
        return 0;
    }
    bytes
        .iter()
        .position(|b| !(b.is_ascii_alphanumeric() || *b == b'_' || *b == b'-'))
        .unwrap_or(bytes.len())
}

fn block_mapping(
    lines: &[Line],
    mut i: usize,
    indent: usize,
    what: &str,
) -> Result<(Value, usize), Invalid> {
    let mut result = Map::default();
    while i < lines.len() {
        let Line {
            number,
            indent: col,
            text,
        } = lines[i];
        if col < indent {
            break;
        }
        if col > indent {
            return Err(Invalid::at(what, number, "unexpected indentation"));
        }
        if is_item(text) {
            return Err(Invalid::at(
                what,
                number,
                "list item where a key was expected",
            ));
        }
        let length = key_length(text);
        if length == 0 || text.as_bytes().get(length) != Some(&b':') {
            return Err(Invalid::at(what, number, "expected 'key: value'"));
        }
        let key = &text[..length];
        let rest = &text[length + 1..];
        if !rest.is_empty() && !rest.starts_with(' ') {
            return Err(Invalid::at(
                what,
                number,
                &format!("expected a space after '{key}:'"),
            ));
        }
        if result.contains(key) {
            return Err(Invalid::at(what, number, &format!("duplicate key '{key}'")));
        }
        let rest = rest.trim();
        i += 1;
        let value;
        if !rest.is_empty() && !rest.starts_with('#') {
            value = inline(rest, number, what)?;
        } else if i < lines.len() && lines[i].indent > indent {
            (value, i) = block(lines, i, lines[i].indent, what)?;
        } else if i < lines.len() && lines[i].indent == indent && is_item(lines[i].text) {
            // The compact form: list items at the key's own indentation.
            (value, i) = block_list(lines, i, indent, what)?;
        } else {
            value = Value::Null;
        }
        result.0.push((key.to_string(), value));
    }
    Ok((Value::Map(result), i))
}

fn block_list(
    lines: &[Line],
    mut i: usize,
    indent: usize,
    what: &str,
) -> Result<(Value, usize), Invalid> {
    let mut items = Vec::new();
    while i < lines.len() {
        let Line {
            number,
            indent: col,
            text,
        } = lines[i];
        if col < indent {
            break;
        }
        if col > indent {
            return Err(Invalid::at(what, number, "unexpected indentation"));
        }
        if !is_item(text) {
            break;
        }
        let rest = text[1..].trim();
        if rest.is_empty() || rest.starts_with('#') {
            return Err(Invalid::at(
                what,
                number,
                "nested blocks in a list are not supported",
            ));
        }
        let length = key_length(rest);
        if length > 0
            && rest.as_bytes().get(length) == Some(&b':')
            && matches!(rest.as_bytes().get(length + 1), None | Some(b' '))
        {
            return Err(Invalid::at(
                what,
                number,
                "block mappings in a list are not supported; write the item as {key: value, ...}",
            ));
        }
        items.push(inline(rest, number, what)?);
        i += 1;
    }
    Ok((Value::List(items), i))
}

fn inline(text: &str, number: usize, what: &str) -> Result<Value, Invalid> {
    let (value, pos) = scalar_or_flow(text, 0, number, what, false)?;
    let tail = text[pos..].trim();
    if !tail.is_empty() && !tail.starts_with('#') {
        return Err(Invalid::at(
            what,
            number,
            &format!("unexpected '{tail}' after value"),
        ));
    }
    Ok(value)
}

fn skip(text: &str, mut pos: usize) -> usize {
    while text.as_bytes().get(pos) == Some(&b' ') {
        pos += 1;
    }
    pos
}

fn scalar_or_flow(
    text: &str,
    pos: usize,
    number: usize,
    what: &str,
    flow: bool,
) -> Result<(Value, usize), Invalid> {
    let pos = skip(text, pos);
    let Some(&c) = text.as_bytes().get(pos) else {
        return Err(Invalid::at(what, number, "missing value"));
    };
    match c {
        b'"' => {
            let (value, end) = json_string(text, pos)
                .ok_or_else(|| Invalid::at(what, number, "malformed double-quoted string"))?;
            Ok((Value::Str(value), end))
        }
        b'\'' => {
            let mut out = String::new();
            let mut j = pos + 1;
            loop {
                let Some(k) = text[j..].find('\'').map(|k| k + j) else {
                    return Err(Invalid::at(
                        what,
                        number,
                        "unterminated single-quoted string",
                    ));
                };
                out.push_str(&text[j..k]);
                if text.as_bytes().get(k + 1) == Some(&b'\'') {
                    out.push('\'');
                    j = k + 2;
                    continue;
                }
                return Ok((Value::Str(out), k + 1));
            }
        }
        b'{' => flow_mapping(text, pos, number, what),
        b'[' => flow_list(text, pos, number, what),
        b'&' | b'*' | b'!' | b'|' | b'>' | b'%' | b'@' | b'`' | b',' | b']' | b'}' | b'#' => {
            Err(Invalid::at(
                what,
                number,
                &format!("unsupported YAML construct '{}'", c as char),
            ))
        }
        _ => {
            let mut end = text.len();
            let stops: &[&str] = if flow {
                &[" #", ",", "]", "}"]
            } else {
                &[" #"]
            };
            for stop in stops {
                if let Some(k) = text[pos..].find(stop) {
                    end = end.min(pos + k);
                }
            }
            let token = text[pos..end].trim();
            if token.contains(": ") || token.ends_with(':') {
                return Err(Invalid::at(
                    what,
                    number,
                    &format!("ambiguous plain scalar '{token}'; quote it"),
                ));
            }
            Ok((plain(token, number, what)?, end))
        }
    }
}

fn plain(token: &str, number: usize, what: &str) -> Result<Value, Invalid> {
    match token {
        "null" | "Null" | "NULL" | "~" => return Ok(Value::Null),
        "true" | "True" | "TRUE" => return Ok(Value::Bool(true)),
        "false" | "False" | "FALSE" => return Ok(Value::Bool(false)),
        _ => {}
    }
    let unsigned = token.strip_prefix(['-', '+']).unwrap_or(token);
    if !unsigned.is_empty() && unsigned.bytes().all(|b| b.is_ascii_digit()) {
        if unsigned.len() > 1 && unsigned.starts_with('0') {
            // 0755 is octal to YAML 1.1 and decimal to 1.2.
            return Err(Invalid::at(
                what,
                number,
                &format!("'{token}' has a leading zero; quote it"),
            ));
        }
        return token
            .parse::<i64>()
            .map(Value::Int)
            .map_err(|_| Invalid::at(what, number, &format!("'{token}' is out of range")));
    }
    if is_float(token) {
        return Err(Invalid::at(
            what,
            number,
            &format!("'{token}' would be a float; quote it"),
        ));
    }
    Ok(Value::Str(token.to_string()))
}

/// `[-+]?(\.[0-9]+|[0-9]+\.[0-9]*)([eE][-+]?[0-9]+)?|[-+]?\.(inf|Inf|INF)|\.(nan|NaN|NAN)`
fn is_float(token: &str) -> bool {
    if matches!(token, ".nan" | ".NaN" | ".NAN") {
        return true;
    }
    let body = token.strip_prefix(['-', '+']).unwrap_or(token);
    if matches!(body, ".inf" | ".Inf" | ".INF") {
        return true;
    }
    let (mantissa, exponent) = match body.find(['e', 'E']) {
        Some(at) => (&body[..at], Some(&body[at + 1..])),
        None => (body, None),
    };
    let Some((whole, fraction)) = mantissa.split_once('.') else {
        return false;
    };
    let digits = |s: &str| s.bytes().all(|b| b.is_ascii_digit());
    let mantissa_ok = if whole.is_empty() {
        !fraction.is_empty() && digits(fraction)
    } else {
        digits(whole) && digits(fraction)
    };
    let exponent_ok = exponent.is_none_or(|e| {
        let e = e.strip_prefix(['-', '+']).unwrap_or(e);
        !e.is_empty() && digits(e)
    });
    mantissa_ok && exponent_ok
}

fn flow_mapping(
    text: &str,
    pos: usize,
    number: usize,
    what: &str,
) -> Result<(Value, usize), Invalid> {
    let mut result = Map::default();
    let mut pos = skip(text, pos + 1);
    if text.as_bytes().get(pos) == Some(&b'}') {
        return Ok((Value::Map(result), pos + 1));
    }
    loop {
        pos = skip(text, pos);
        let length = key_length(&text[pos..]);
        if length == 0 || text.as_bytes().get(pos + length) != Some(&b':') {
            return Err(Invalid::at(
                what,
                number,
                "expected 'key: value' in flow mapping",
            ));
        }
        let key = &text[pos..pos + length];
        if result.contains(key) {
            return Err(Invalid::at(what, number, &format!("duplicate key '{key}'")));
        }
        let (value, end) = scalar_or_flow(text, pos + length + 1, number, what, true)?;
        result.0.push((key.to_string(), value));
        pos = skip(text, end);
        match text.as_bytes().get(pos) {
            Some(b',') => pos += 1,
            Some(b'}') => return Ok((Value::Map(result), pos + 1)),
            _ => {
                return Err(Invalid::at(
                    what,
                    number,
                    "expected ',' or '}' in flow mapping",
                ));
            }
        }
    }
}

fn flow_list(text: &str, pos: usize, number: usize, what: &str) -> Result<(Value, usize), Invalid> {
    let mut items = Vec::new();
    let mut pos = skip(text, pos + 1);
    if text.as_bytes().get(pos) == Some(&b']') {
        return Ok((Value::List(items), pos + 1));
    }
    loop {
        let (value, end) = scalar_or_flow(text, pos, number, what, true)?;
        items.push(value);
        pos = skip(text, end);
        match text.as_bytes().get(pos) {
            Some(b',') => pos += 1,
            Some(b']') => return Ok((Value::List(items), pos + 1)),
            _ => {
                return Err(Invalid::at(
                    what,
                    number,
                    "expected ',' or ']' in flow list",
                ));
            }
        }
    }
}

/// A JSON string starting at `text[pos]`, which is `"`, and the index after
/// its closing quote. Strict, as Python's decoder is: no raw control
/// characters, and every escape one JSON defines.
fn json_string(text: &str, pos: usize) -> Option<(String, usize)> {
    let bytes = text.as_bytes();
    let mut out = String::new();
    let mut i = pos + 1;
    loop {
        let c = *bytes.get(i)?;
        match c {
            b'"' => return Some((out, i + 1)),
            b'\\' => {
                let escape = *bytes.get(i + 1)?;
                i += 2;
                match escape {
                    b'"' => out.push('"'),
                    b'\\' => out.push('\\'),
                    b'/' => out.push('/'),
                    b'b' => out.push('\u{8}'),
                    b'f' => out.push('\u{c}'),
                    b'n' => out.push('\n'),
                    b'r' => out.push('\r'),
                    b't' => out.push('\t'),
                    b'u' => {
                        let high = hex4(bytes.get(i..i + 4)?)?;
                        i += 4;
                        let code = if (0xd800..0xdc00).contains(&high)
                            && bytes.get(i..i + 2) == Some(b"\\u")
                            && let Some(low) = bytes.get(i + 2..i + 6).and_then(hex4)
                            && (0xdc00..0xe000).contains(&low)
                        {
                            i += 6;
                            0x10000 + ((high - 0xd800) << 10) + (low - 0xdc00)
                        } else {
                            high
                        };
                        // A lone surrogate is legal JSON and no Rust string.
                        out.push(char::from_u32(code)?);
                    }
                    _ => return None,
                }
            }
            0..0x20 => return None,
            _ => {
                let ch = text[i..].chars().next()?;
                out.push(ch);
                i += ch.len_utf8();
            }
        }
    }
}

fn hex4(bytes: &[u8]) -> Option<u32> {
    u32::from_str_radix(std::str::from_utf8(bytes).ok()?, 16).ok()
}

/// A string as Python's `json.dumps(value, ensure_ascii=True)` writes it,
/// which is how every string in a canonical manifest is written.
pub fn quote(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 || (c as u32) > 0x7f => {
                let mut units = [0u16; 2];
                for unit in c.encode_utf16(&mut units) {
                    out.push_str(&format!("\\u{unit:04x}"));
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(text: &str) -> Map {
        match parse(text, "test").expect("parses") {
            Value::Map(map) => map,
            other => panic!("not a mapping: {other:?}"),
        }
    }

    #[test]
    fn reads_the_shapes_a_spec_uses() {
        let doc = map(concat!(
            "# comment\n",
            "format: 1\n",
            "identity:\n",
            "  arch: \"x86_64\"\n",
            "  min_os: '26.0'\n",
            "artifact:\n",
            "  sha256: null\n",
            "  sources:\n",
            "    - \"https://example.com/a\"\n",
            "entries:\n",
            "  - {path: \"/usr\", type: directory, size: 3}\n",
            "flow: [a, \"b\", 3]\n",
            "empty:\n",
        ));
        assert_eq!(doc.get("format"), Some(&Value::Int(1)));
        let Some(Value::Map(identity)) = doc.get("identity") else {
            panic!()
        };
        assert_eq!(identity.get("min_os"), Some(&Value::Str("26.0".into())));
        let Some(Value::Map(artifact)) = doc.get("artifact") else {
            panic!()
        };
        assert_eq!(artifact.get("sha256"), Some(&Value::Null));
        let Some(Value::List(entries)) = doc.get("entries") else {
            panic!()
        };
        let Value::Map(entry) = &entries[0] else {
            panic!()
        };
        assert_eq!(entry.get("type"), Some(&Value::Str("directory".into())));
        assert_eq!(entry.get("size"), Some(&Value::Int(3)));
        assert_eq!(
            doc.get("flow"),
            Some(&Value::List(vec![
                Value::Str("a".into()),
                Value::Str("b".into()),
                Value::Int(3)
            ]))
        );
        assert_eq!(doc.get("empty"), Some(&Value::Null));
    }

    #[test]
    fn a_compact_list_sits_at_its_keys_indentation() {
        let doc = map("sources:\n- \"a\"\n- \"b\"\nnext: 1\n");
        assert_eq!(
            doc.get("sources"),
            Some(&Value::List(vec![
                Value::Str("a".into()),
                Value::Str("b".into())
            ]))
        );
        assert_eq!(doc.get("next"), Some(&Value::Int(1)));
    }

    /// The two traps the format calls out, and every other construct the
    /// reference parser refuses.
    #[test]
    fn refuses_what_other_parsers_would_misread() {
        for bad in [
            "min_os: 26.0\n",
            "min_os: .5\n",
            "x: 1.5e5\n",
            "x: .inf\n",
            "mode: 0755\n",
            "a: &anchor 1\n",
            "a: !tag 1\n",
            "a: |\n  text\n",
            "a: 1\na: 2\n",
            "a: b: c\n",
            "\ta: 1\n",
            "  a: 1\n",
            "list:\n  - a: 1\n",
            "a: \"unterminated\n",
            "a: \"bad \\q escape\"\n",
            "",
        ] {
            assert!(parse(bad, "test").is_err(), "{bad:?} was accepted");
        }
    }

    #[test]
    fn a_quoted_float_or_octal_is_a_string() {
        let doc = map("a: \"26.0\"\nb: \"0755\"\nc: 1.2.3\n");
        assert_eq!(doc.get("a"), Some(&Value::Str("26.0".into())));
        assert_eq!(doc.get("b"), Some(&Value::Str("0755".into())));
        assert_eq!(doc.get("c"), Some(&Value::Str("1.2.3".into())));
    }

    #[test]
    fn json_escapes_round_trip() {
        let original = "tab\there \"quoted\" back\\slash \u{1} caf\u{e9} \u{1f600}";
        let quoted = quote(original);
        assert!(quoted.is_ascii());
        let doc = map(&format!("s: {quoted}\n"));
        assert_eq!(doc.get("s"), Some(&Value::Str(original.into())));
    }
}
