use std::ffi::{CStr, c_char, c_int, c_long};

unsafe extern "C" {
    fn warnx(format: *const c_char, ...);
}

/// Report a `getbsize` complaint the way the guest's own `warnx` would.
fn complain(message: &[u8]) {
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
    let underflow = || {
        complain(b"minimum blocksize is 512");
        ("512-blocks".to_string(), 512)
    };

    let text = std::ffi::CString::new(p).expect("environment value has no NUL");
    let mut end = std::ptr::null_mut();
    // SAFETY: a live C string and writable end pointer. Using C's parser
    // preserves its byte/locale rules and ERANGE on signed long overflow.
    let mut n = unsafe { libc::strtol(text.as_ptr(), &mut end, 10) };
    // SAFETY: strtol leaves end pointing within text, including its NUL.
    let ep = unsafe { CStr::from_ptr(end) }.to_bytes();
    if n < 0 {
        return underflow();
    }
    if n == 0 {
        n = 1;
    }
    let (form, max, mul) = match (ep.first(), ep.len() > 1) {
        (Some(b'G' | b'g'), false) => ("G", MAXB / GB, GB),
        (Some(b'K' | b'k'), false) => ("K", MAXB / KB, KB),
        (Some(b'M' | b'm'), false) => ("M", MAXB / MB, MB),
        (None, _) => ("", MAXB, 1),
        _ => {
            complain(&[p, b": unknown blocksize"].concat());
            n = 512;
            ("", MAXB, 1)
        }
    };
    if n > max {
        complain(b"maximum blocksize is 1G");
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
