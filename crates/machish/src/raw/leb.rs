//! LEB128 decoding, and the sequential [`Cursor`] built on it.
//!
//! All three Mach-O fixup encodings are LEB-soaked: the `LC_DYLD_INFO`
//! rebase/bind opcode streams take their operands as ULEB/SLEB, the export trie
//! encodes every node field as ULEB, and `LC_DYLD_CHAINED_FIXUPS` uses ULEB for
//! import addends. So decoding is a shared primitive rather than a detail of any
//! one decoder.
//!
//! Both decoders reject over-long and overflowing encodings rather than
//! truncating silently. That strictness matters because the input is an
//! untrusted binary and a runaway LEB is a cheap way to spin a decoder forever.

use crate::error::{Error, Result};
use crate::raw::View;

/// The most bytes a well-formed 64-bit LEB128 can occupy: nine 7-bit groups
/// plus a tenth carrying the top bit.
const MAX_LEB_BYTES: u32 = 10;

/// Decode an unsigned LEB128 at `*pos`, advancing `pos` past it.
pub fn uleb128(view: &View<'_>, pos: &mut usize) -> Result<u64> {
    let mut result: u64 = 0;
    let mut shift: u32 = 0;
    loop {
        let byte = view.u8(*pos).map_err(|_| Error::BadLeb)?;
        *pos += 1;
        let payload = u64::from(byte & 0x7f);

        if shift >= 64 {
            // Past bit 63 the only legal payload is zero; anything else is a
            // value that does not fit in the u64 we promised to return.
            if payload != 0 {
                return Err(Error::BadLeb);
            }
        } else {
            // Round-trip the shift to detect bits falling off the top.
            if (payload << shift) >> shift != payload {
                return Err(Error::BadLeb);
            }
            result |= payload << shift;
        }

        if byte & 0x80 == 0 {
            return Ok(result);
        }
        shift += 7;
        if shift >= MAX_LEB_BYTES * 7 {
            return Err(Error::BadLeb);
        }
    }
}

/// Decode a signed LEB128 at `*pos`, advancing `pos` past it.
pub fn sleb128(view: &View<'_>, pos: &mut usize) -> Result<i64> {
    let mut result: i64 = 0;
    let mut shift: u32 = 0;
    let mut byte;
    loop {
        byte = view.u8(*pos).map_err(|_| Error::BadLeb)?;
        *pos += 1;

        if shift >= 63 {
            // At shift 63 only bit 0 of the payload lands inside the i64; the
            // remaining six bits fall off the top. The only encodings where
            // that is lossless are a pure sign extension of bit 63, so a tenth
            // byte must be all-zero or all-one payload. (Note this is where the
            // check has to live: a well-formed 64-bit SLEB is at most ten bytes,
            // so `shift` reaches 63 but never 64.)
            let payload = byte & 0x7f;
            if payload != 0x00 && payload != 0x7f {
                return Err(Error::BadLeb);
            }
            result |= i64::from(payload & 1) << 63;
        } else {
            result |= i64::from(byte & 0x7f) << shift;
        }

        shift += 7;
        if byte & 0x80 == 0 {
            break;
        }
        if shift >= MAX_LEB_BYTES * 7 {
            return Err(Error::BadLeb);
        }
    }

    // Sign-extend from the last payload bit if the value is negative.
    if shift < 64 && byte & 0x40 != 0 {
        result |= -1i64 << shift;
    }
    Ok(result)
}

/// A sequential reader over a [`View`], tracking its own position.
///
/// The opcode VMs and the trie walker both consume their input strictly
/// forwards, so threading a `&mut usize` through every call would be noise.
#[derive(Copy, Clone)]
pub struct Cursor<'a> {
    view: View<'a>,
    pos: usize,
}

impl<'a> Cursor<'a> {
    pub fn new(view: View<'a>) -> Self {
        Cursor { view, pos: 0 }
    }

    /// A cursor starting at `pos` within `view`.
    pub fn at(view: View<'a>, pos: usize) -> Self {
        Cursor { view, pos }
    }

