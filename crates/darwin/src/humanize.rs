//! Sizes for people: libutil's `humanize_number` and `expand_number`, and
//! libc's `getbsize`.
//!
//! All BSD's, and none in glibc. Ported from Apple's `libutil/
//! humanize_number.c`, `libutil/expand_number.c` and `gen/FreeBSD/getbsize.c`;
//! `ls -h`, `ls -s` and `du -t` are the callers that turn up.

#![cfg(not(target_os = "macos"))]

use std::ffi::{CStr, c_char, c_int, c_long};

const HN_DECIMAL: c_int = 0x01;
const HN_NOSPACE: c_int = 0x02;
const HN_B: c_int = 0x04;
const HN_DIVISOR_1000: c_int = 0x08;
const HN_IEC_PREFIXES: c_int = 0x10;
const HN_GETSCALE: c_int = 0x10;
const HN_AUTOSCALE: c_int = 0x20;

const MAXSCALE: c_int = 7;

/// What `humanize_number` writes, or the integer it returns instead.
#[derive(Debug, PartialEq, Eq)]
enum Humanized {
    Text(String),
    Scale(c_int),
    Invalid,
}

/// The locale's decimal point, as `localeconv` reports it.
fn decimal_point() -> String {
    // SAFETY: `localeconv` returns libc's own structure, whose
    // `decimal_point` is a NUL-terminated string.
    unsafe {
        let conv = libc::localeconv();
        if conv.is_null() || (*conv).decimal_point.is_null() {
            return ".".to_string();
        }
        CStr::from_ptr((*conv).decimal_point)
            .to_string_lossy()
            .into_owned()
    }
}

fn humanize(len: usize, quotient: i64, suffix: &str, scale: c_int, flags: c_int) -> Humanized {
    if scale < 0 {
        return Humanized::Invalid;
    }
    if scale >= MAXSCALE && (scale & !(HN_AUTOSCALE | HN_GETSCALE)) != 0 {
        return Humanized::Invalid;
    }
    if flags & HN_DIVISOR_1000 != 0 && flags & HN_IEC_PREFIXES != 0 {
        return Humanized::Invalid;
    }

    let (mut baselen, divisor, divisordeccut, prefixes): (usize, i64, i64, [&str; 7]) =
        if flags & HN_IEC_PREFIXES != 0 {
            let first = if flags & HN_B != 0 { "B" } else { "" };
            (2, 1024, 973, [first, "Ki", "Mi", "Gi", "Ti", "Pi", "Ei"])
        } else if flags & HN_DIVISOR_1000 != 0 {
            let first = if flags & HN_B != 0 { "B" } else { "" };
            (1, 1000, 950, [first, "k", "M", "G", "T", "P", "E"])
        } else {
            let first = if flags & HN_B != 0 { "B" } else { "" };
            (1, 1024, 973, [first, "K", "M", "G", "T", "P", "E"])
        };

    let mut quotient = quotient;
    let sign: i64 = if quotient < 0 {
        quotient = quotient.wrapping_neg();
        baselen += 2;
        -1
    } else {
        baselen += 1;
        1
    };
    let sep = if flags & HN_NOSPACE != 0 {
        ""
    } else {
        baselen += 1;
        " "
    };
    baselen += suffix.len();
    if len < baselen + 1 {
        return Humanized::Invalid;
    }

    let mut remainder: i64 = 0;
    let mut i: c_int = 0;
    if scale & (HN_AUTOSCALE | HN_GETSCALE) != 0 {
        // The largest number the spare columns can show.
        let mut max: i64 = 1;
        let mut spare = len - baselen;
        while spare > 0 && max <= i64::MAX / 10 {
            max *= 10;
            spare -= 1;
        }
        // Divide until it fits, and once more if rounding would overflow.
        while (quotient >= max || (quotient == max - 1 && remainder >= divisordeccut))
            && i < MAXSCALE
        {
            remainder = quotient % divisor;
            quotient /= divisor;
            i += 1;
        }
        if scale & HN_GETSCALE != 0 {
            return Humanized::Scale(i);
        }
    } else {
        while i < scale && i < MAXSCALE {
            remainder = quotient % divisor;
            quotient /= divisor;
            i += 1;
        }
    }

    let prefix = prefixes[i as usize];
    let text = if ((quotient == 9 && remainder < divisordeccut) || quotient < 9)
        && i > 0
        && flags & HN_DECIMAL != 0
    {
        let s1 = quotient + ((remainder * 10 + divisor / 2) / divisor / 10);
        let s2 = ((remainder * 10 + divisor / 2) / divisor) % 10;
        format!(
            "{}{}{}{sep}{prefix}{suffix}",
            sign * s1,
            decimal_point(),
            s2
        )
    } else {
        format!(
            "{}{sep}{prefix}{suffix}",
            sign * (quotient + (remainder + divisor / 2) / divisor)
        )
    };
    Humanized::Text(text)
}

