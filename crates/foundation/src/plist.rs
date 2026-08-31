//! Reading a property list.
//!
//! Needed before anything else in a bundle can be answered: `Info.plist` is what
//! says which class is the principal one and which nib is the main one, and a
//! `.app` whose `Info.plist` cannot be read is a `.app` with no identity at all.
//!
//! Both spellings a `.app` uses are here. The binary one is what Xcode writes
//! and what Deltarune has; the XML one is what a hand-edited plist still looks
//! like, and reading only the first would fail on the second with nothing to say
//! why.
//!
//! Not a general plist library: what is decoded is what a bundle and a nib are
//! made of. A type nobody has needed is [`Value::Unsupported`] with its marker
//! in it, which is a name a reader can report rather than a silent `nil`.

use std::collections::BTreeMap;

/// One value in a property list.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Integer(i64),
    Real(f64),
    String(String),
    Data(Vec<u8>),
    /// Seconds since 2001-01-01, which is Apple's epoch.
    Date(f64),
    Array(Vec<Value>),
    Dictionary(BTreeMap<String, Value>),
    /// A reference into the object table, which only an archive uses.
    Uid(u64),
    /// A marker this reader does not decode, kept so it can be named.
    Unsupported(u8),
}

impl Value {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::String(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_dictionary(&self) -> Option<&BTreeMap<String, Value>> {
        match self {
            Value::Dictionary(d) => Some(d),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&[Value]> {
        match self {
            Value::Array(a) => Some(a),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Value::Integer(v) => Some(*v),
            Value::Uid(v) => Some(*v as i64),
            _ => None,
        }
    }

    /// The value at `key`, for a dictionary.
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.as_dictionary()?.get(key)
    }
}

/// What went wrong, as a sentence naming the offset.
#[derive(Debug, PartialEq)]
pub struct Error(pub String);

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

fn bad(what: impl std::fmt::Display) -> Error {
    Error(what.to_string())
}

/// Parse a property list, in whichever of the two spellings it is written.
pub fn parse(bytes: &[u8]) -> Result<Value, Error> {
    match bytes.starts_with(b"bplist00") {
        true => binary::parse(bytes),
        false => xml::parse(bytes),
    }
}

/// The binary spelling: a trailer that says how wide the offsets are, a table of
/// them, and one tagged object at each.
mod binary {
    use super::*;

    struct Reader<'a> {
        bytes: &'a [u8],
        offsets: Vec<usize>,
        ref_size: usize,
    }

    pub fn parse(bytes: &[u8]) -> Result<Value, Error> {
        // The trailer is the last 32 bytes: six unused, then the offset width,
        // the reference width, the object count, the root, and where the offset
        // table starts.
        if bytes.len() < 40 {
            return Err(bad("a binary plist is at least forty bytes"));
        }
        let trailer = &bytes[bytes.len() - 32..];
        let offset_size = trailer[6] as usize;
        let ref_size = trailer[7] as usize;
        let count = be(&trailer[8..16]) as usize;
        let root = be(&trailer[16..24]) as usize;
        let table = be(&trailer[24..32]) as usize;
        if offset_size == 0 || offset_size > 8 || ref_size == 0 || ref_size > 8 {
            return Err(bad("a binary plist trailer with an impossible width"));
        }
        let end = table
            .checked_add(count * offset_size)
            .ok_or_else(|| bad("an offset table past the end of the file"))?;
        if end > bytes.len() {
            return Err(bad("an offset table past the end of the file"));
        }

        let offsets: Vec<usize> = (0..count)
            .map(|i| be(&bytes[table + i * offset_size..][..offset_size]) as usize)
            .collect();
        let reader = Reader {
            bytes,
            offsets,
            ref_size,
        };
        reader.object(root, 0)
    }

    /// A big-endian integer of one to eight bytes.
    fn be(bytes: &[u8]) -> u64 {
        bytes.iter().fold(0u64, |v, b| (v << 8) | *b as u64)
    }

    /// How deep a plist may nest.
    ///
    /// A file with a cycle in its offset table would otherwise recurse until the
    /// stack ran out, and a bundle is something this runtime reads from disk on
    /// a guest's say-so.
    const MAX_DEPTH: usize = 64;

    impl Reader<'_> {
        fn object(&self, index: usize, depth: usize) -> Result<Value, Error> {
            if depth > MAX_DEPTH {
                return Err(bad("a plist nested past sixty-four levels"));
            }
            let at = *self
                .offsets
                .get(index)
                .ok_or_else(|| bad(format!("object {index} is not in the offset table")))?;
            let marker = *self
                .bytes
                .get(at)
                .ok_or_else(|| bad(format!("object {index} is past the end of the file")))?;
            let (kind, low) = (marker >> 4, (marker & 0x0f) as usize);
            match kind {
                0x0 => Ok(match low {
                    0 => Value::Null,
                    8 => Value::Bool(false),
                    9 => Value::Bool(true),
                    _ => Value::Unsupported(marker),
                }),
                // An integer's length is 2^low bytes, and only an eight-byte one
                // is signed - which is why a `1 << 3` is read as `i64` and the
                // rest are not.
                0x1 => {
                    let width = 1usize << low;
                    let raw = be(self.slice(at + 1, width)?);
                    Ok(Value::Integer(match width {
                        8 => raw as i64,
                        _ => raw as i64,
                    }))
                }
                0x2 => {
                    let width = 1usize << low;
                    let raw = self.slice(at + 1, width)?;
                    Ok(Value::Real(match width {
                        4 => f32::from_be_bytes(raw.try_into().unwrap_or_default()) as f64,
                        8 => f64::from_be_bytes(raw.try_into().unwrap_or_default()),
                        _ => return Err(bad("a real that is neither four nor eight bytes")),
                    }))
                }
                0x3 => Ok(Value::Date(f64::from_be_bytes(
                    self.slice(at + 1, 8)?.try_into().unwrap_or_default(),
                ))),
                0x4 => {
                    let (len, body) = self.counted(at, low)?;
                    Ok(Value::Data(self.slice(body, len)?.to_vec()))
                }
                // ASCII, one byte a character.
                0x5 => {
                    let (len, body) = self.counted(at, low)?;
                    Ok(Value::String(
                        String::from_utf8_lossy(self.slice(body, len)?).into_owned(),
                    ))
                }
                // UTF-16, big-endian, `len` *characters* rather than bytes.
                0x6 => {
                    let (len, body) = self.counted(at, low)?;
                    let raw = self.slice(body, len * 2)?;
                    let units: Vec<u16> = raw
                        .chunks_exact(2)
                        .map(|c| u16::from_be_bytes([c[0], c[1]]))
                        .collect();
                    Ok(Value::String(String::from_utf16_lossy(&units)))
                }
                0x8 => Ok(Value::Uid(be(self.slice(at + 1, low + 1)?))),
                0xa | 0xc => {
                    let (len, body) = self.counted(at, low)?;
                    let mut out = Vec::with_capacity(len);
                    for i in 0..len {
                        let r = be(self.slice(body + i * self.ref_size, self.ref_size)?) as usize;
                        out.push(self.object(r, depth + 1)?);
                    }
                    Ok(Value::Array(out))
                }
                0xd => {
                    let (len, body) = self.counted(at, low)?;
                    let mut out = BTreeMap::new();
                    for i in 0..len {
                        let k = be(self.slice(body + i * self.ref_size, self.ref_size)?) as usize;
                        let v = be(self.slice(body + (len + i) * self.ref_size, self.ref_size)?)
                            as usize;
                        let key = match self.object(k, depth + 1)? {
                            Value::String(s) => s,
                            // A non-string key is legal in the format and means
                            // nothing to a reader of a bundle; named rather than
                            // dropped.
                            other => format!("<non-string key {other:?}>"),
                        };
                        out.insert(key, self.object(v, depth + 1)?);
                    }
                    Ok(Value::Dictionary(out))
                }
                _ => Ok(Value::Unsupported(marker)),
            }
        }

        /// The length and the body offset of a counted object.
        ///
        /// A `low` of fifteen means the count did not fit in the nibble and is
        /// an integer object that follows.
        fn counted(&self, at: usize, low: usize) -> Result<(usize, usize), Error> {
            if low != 0x0f {
                return Ok((low, at + 1));
            }
            let marker = *self
                .bytes
                .get(at + 1)
                .ok_or_else(|| bad("a counted object with no count"))?;
            if marker >> 4 != 0x1 {
                return Err(bad("a counted object whose count is not an integer"));
            }
            let width = 1usize << (marker & 0x0f);
            Ok((be(self.slice(at + 2, width)?) as usize, at + 2 + width))
        }

        fn slice(&self, at: usize, len: usize) -> Result<&[u8], Error> {
            self.bytes
                .get(at..at + len)
                .ok_or_else(|| bad(format!("a value at {at} runs past the end of the file")))
        }
    }
}

