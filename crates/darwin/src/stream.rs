#![cfg(target_os = "linux")]

use std::collections::HashMap;
use std::ffi::{c_char, c_int, c_void};
use std::sync::Mutex;

type ReadFn = unsafe extern "C" fn(*mut c_void, *mut c_char, c_int) -> c_int;
type WriteFn = unsafe extern "C" fn(*mut c_void, *const c_char, c_int) -> c_int;
type SeekFn = unsafe extern "C" fn(*mut c_void, i64, c_int) -> i64;
type CloseFn = unsafe extern "C" fn(*mut c_void) -> c_int;

/// The guest's cookie and callbacks, which is the cookie the host is given.
struct Funopen {
    cookie: *mut c_void,
    read: Option<ReadFn>,
    write: Option<WriteFn>,
    seek: Option<SeekFn>,
    close: Option<CloseFn>,
}

/// The BSD callbacks take an `int` count; a larger request is served in part,
/// which both conventions allow.
fn clamp(size: usize) -> c_int {
    size.min(c_int::MAX as usize) as c_int
}

unsafe extern "C" fn cookie_read(cookie: *mut c_void, buf: *mut c_char, size: usize) -> isize {
    // SAFETY: the host hands back the cookie `funopen` gave it.
    let f = unsafe { &*cookie.cast::<Funopen>() };
    match f.read {
        // SAFETY: the guest's own callback, with its own cookie and the host's
        // buffer of at least that many bytes.
        Some(read) => unsafe { read(f.cookie, buf, clamp(size)) as isize },
        None => {
            crate::errno::set(libc::EBADF);
            -1
        }
    }
}

unsafe extern "C" fn cookie_write(cookie: *mut c_void, buf: *const c_char, size: usize) -> isize {
    // SAFETY: as [`cookie_read`].
    let f = unsafe { &*cookie.cast::<Funopen>() };
    match f.write {
        // SAFETY: as [`cookie_read`]. A failure is -1 in both conventions;
        // glibc's zero would mean "wrote nothing" and loop.
        Some(write) => unsafe { write(f.cookie, buf, clamp(size)) as isize },
        None => {
            crate::errno::set(libc::EBADF);
            -1
        }
    }
}

unsafe extern "C" fn cookie_seek(cookie: *mut c_void, offset: *mut i64, whence: c_int) -> c_int {
    // SAFETY: as [`cookie_read`]; `offset` is the host's.
    unsafe {
        let f = &*cookie.cast::<Funopen>();
        let Some(seek) = f.seek else {
            crate::errno::set(libc::ESPIPE);
            return -1;
        };
        // `SEEK_SET`, `SEEK_CUR` and `SEEK_END` are 0, 1 and 2 on both.
        let at = seek(f.cookie, *offset, whence);
        if at < 0 {
            return -1;
        }
        *offset = at;
        0
    }
}

unsafe extern "C" fn cookie_close(cookie: *mut c_void) -> c_int {
    // SAFETY: the host's last call with this cookie, which is then dropped.
    let f = unsafe { Box::from_raw(cookie.cast::<Funopen>()) };
    match f.close {
        // SAFETY: the guest's callback and cookie.
        Some(close) => unsafe { close(f.cookie) },
        None => 0,
    }
}

/// glibc's `cookie_io_functions_t`.
#[repr(C)]
struct CookieIo {
    read: Option<unsafe extern "C" fn(*mut c_void, *mut c_char, usize) -> isize>,
    write: Option<unsafe extern "C" fn(*mut c_void, *const c_char, usize) -> isize>,
    seek: Option<unsafe extern "C" fn(*mut c_void, *mut i64, c_int) -> c_int>,
    close: Option<unsafe extern "C" fn(*mut c_void) -> c_int>,
}

unsafe extern "C" {
    fn fopencookie(cookie: *mut c_void, mode: *const c_char, io: CookieIo) -> *mut libc::FILE;
}