/// `int humanize_number(char *buf, size_t len, int64_t number,
/// const char *suffix, int scale, int flags)`
///
/// # Safety
///
/// `buf` is writable for `len` bytes and `suffix` is NUL-terminated.
pub unsafe extern "C" fn humanize_number(
    buf: *mut c_char,
    len: usize,
    number: i64,
    suffix: *const c_char,
    scale: c_int,
    flags: c_int,
) -> c_int {
    // Most callers do not check for -1, so the buffer is terminated first.
    if len > 0 && !buf.is_null() {
        // SAFETY: at least one writable byte.
        unsafe { *buf = 0 };
    }
    if buf.is_null() || suffix.is_null() {
        return -1;
    }
    // SAFETY: the caller's obligation.
    let suffix = unsafe { CStr::from_ptr(suffix) }.to_string_lossy();
    match humanize(len, number, &suffix, scale, flags) {
        Humanized::Invalid => -1,
        Humanized::Scale(i) => i,
        Humanized::Text(text) => {
            // `snprintf`: as much as fits, always terminated, and the full
            // length returned.
            let n = text.len().min(len - 1);
            // SAFETY: `n + 1 <= len` bytes of the caller's buffer.
            unsafe {
                std::ptr::copy_nonoverlapping(text.as_ptr().cast(), buf, n);
                *buf.add(n) = 0;
            }
            text.len() as c_int
        }
    }
}

/// `expand_number`'s answer, or the errno it fails with.
fn expand(buf: &CStr) -> Result<u64, c_int> {
    let mut end: *mut c_char = std::ptr::null_mut();
    // SAFETY: a NUL-terminated string and a live out-pointer. `strtoimax` with
    // base 0, as Apple's does, so `0x10k` is a size too.
    let number = unsafe { libc::strtoll(buf.as_ptr(), &mut end, 0) };
    if std::ptr::eq(end, buf.as_ptr()) {
        return Err(libc::EINVAL);
    }
    // SAFETY: `strtoll` leaves `end` inside the string, at worst on its NUL.
    let unit = unsafe { *end } as u8;
    if unit == 0 {
        return Ok(number as u64);
    }
    // Each step past `b` is a shift by ten; only the first unit character
    // counts, so `10kb` is ten kilobytes.
    let Some(shifts) = b"bkmgtpe"
        .iter()
        .position(|u| *u == unit.to_ascii_lowercase())
    else {
        return Err(libc::EINVAL);
    };
    let mut number = number;
    for _ in 0..shifts {
        let shifted = number.wrapping_shl(10);
        if (number < 0 && shifted > number) || (number >= 0 && shifted < number) {
            return Err(libc::ERANGE);
        }
        number = shifted;
    }
    Ok(number as u64)
}

/// `int expand_number(const char *buf, uint64_t *num)`
///
/// # Safety
///
/// `buf` is NUL-terminated and `num` is writable.
pub unsafe extern "C" fn expand_number(buf: *const c_char, num: *mut u64) -> c_int {
    if buf.is_null() {
        crate::errno::set(libc::EINVAL);
        return -1;
    }
    // SAFETY: the caller's obligation.
    match expand(unsafe { CStr::from_ptr(buf) }) {
        Ok(n) => {
            // SAFETY: the caller's obligation.
            unsafe { *num = n };
            0
        }
        Err(errno) => {
            crate::errno::set(errno);
            -1
        }
    }
}

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
        "humanize_number" => humanize_number,
        "expand_number" => expand_number,
        "getbsize" => getbsize,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(len: usize, n: i64, suffix: &str, scale: c_int, flags: c_int) -> String {
        match humanize(len, n, suffix, scale, flags) {
            Humanized::Text(t) => t,
            other => panic!("{other:?}"),
        }
    }

    /// `ls -lh`'s call: five columns, autoscaled, decimal, no space, bytes.
    #[test]
    fn ls_h_sizes() {
        let ls = |n| text(5, n, "", HN_AUTOSCALE, HN_B | HN_NOSPACE | HN_DECIMAL);
        assert_eq!(ls(0), "0B");
        assert_eq!(ls(512), "512B");
        assert_eq!(ls(1023), "1.0K");
        assert_eq!(ls(1536), "1.5K");
        assert_eq!(ls(10 * 1024), "10K");
        assert_eq!(ls(3 * 1024 * 1024 + 300 * 1024), "3.3M");
    }

    #[test]
    fn getscale_reports_the_divisions_it_would_make() {
        assert_eq!(
            humanize(5, 5_000_000, "", HN_GETSCALE, 0),
            Humanized::Scale(2)
        );
    }

    #[test]
    fn a_buffer_too_small_for_one_digit_is_refused() {
        assert_eq!(humanize(2, 5, "B", 0, 0), Humanized::Invalid);
    }

    #[test]
    fn expand_number_reads_a_unit_suffix() {
        assert_eq!(expand(c"512"), Ok(512));
        assert_eq!(expand(c"10k"), Ok(10 * 1024));
        assert_eq!(expand(c"10KB"), Ok(10 * 1024));
        assert_eq!(expand(c"3M"), Ok(3 << 20));
        assert_eq!(expand(c"0x10g"), Ok(16 << 30));
        assert_eq!(expand(c"7b"), Ok(7));
        assert_eq!(expand(c"k"), Err(libc::EINVAL));
        assert_eq!(expand(c"5q"), Err(libc::EINVAL));
        assert_eq!(expand(c"9e"), Err(libc::ERANGE));
    }

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