    pub fn view(&self) -> &View<'a> {
        &self.view
    }

    pub fn pos(&self) -> usize {
        self.pos
    }

    pub fn set_pos(&mut self, pos: usize) {
        self.pos = pos;
    }

    /// Whether the cursor has consumed the whole view.
    pub fn at_end(&self) -> bool {
        self.pos >= self.view.len()
    }

    pub fn u8(&mut self) -> Result<u8> {
        let v = self.view.u8(self.pos)?;
        self.pos += 1;
        Ok(v)
    }

    pub fn uleb128(&mut self) -> Result<u64> {
        uleb128(&self.view, &mut self.pos)
    }

    pub fn sleb128(&mut self) -> Result<i64> {
        sleb128(&self.view, &mut self.pos)
    }

    /// A NUL-terminated string at the cursor, advancing past the terminator.
    pub fn cstr(&mut self) -> Result<&'a str> {
        let s = self.view.cstr(self.pos)?;
        self.pos += s.len() + 1;
        Ok(s)
    }

    /// Advance by `n` bytes, erroring if that would leave the view.
    pub fn skip(&mut self, n: usize) -> Result<()> {
        let next = self.pos.checked_add(n).ok_or(Error::Truncated { what: "skip" })?;
        if next > self.view.len() {
            return Err(Error::Truncated { what: "skip" });
        }
        self.pos = next;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Error;

    fn uleb(bytes: &[u8]) -> Result<u64> {
        let view = View::new(bytes);
        let mut pos = 0;
        uleb128(&view, &mut pos)
    }

    fn sleb(bytes: &[u8]) -> Result<i64> {
        let view = View::new(bytes);
        let mut pos = 0;
        sleb128(&view, &mut pos)
    }

    #[test]
    fn uleb_single_byte() {
        assert_eq!(uleb(&[0x00]).unwrap(), 0);
        assert_eq!(uleb(&[0x01]).unwrap(), 1);
        assert_eq!(uleb(&[0x7f]).unwrap(), 127);
    }

    #[test]
    fn uleb_multi_byte() {
        assert_eq!(uleb(&[0x80, 0x01]).unwrap(), 128);
        assert_eq!(uleb(&[0xe5, 0x8e, 0x26]).unwrap(), 624485);
    }

    #[test]
    fn uleb_max_u64() {
        // Ten bytes: nine 0x7f payloads plus a final 0x01 for bit 63.
        let bytes = [0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01];
        assert_eq!(uleb(&bytes).unwrap(), u64::MAX);
    }

    #[test]
    fn uleb_rejects_overflow() {
        // Bit 64 set in the tenth byte: does not fit a u64.
        let bytes = [0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x02];
        assert_eq!(uleb(&bytes), Err(Error::BadLeb));
    }

    #[test]
    fn uleb_rejects_unterminated() {
        // Continuation bit set on every byte, never terminating.
        assert_eq!(uleb(&[0x80, 0x80, 0x80]), Err(Error::BadLeb));
        let runaway = [0x80u8; 16];
        assert_eq!(uleb(&runaway), Err(Error::BadLeb));
    }

    #[test]
    fn uleb_rejects_empty() {
        assert_eq!(uleb(&[]), Err(Error::BadLeb));
    }

    #[test]
    fn uleb_padded_encoding_is_accepted() {
        // Non-canonical but well-formed: 1 encoded in three bytes.
        assert_eq!(uleb(&[0x81, 0x80, 0x00]).unwrap(), 1);
    }

    #[test]
    fn sleb_single_byte() {
        assert_eq!(sleb(&[0x00]).unwrap(), 0);
        assert_eq!(sleb(&[0x01]).unwrap(), 1);
        assert_eq!(sleb(&[0x3f]).unwrap(), 63);
        assert_eq!(sleb(&[0x40]).unwrap(), -64);
        assert_eq!(sleb(&[0x7f]).unwrap(), -1);
    }

    #[test]
    fn sleb_multi_byte() {
        assert_eq!(sleb(&[0x80, 0x01]).unwrap(), 128);
        assert_eq!(sleb(&[0x80, 0x7f]).unwrap(), -128);
        assert_eq!(sleb(&[0xc0, 0xbb, 0x78]).unwrap(), -123456);
    }

    #[test]
    fn sleb_extremes() {
        let min = [0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x7f];
        assert_eq!(sleb(&min).unwrap(), i64::MIN);
        let max = [0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00];
        assert_eq!(sleb(&max).unwrap(), i64::MAX);
    }

    #[test]
    fn sleb_rejects_bad_extension() {
        // A tenth byte that is neither all-zero nor all-one payload.
        let bytes = [0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x40];
        assert_eq!(sleb(&bytes), Err(Error::BadLeb));
    }

    #[test]
    fn sleb_rejects_unterminated() {
        let runaway = [0x80u8; 16];
        assert_eq!(sleb(&runaway), Err(Error::BadLeb));
    }

    #[test]
    fn cursor_advances_across_mixed_reads() {
        let bytes = [0x2a, 0xe5, 0x8e, 0x26, b'h', b'i', 0x00, 0x7f];
        let mut c = Cursor::new(View::new(&bytes));
        assert_eq!(c.u8().unwrap(), 0x2a);
        assert_eq!(c.uleb128().unwrap(), 624485);
        assert_eq!(c.cstr().unwrap(), "hi");
        assert_eq!(c.pos(), 7);
        assert_eq!(c.sleb128().unwrap(), -1);
        assert!(c.at_end());
    }

    #[test]
    fn cursor_skip_is_bounds_checked() {
        let bytes = [0u8; 4];
        let mut c = Cursor::new(View::new(&bytes));
        assert!(c.skip(4).is_ok());
        assert!(c.skip(1).is_err());
        assert!(c.skip(usize::MAX).is_err());
    }

    #[test]
    fn cursor_cstr_requires_terminator() {
        let bytes = *b"unterminated";
        let mut c = Cursor::new(View::new(&bytes));
        assert_eq!(c.cstr(), Err(Error::BadString));
    }
}
