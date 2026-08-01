//! A bounds-checked cursor over the raw Mach-O bytes. Every read returns a
//! [`Result`] so malformed or truncated inputs surface as errors rather than
//! panics — the input is an untrusted third-party binary.
//!
//! Ported from `selfish/src/raw/view.rs:10-52`, dropping the `Endian` type
//! parameter: every integer here is little-endian.

use crate::error::{Error, Result};

/// An immutable, bounds-checked view over a Mach-O file's bytes.
#[derive(Copy, Clone)]
pub struct View<'a> {
    bytes: &'a [u8],
}

impl<'a> View<'a> {
    pub fn new(bytes: &'a [u8]) -> Self {
        View { bytes }
    }

    pub fn bytes(&self) -> &'a [u8] {
        self.bytes
    }

    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// Sub-slice `[off, off+len)`, or `Truncated` if it would run past the end.
    pub fn slice(&self, off: usize, len: usize, what: &'static str) -> Result<&'a [u8]> {
        self.bytes
            .get(off..off.checked_add(len).ok_or(Error::Truncated { what })?)
            .ok_or(Error::Truncated { what })
    }

    /// A sub-view of `[off, off+len)`, for parsing a nested structure without
    /// letting it read past its own extent.
    pub fn subview(&self, off: usize, len: usize, what: &'static str) -> Result<View<'a>> {
        Ok(View::new(self.slice(off, len, what)?))
    }

    pub fn u8(&self, off: usize) -> Result<u8> {
        self.bytes
            .get(off)
            .copied()
            .ok_or(Error::Truncated { what: "u8" })
    }

    pub fn u16(&self, off: usize) -> Result<u16> {
        let b = self.slice(off, 2, "u16")?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    pub fn u32(&self, off: usize) -> Result<u32> {
        let b = self.slice(off, 4, "u32")?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    pub fn i32(&self, off: usize) -> Result<i32> {
        self.u32(off).map(|v| v as i32)
    }

    pub fn u64(&self, off: usize) -> Result<u64> {
        let b = self.slice(off, 8, "u64")?;
        Ok(u64::from_le_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }

    /// Big-endian `u32`. Needed for exactly one structure: the fat/universal
    /// header, which is big-endian even on little-endian targets.
    pub fn u32_be(&self, off: usize) -> Result<u32> {
        let b = self.slice(off, 4, "u32_be")?;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }

    /// Big-endian `u64`, for `fat_arch_64`.
    pub fn u64_be(&self, off: usize) -> Result<u64> {
        let b = self.slice(off, 8, "u64_be")?;
        Ok(u64::from_be_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }

    /// A NUL-terminated UTF-8 string starting at `off`, borrowed from the file.
    pub fn cstr(&self, off: usize) -> Result<&'a str> {
        let rest = self
            .bytes
            .get(off..)
            .ok_or(Error::Truncated { what: "cstr" })?;
        let end = rest.iter().position(|&b| b == 0).ok_or(Error::BadString)?;
        core::str::from_utf8(&rest[..end]).map_err(|_| Error::BadString)
    }

    /// A fixed-width character array, NUL-padded but *not* necessarily
    /// NUL-terminated. This is how `segname` and `sectname` are stored: 16
    /// bytes that may be exactly filled, so [`cstr`](Self::cstr) would run into
    /// the following field.
    pub fn fixed_str(&self, off: usize, len: usize) -> Result<&'a str> {
        let b = self.slice(off, len, "fixed_str")?;
        let end = b.iter().position(|&x| x == 0).unwrap_or(len);
        core::str::from_utf8(&b[..end]).map_err(|_| Error::BadString)
    }
}