/// `FILE *funopen(const void *cookie, int (*readfn)(void *, char *, int),
/// int (*writefn)(void *, const char *, int), fpos_t (*seekfn)(void *, fpos_t, int),
/// int (*closefn)(void *))`
///
/// # Safety
///
/// Each callback is null or callable from this thread with `cookie`, for as
/// long as the stream is open.
pub unsafe extern "C" fn funopen(
    cookie: *const c_void,
    read: Option<ReadFn>,
    write: Option<WriteFn>,
    seek: Option<SeekFn>,
    close: Option<CloseFn>,
) -> *mut libc::FILE {
    // The mode follows from which directions have a callback, as BSD's does.
    let mode = match (read.is_some(), write.is_some()) {
        (true, true) => c"r+",
        (true, false) => c"r",
        (false, true) => c"w",
        (false, false) => {
            crate::errno::set(libc::EINVAL);
            return std::ptr::null_mut();
        }
    };
    let state = Box::into_raw(Box::new(Funopen {
        cookie: cookie.cast_mut(),
        read,
        write,
        seek,
        close,
    }));
    let io = CookieIo {
        read: Some(cookie_read),
        write: Some(cookie_write),
        seek: Some(cookie_seek),
        close: Some(cookie_close),
    };
    // SAFETY: a cookie this function owns until the host's close hands it back.
    let stream = unsafe { fopencookie(state.cast(), mode.as_ptr(), io) };
    if stream.is_null() {
        // SAFETY: the host never took it.
        drop(unsafe { Box::from_raw(state) });
    }
    stream
}

/// One line buffer per stream that `fgetln` has read from.
///
/// Keyed by the stream's address. A stream closed and another opened at the
/// same address reuses the buffer, which is harmless: the contract is only
/// that the line lives until the next read of that stream.
static LINES: Mutex<Option<HashMap<usize, Line>>> = Mutex::new(None);

struct Line {
    buf: *mut c_char,
    cap: usize,
}

// SAFETY: the buffer is only touched under `LINES`'s lock, and a stream is
// not read from two threads at once without the caller's own locking.
unsafe impl Send for Line {}

/// `char *fgetln(FILE *stream, size_t *len)`
///
/// The line, newline included and not terminated, valid until the next read
/// of `stream`; null at the end of the file or on an error.
///
/// # Safety
///
/// `stream` is an open stream and `len` is writable.
pub unsafe extern "C" fn fgetln(stream: *mut libc::FILE, len: *mut usize) -> *mut c_char {
    let mut lines = LINES.lock().unwrap_or_else(|e| e.into_inner());
    let line = lines
        .get_or_insert_with(HashMap::new)
        .entry(stream as usize)
        .or_insert(Line {
            buf: std::ptr::null_mut(),
            cap: 0,
        });
    // SAFETY: the caller's stream, and a buffer `getline` owns and grows with
    // the host's allocator.
    let n = unsafe { libc::getline(&mut line.buf, &mut line.cap, stream) };
    if n < 0 {
        return std::ptr::null_mut();
    }
    unsafe { *len = n as usize };
    line.buf
}

exports::exports! {
    functions {
        "funopen" => funopen,
        "fgetln" => fgetln,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Source {
        data: &'static [u8],
        at: usize,
        closed: bool,
    }

    unsafe extern "C" fn read_source(cookie: *mut c_void, buf: *mut c_char, n: c_int) -> c_int {
        // SAFETY: the test's own cookie and the host's buffer.
        unsafe {
            let s = &mut *cookie.cast::<Source>();
            let take = (n as usize).min(s.data.len() - s.at);
            std::ptr::copy_nonoverlapping(s.data[s.at..].as_ptr().cast(), buf, take);
            s.at += take;
            take as c_int
        }
    }

    unsafe extern "C" fn close_source(cookie: *mut c_void) -> c_int {
        // SAFETY: the test's own cookie.
        unsafe { (*cookie.cast::<Source>()).closed = true };
        0
    }

    #[test]
    fn a_funopen_stream_reads_through_the_callback_and_fgetln_splits_lines() {
        let mut source = Source {
            data: b"first\nsecond",
            at: 0,
            closed: false,
        };
        // SAFETY: a cookie that outlives the stream, and callbacks of this
        // test's own.
        unsafe {
            let f = funopen(
                (&raw mut source).cast(),
                Some(read_source),
                None,
                None,
                Some(close_source),
            );
            assert!(!f.is_null());
            let mut len = 0;
            let line = fgetln(f, &mut len);
            assert_eq!(
                std::slice::from_raw_parts(line.cast::<u8>(), len),
                b"first\n"
            );
            let line = fgetln(f, &mut len);
            assert_eq!(
                std::slice::from_raw_parts(line.cast::<u8>(), len),
                b"second"
            );
            assert!(fgetln(f, &mut len).is_null());
            assert_eq!(libc::fclose(f), 0);
        }
        assert!(source.closed);
    }

    #[test]
    fn a_stream_with_no_direction_is_refused() {
        // SAFETY: no callbacks, so nothing is ever called.
        let f = unsafe { funopen(std::ptr::null(), None, None, None, None) };
        assert!(f.is_null());
        assert_eq!(crate::errno::get(), libc::EINVAL);
    }
}
