#![cfg(not(target_os = "macos"))]

use std::ffi::{CStr, c_char, c_int, c_long};

unsafe extern "C" {
    fn warnx(format: *const c_char, ...);
}

/// Report a `getbsize` complaint the way the guest's own `warnx` would.
fn complain(message: &str) {
    let Ok(message) = std::ffi::CString::new(message) else {
        return;
    };
    // SAFETY: a literal format and one NUL-terminated argument for it.
    unsafe { warnx(c"%s".as_ptr(), message.as_ptr()) };
}

/// The header and block size `BLOCKSIZE` asks for.
fn blocksize(value: Option<&[u8]>) -> (String, c_long) {
    const KB: c_long = 1024;
    const MB: c_long = 1024 * 1024;
    const GB: c_long = 1024 * 1024 * 1024;
    const MAXB: c_long = GB;

    let Some(p) = value.filter(|p| !p.is_empty()) else {
        return ("512-blocks".to_string(), 512);
    };
    let text = String::from_utf8_lossy(p);
    let underflow = || {
        complain("minimum blocksize is 512");
        ("512-blocks".to_string(), 512)
    };

    // `strtol(p, &ep, 10)`.
    let trimmed = text.trim_start();
    let (negative, digits_from) = match trimmed.as_bytes().first() {
        Some(b'-') => (true, 1),
        Some(b'+') => (false, 1),
        _ => (false, 0),
    };
    let rest = &trimmed[digits_from..];
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    let (mut n, ep) = match digits {
        0 => (0, text.as_ref()),
        _ => {
            let n = rest[..digits].parse::<c_long>().unwrap_or(c_long::MAX);
            (if negative { -n } else { n }, &rest[digits..])
        }
    };
    if n < 0 {
        return underflow();
    }
    if n == 0 {
        n = 1;
    }
    let ep = ep.as_bytes();
    let (form, max, mul) = match (ep.first(), ep.len() > 1) {
        (Some(b'G' | b'g'), false) => ("G", MAXB / GB, GB),
        (Some(b'K' | b'k'), false) => ("K", MAXB / KB, KB),
        (Some(b'M' | b'm'), false) => ("M", MAXB / MB, MB),
        (None, _) => ("", MAXB, 1),
        _ => {
            complain(&format!("{text}: unknown blocksize"));
            n = 512;
            ("", MAXB, 1)
        }
    };
    if n > max {
        complain(&format!("maximum blocksize is {}G", MAXB / GB));
        n = max;
    }
    let size = n * mul;
    if size < 512 {
        return underflow();
    }
    (format!("{n}{form}-blocks"), size)
}

/// `char *getbsize(int *headerlenp, long *blocksizep)`
///
/// # Safety
///
/// Both pointers are writable.
pub unsafe extern "C" fn getbsize(headerlenp: *mut c_int, blocksizep: *mut c_long) -> *mut c_char {
    /// Apple's `static char header[20]`, shared by every caller as theirs is.
    static mut HEADER: [c_char; 20] = [0; 20];

    // The guest's environment, which is where its `BLOCKSIZE` is.
    // SAFETY: a literal name; the answer is null or the guest's own string.
    let value = unsafe {
        let p = crate::env::getenv(c"BLOCKSIZE".as_ptr());
        (!p.is_null()).then(|| CStr::from_ptr(p).to_bytes().to_vec())
    };
    let (header, size) = blocksize(value.as_deref());
    let n = header.len().min(19);
    // SAFETY: the static is only written here, and the pointers are the
    // caller's obligation.
    unsafe {
        let out = (&raw mut HEADER).cast::<c_char>();
        std::ptr::copy_nonoverlapping(header.as_ptr().cast(), out, n);
        *out.add(n) = 0;
        *headerlenp = n as c_int;
        *blocksizep = size;
        out
    }
}

exports::exports! {
    functions {
        "getbsize" => getbsize,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocksize_follows_the_environment() {
        assert_eq!(blocksize(None), ("512-blocks".to_string(), 512));
        assert_eq!(blocksize(Some(b"1k")), ("1K-blocks".to_string(), 1024));
        assert_eq!(blocksize(Some(b"4096")), ("4096-blocks".to_string(), 4096));
        assert_eq!(
            blocksize(Some(b"1M")),
            ("1M-blocks".to_string(), 1024 * 1024)
        );
        assert_eq!(blocksize(Some(b"100")), ("512-blocks".to_string(), 512));
    }
}
