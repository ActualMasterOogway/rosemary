//! Wide-character I/O: multibyte sequences in the stream, converted in the
//! current locale with the stream's own conversion state.
//!
//! Darwin's `wchar_t` and `wint_t` are both 32-bit `int`, which is also every
//! host's `wchar_t` width, so the host's converters are used directly.

use std::ffi::{c_int, c_void};

use super::backend::{mbrtowc, wcrtomb};
use super::{File, Locked, STDINP, STDOUTP, engine, flag, state};

/// Darwin's `WEOF`.
pub const WEOF: c_int = -1;

/// Longer than any host's `MB_LEN_MAX`.
const MB_MAX: usize = 16;

/// Lock `fp` and mark it wide oriented, for the length of the guard.
unsafe fn wide(fp: *mut File) -> Locked {
    // SAFETY: the caller's live stream.
    unsafe {
        let guard = Locked::new(fp);
        engine::orient(fp, 1);
        guard
    }
}

/// The stream's conversion state, as the host's converters take it.
unsafe fn mbstate(fp: *mut File) -> *mut c_void {
    // SAFETY: the caller's locked stream.
    unsafe { (&raw mut state(fp).mbstate).cast() }
}

/// `fgetwc` with the lock already held.
unsafe fn getwc_locked(fp: *mut File) -> c_int {
    // SAFETY: the caller's locked stream.
    unsafe {
        loop {
            if (*fp).r <= 0 && !engine::refill(fp) {
                return WEOF;
            }
            let mut wc: c_int = 0;
            let used = mbrtowc(&mut wc, (*fp).p.cast(), (*fp).r as usize, mbstate(fp));
            match used {
                usize::MAX => {
                    (*fp).flags |= flag::ERR;
                    return WEOF;
                }
                // Incomplete: the bytes are in the state; read more.
                x if x == usize::MAX - 1 => {
                    (*fp).p = (*fp).p.add((*fp).r as usize);
                    (*fp).r = 0;
                }
                n => {
                    let n = n.max(1);
                    (*fp).p = (*fp).p.add(n);
                    (*fp).r -= n as c_int;
                    return wc;
                }
            }
        }
    }
}

/// `fputwc` with the lock already held.
unsafe fn putwc_locked(wc: c_int, fp: *mut File) -> c_int {
    let mut buf = [0u8; MB_MAX];
    // SAFETY: the caller's locked stream and a buffer longer than any
    // multibyte character.
    unsafe {
        let n = wcrtomb(buf.as_mut_ptr().cast(), wc, mbstate(fp));
        if n == usize::MAX {
            (*fp).flags |= flag::ERR;
            return WEOF;
        }
        match engine::write_all(fp, buf.as_ptr(), n) == n {
            true => wc,
            false => WEOF,
        }
    }
}

/// `wint_t fgetwc(FILE *stream)`
///
/// # Safety
///
/// `stream` is open.
pub unsafe extern "C" fn fgetwc(fp: *mut File) -> c_int {
    // SAFETY: the caller's stream.
    unsafe {
        let _guard = wide(fp);
        getwc_locked(fp)
    }
}

/// `wint_t getwc(FILE *stream)`
///
/// # Safety
///
/// As [`fgetwc`].
pub unsafe extern "C" fn getwc(fp: *mut File) -> c_int {
    // SAFETY: the caller's stream.
    unsafe { fgetwc(fp) }
}

/// `wint_t getwchar(void)`
pub unsafe extern "C" fn getwchar() -> c_int {
    // SAFETY: the guest's `stdin`.
    unsafe { fgetwc(STDINP.get()) }
}

/// `wint_t fputwc(wchar_t wc, FILE *stream)`
///
/// # Safety
///
/// `stream` is open.
pub unsafe extern "C" fn fputwc(wc: c_int, fp: *mut File) -> c_int {
    // SAFETY: the caller's stream.
    unsafe {
        let _guard = wide(fp);
        putwc_locked(wc, fp)
    }
}

/// `wint_t putwc(wchar_t wc, FILE *stream)`
///
/// # Safety
///
/// As [`fputwc`].
pub unsafe extern "C" fn putwc(wc: c_int, fp: *mut File) -> c_int {
    // SAFETY: the caller's stream.
    unsafe { fputwc(wc, fp) }
}

