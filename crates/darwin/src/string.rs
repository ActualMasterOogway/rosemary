//! The `<string.h>` functions Darwin has and this host does not.
//!
//! Everything in `<string.h>` that both sides define is a genuine passthrough
//! and is claimed as one in [`crate::passthrough`]. What is left is Apple's own
//! `memset_pattern4`/`8`/`16`, which fill a buffer by repeating a fixed-width
//! pattern - `memset` with a wider unit. glibc has no equivalent under any name,
//! so an image that calls one binds to nothing and jumps to zero.
//!
//! Not niche, either: it is what a C++ compiler on Darwin emits for
//! `std::fill` over a 4-, 8- or 16-byte type, so any program that initialises an
//! array of doubles or of pointers has one.

#![cfg(not(target_os = "macos"))]

use std::ffi::c_void;

use machish::resolve::SyntheticDylib;

/// Fill `len` bytes at `b` by repeating the `n`-byte pattern at `pattern`.
///
/// A trailing partial copy is written, which is what Apple's does: the length is
/// in bytes and is not required to be a multiple of the pattern.
///
/// # Safety
///
/// `b` is `len` writable bytes and `pattern` is `n` readable ones; the two do
/// not overlap.
unsafe fn fill(b: *mut c_void, pattern: *const c_void, n: usize, len: usize) {
    if b.is_null() || pattern.is_null() || len == 0 {
        return;
    }
    let dst = b.cast::<u8>();
    let src = pattern.cast::<u8>();
    let mut written = 0;
    while written < len {
        let step = n.min(len - written);
        // SAFETY: `written + step <= len`, so the destination is inside the
        // caller's buffer, and `step <= n`, so the source is inside the pattern.
        // The caller states they do not overlap.
        unsafe { std::ptr::copy_nonoverlapping(src, dst.add(written), step) };
        written += step;
    }
}

/// `void memset_pattern4(void *b, const void *pattern4, size_t len)`
///
/// # Safety
///
/// As [`fill`], with a four-byte pattern.
pub unsafe extern "C" fn memset_pattern4(b: *mut c_void, pattern: *const c_void, len: usize) {
    // SAFETY: the caller's own buffer and a four-byte pattern.
    unsafe { fill(b, pattern, 4, len) };
}

/// `void memset_pattern8(void *b, const void *pattern8, size_t len)`
///
/// # Safety
///
/// As [`fill`], with an eight-byte pattern.
pub unsafe extern "C" fn memset_pattern8(b: *mut c_void, pattern: *const c_void, len: usize) {
    // SAFETY: the caller's own buffer and an eight-byte pattern.
    unsafe { fill(b, pattern, 8, len) };
}

/// `void memset_pattern16(void *b, const void *pattern16, size_t len)`
///
/// # Safety
///
/// As [`fill`], with a sixteen-byte pattern.
pub unsafe extern "C" fn memset_pattern16(b: *mut c_void, pattern: *const c_void, len: usize) {
    // SAFETY: the caller's own buffer and a sixteen-byte pattern.
    unsafe { fill(b, pattern, 16, len) };
}

pub const NAMES: &[&str] = &["memset_pattern4", "memset_pattern8", "memset_pattern16"];

pub fn symbol(name: &str) -> Option<u64> {
    let f: *const () = match name {
        "memset_pattern4" => memset_pattern4 as *const (),
        "memset_pattern8" => memset_pattern8 as *const (),
        "memset_pattern16" => memset_pattern16 as *const (),
        _ => return None,
    };
    Some(f as usize as u64)
}

pub fn provide(lib: &mut SyntheticDylib) {
    for name in NAMES {
        crate::define_fn(
            lib,
            &format!("_{name}"),
            symbol(name).unwrap_or(0) as *const (),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filled(n: usize, pattern: &[u8], len: usize) -> Vec<u8> {
        let mut buf = vec![0xAAu8; len + 8];
        // SAFETY: `buf` has `len` writable bytes at its start and `pattern` has
        // `n`; they are separate allocations.
        unsafe { fill(buf.as_mut_ptr().cast(), pattern.as_ptr().cast(), n, len) };
        buf
    }

    #[test]
    fn the_pattern_repeats_across_the_buffer() {
        let out = filled(4, b"abcd", 12);
        assert_eq!(&out[..12], b"abcdabcdabcd");
        assert!(out[12..].iter().all(|b| *b == 0xAA), "wrote past `len`");
    }

    /// The length is in bytes and need not be a multiple of the pattern; the
    /// last copy is cut short rather than rounded up.
    #[test]
    fn a_partial_final_copy_is_written_and_nothing_beyond_it() {
        let out = filled(16, b"0123456789abcdef", 20);
        assert_eq!(&out[..20], b"0123456789abcdef0123");
        assert!(out[20..].iter().all(|b| *b == 0xAA), "wrote past `len`");
    }

    #[test]
    fn a_zero_length_writes_nothing() {
        let out = filled(8, b"01234567", 0);
        assert!(out.iter().all(|b| *b == 0xAA));
    }

    /// The published entry points, through the addresses a guest is handed.
    #[test]
    fn the_exported_entry_points_are_the_ones_they_claim_to_be() {
        for (name, n) in [
            ("memset_pattern4", 4usize),
            ("memset_pattern8", 8),
            ("memset_pattern16", 16),
        ] {
            let addr = symbol(name).expect(name);
            // SAFETY: each of the three has this prototype.
            let f: unsafe extern "C" fn(*mut c_void, *const c_void, usize) =
                unsafe { std::mem::transmute(addr as usize as *const ()) };
            let pattern = vec![b'z'; n];
            let mut buf = vec![0u8; n * 3];
            // SAFETY: `buf` is `n * 3` writable bytes and `pattern` is `n`.
            unsafe { f(buf.as_mut_ptr().cast(), pattern.as_ptr().cast(), buf.len()) };
            assert!(buf.iter().all(|b| *b == b'z'), "{name}");
        }
    }
}