/// The XML spelling, read by shape rather than by a general parser.
///
/// Enough for an `Info.plist` a person edited: the tags are a fixed set and they
/// nest, so a stack of containers and a tag scanner is the whole of it.
mod xml {
    use super::*;

    pub fn parse(bytes: &[u8]) -> Result<Value, Error> {
        let text: String = String::from_utf8_lossy(bytes).into_owned();
        let mut stack: Vec<Value> = Vec::new();
        let mut keys: Vec<Option<String>> = Vec::new();
        let mut root = None;
        let mut rest: &str = &text;

        while let Some(open) = rest.find('<') {
            let close = match rest[open..].find('>') {
                Some(at) => open + at,
                None => break,
            };
            let tag = &rest[open + 1..close];
            let body_start = close + 1;
            let body_end = rest[body_start..].find('<').map(|at| body_start + at);
            let body = body_end.map(|end| &rest[body_start..end]).unwrap_or("");

            let mut push =
                |value: Value, stack: &mut Vec<Value>, keys: &mut Vec<Option<String>>| match stack
                    .last_mut()
                {
                    Some(Value::Array(a)) => a.push(value),
                    Some(Value::Dictionary(d)) => {
                        if let Some(Some(key)) = keys.last_mut().map(std::mem::take) {
                            d.insert(key, value);
                        }
                    }
                    _ => root = Some(value),
                };

            match tag {
                "dict" => {
                    stack.push(Value::Dictionary(BTreeMap::new()));
                    keys.push(None);
                }
                "array" => {
                    stack.push(Value::Array(Vec::new()));
                    keys.push(None);
                }
                "/dict" | "/array" => {
                    let done = stack
                        .pop()
                        .ok_or_else(|| bad("a closing tag with nothing open"))?;
                    keys.pop();
                    push(done, &mut stack, &mut keys);
                }
                "key" => {
                    if let Some(slot) = keys.last_mut() {
                        *slot = Some(unescape(body));
                    }
                }
                "string" => push(Value::String(unescape(body)), &mut stack, &mut keys),
                "integer" => push(
                    Value::Integer(body.trim().parse().unwrap_or(0)),
                    &mut stack,
                    &mut keys,
                ),
                "real" => push(
                    Value::Real(body.trim().parse().unwrap_or(0.0)),
                    &mut stack,
                    &mut keys,
                ),
                "true/" => push(Value::Bool(true), &mut stack, &mut keys),
                "false/" => push(Value::Bool(false), &mut stack, &mut keys),
                _ => {}
            }
            rest = &rest[close + 1..];
        }
        root.ok_or_else(|| bad("an XML plist with no root value"))
    }