/// `wint_t putwchar(wchar_t wc)`
pub unsafe extern "C" fn putwchar(wc: c_int) -> c_int {
    // SAFETY: the guest's `stdout`.
    unsafe { fputwc(wc, STDOUTP.get()) }
}

/// `wint_t ungetwc(wint_t wc, FILE *stream)`
///
/// # Safety
///
/// `stream` is open.
pub unsafe extern "C" fn ungetwc(wc: c_int, fp: *mut File) -> c_int {
    if wc == WEOF {
        return WEOF;
    }
    let mut buf = [0u8; MB_MAX];
    // SAFETY: the caller's stream and a local buffer.
    unsafe {
        let _guard = wide(fp);
        // A copy: pushing back must not disturb the read side's state.
        let mut scratch = state(fp).mbstate;
        let n = wcrtomb(buf.as_mut_ptr().cast(), wc, (&raw mut scratch).cast());
        if n == usize::MAX {
            return WEOF;
        }
        for &b in buf[..n].iter().rev() {
            if engine::ungetc(c_int::from(b), fp) == super::EOF {
                return WEOF;
            }
        }
        wc
    }
}

/// `wchar_t *fgetws(wchar_t *ws, int n, FILE *stream)`
///
/// # Safety
///
/// `ws` is writable for `n` wide characters and `stream` is open.
pub unsafe extern "C" fn fgetws(ws: *mut u32, n: c_int, fp: *mut File) -> *mut u32 {
    if n <= 0 {
        crate::errno::set(libc::EINVAL);
        return std::ptr::null_mut();
    }
    // SAFETY: the caller's buffer and stream.
    unsafe {
        let _guard = wide(fp);
        let mut at = 0usize;
        while at < (n - 1) as usize {
            let wc = getwc_locked(fp);
            if wc == WEOF {
                if at == 0 || (*fp).flags & flag::ERR != 0 {
                    return std::ptr::null_mut();
                }
                break;
            }
            *ws.add(at) = wc as u32;
            at += 1;
            if wc == c_int::from(b'\n') {
                break;
            }
        }
        *ws.add(at) = 0;
        ws
    }
}

/// `int fputws(const wchar_t *ws, FILE *stream)`
///
/// # Safety
///
/// `ws` is a NUL-terminated wide string and `stream` is open.
pub unsafe extern "C" fn fputws(ws: *const u32, fp: *mut File) -> c_int {
    // SAFETY: the caller's string and stream.
    unsafe {
        let _guard = wide(fp);
        let mut at = ws;
        while *at != 0 {
            if putwc_locked(*at as c_int, fp) == WEOF {
                return -1;
            }
            at = at.add(1);
        }
        0
    }
}

/// `wchar_t *fgetwln(FILE *stream, size_t *len)`
///
/// # Safety
///
/// `stream` is open and `len` is writable.
pub unsafe extern "C" fn fgetwln(fp: *mut File, len: *mut usize) -> *mut u32 {
    // SAFETY: the caller's stream and variable.
    unsafe {
        let _guard = wide(fp);
        let mut line = std::mem::take(&mut state(fp).wide_line);
        line.clear();
        loop {
            let wc = getwc_locked(fp);
            if wc == WEOF {
                break;
            }
            line.push(wc as u32);
            if wc == c_int::from(b'\n') {
                break;
            }
        }
        *len = line.len();
        let found = match line.is_empty() {
            true => std::ptr::null_mut(),
            false => line.as_mut_ptr(),
        };
        state(fp).wide_line = line;
        found
    }
}

/// `int fwide(FILE *stream, int mode)`
///
/// # Safety
///
/// `stream` is open.
pub unsafe extern "C" fn fwide(fp: *mut File, mode: c_int) -> c_int {
    // SAFETY: the caller's stream.
    unsafe {
        let _guard = Locked::new(fp);
        let state = state(fp);
        if state.orientation == 0 && mode != 0 {
            state.orientation = mode.signum() as i8;
        }
        c_int::from(state.orientation)
    }
}

/// Write wide characters, as `vfwprintf` renders them. The caller holds the
/// lock.
pub(super) unsafe fn write_wide(fp: *mut File, ws: &[u32]) -> bool {
    // SAFETY: the caller's locked stream.
    ws.iter()
        .all(|&wc| unsafe { putwc_locked(wc as c_int, fp) } != WEOF)
}