    fn unescape(text: &str) -> String {
        text.replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&quot;", "\"")
            .replace("&apos;", "'")
            .replace("&amp;", "&")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_xml_plist_reads_the_keys_a_bundle_needs() {
        let text = br#"<?xml version="1.0"?>
        <plist version="1.0">
        <dict>
            <key>CFBundleExecutable</key><string>Mac_Runner</string>
            <key>NSPrincipalClass</key><string>YYApplication</string>
            <key>NSMainNibFile</key><string>MainMenu</string>
            <key>LSMinimumSystemVersion</key><string>10.9.0</string>
            <key>CFBundleSupportedPlatforms</key>
            <array><string>MacOSX</string></array>
            <key>NSHighResolutionCapable</key><true/>
        </dict>
        </plist>"#;
        let value = parse(text).expect("an XML plist");
        assert_eq!(
            value.get("CFBundleExecutable").and_then(Value::as_str),
            Some("Mac_Runner")
        );
        assert_eq!(
            value.get("NSPrincipalClass").and_then(Value::as_str),
            Some("YYApplication")
        );
        assert_eq!(
            value.get("NSHighResolutionCapable"),
            Some(&Value::Bool(true))
        );
        assert_eq!(
            value
                .get("CFBundleSupportedPlatforms")
                .and_then(Value::as_array),
            Some(&[Value::String("MacOSX".into())][..])
        );
    }

    /// A binary plist built here rather than checked in: the point is the
    /// decoding, and a fixture on disk would only move the question.
    #[test]
    fn a_binary_plist_reads_the_same_shape() {
        // { "a": 1, "b": "two" } - one dictionary, two ASCII keys, an integer
        // and a string, laid out by hand against the format above.
        let mut out = Vec::new();
        out.extend_from_slice(b"bplist00");
        let mut offsets = Vec::new();
        offsets.push(out.len());
        out.push(0xd2); // dict, two entries
        out.extend_from_slice(&[1, 2, 3, 4]); // key refs then value refs
        offsets.push(out.len());
        out.extend_from_slice(&[0x51, b'a']); // "a"
        offsets.push(out.len());
        out.extend_from_slice(&[0x51, b'b']); // "b"
        offsets.push(out.len());
        out.extend_from_slice(&[0x10, 1]); // 1
        offsets.push(out.len());
        out.extend_from_slice(&[0x53, b't', b'w', b'o']); // "two"

        let table = out.len();
        for offset in &offsets {
            out.push(*offset as u8);
        }
        out.extend_from_slice(&[0; 6]);
        out.push(1); // offset width
        out.push(1); // reference width
        out.extend_from_slice(&(offsets.len() as u64).to_be_bytes());
        out.extend_from_slice(&0u64.to_be_bytes()); // root
        out.extend_from_slice(&(table as u64).to_be_bytes());

        let value = parse(&out).expect("a binary plist");
        assert_eq!(value.get("a"), Some(&Value::Integer(1)));
        assert_eq!(value.get("b").and_then(Value::as_str), Some("two"));
    }

    #[test]
    fn a_truncated_plist_is_an_error_rather_than_a_panic() {
        assert!(parse(b"bplist00").is_err());
        assert!(parse(b"bplist00\x00\x00\x00\x00\x00\x00\x00\x00").is_err());
        // A trailer promising an offset table past the end.
        let mut bytes = vec![0u8; 64];
        bytes[..8].copy_from_slice(b"bplist00");
        bytes[64 - 32 + 6] = 8;
        bytes[64 - 32 + 7] = 8;
        bytes[64 - 32 + 15] = 0xff;
        assert!(parse(&bytes).is_err());
    }
}
