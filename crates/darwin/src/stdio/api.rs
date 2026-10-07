//! The byte-oriented functions, each taking the stream's lock unless its name
//! says `_unlocked`.

use std::ffi::{c_char, c_int, c_long, c_void};

use super::{EOF, File, Locked, STDERRP, STDINP, STDOUTP, engine, flag};

pub const IOFBF: c_int = 0;
pub const IOLBF: c_int = 1;
pub const IONBF: c_int = 2;

/// Lock `fp` and mark it byte oriented, for the length of the guard.
unsafe fn bytes(fp: *mut File) -> Locked {
    // SAFETY: the caller's live stream.
    unsafe {
        let guard = Locked::new(fp);
        engine::orient(fp, -1);
        guard
    }
}

/// `size_t fread(void *buf, size_t size, size_t count, FILE *stream)`
///
/// # Safety
///
/// `buf` is writable for `size * count` bytes and `stream` is open.
pub unsafe extern "C" fn fread(
    buf: *mut c_void,
    size: usize,
    count: usize,
    fp: *mut File,
) -> usize {
    let Some(total) = size.checked_mul(count) else {
        crate::errno::set(libc::EINVAL);
        // SAFETY: the caller's stream.
        unsafe { (*fp).flags |= flag::ERR };
        return 0;
    };
    if total == 0 {
        return 0;
    }
    // SAFETY: the caller's buffer and stream.
    unsafe {
        let _guard = bytes(fp);
        engine::read_all(fp, buf.cast(), total) / size
    }
}

/// `size_t fwrite(const void *buf, size_t size, size_t count, FILE *stream)`
///
/// # Safety
///
/// `buf` is readable for `size * count` bytes and `stream` is open.
pub unsafe extern "C" fn fwrite(
    buf: *const c_void,
    size: usize,
    count: usize,
    fp: *mut File,
) -> usize {
    let Some(total) = size.checked_mul(count) else {
        crate::errno::set(libc::EINVAL);
        // SAFETY: the caller's stream.
        unsafe { (*fp).flags |= flag::ERR };
        return 0;
    };
    if total == 0 {
        return 0;
    }
    // SAFETY: the caller's buffer and stream.
    unsafe {
        let _guard = bytes(fp);
        engine::write_all(fp, buf.cast(), total) / size
    }
}

/// `int fgetc(FILE *stream)`
///
/// # Safety
///
/// `stream` is open.
pub unsafe extern "C" fn fgetc(fp: *mut File) -> c_int {
    // SAFETY: the caller's stream.
    unsafe {
        let _guard = bytes(fp);
        engine::getc(fp)
    }
}

/// `int getc(FILE *stream)`
///
/// # Safety
///
/// As [`fgetc`].
pub unsafe extern "C" fn getc(fp: *mut File) -> c_int {
    // SAFETY: the caller's stream.
    unsafe { fgetc(fp) }
}

/// `int getc_unlocked(FILE *stream)`
///
/// # Safety
///
/// As [`fgetc`], with the caller holding the lock if it is shared.
pub unsafe extern "C" fn getc_unlocked(fp: *mut File) -> c_int {
    // SAFETY: the caller's stream.
    unsafe { engine::getc(fp) }
}

/// `int getchar(void)`
pub unsafe extern "C" fn getchar() -> c_int {
    // SAFETY: the guest's `stdin`.
    unsafe { fgetc(STDINP.get()) }
}

/// `int getchar_unlocked(void)`
pub unsafe extern "C" fn getchar_unlocked() -> c_int {
    // SAFETY: the guest's `stdin`.
    unsafe { engine::getc(STDINP.get()) }
}

/// `int fputc(int c, FILE *stream)`
///
/// # Safety
///
/// `stream` is open.
pub unsafe extern "C" fn fputc(c: c_int, fp: *mut File) -> c_int {
    // SAFETY: the caller's stream.
    unsafe {
        let _guard = bytes(fp);
        engine::putc(c, fp)
    }
}

/// `int putc(int c, FILE *stream)`
///
/// # Safety
///
/// As [`fputc`].
pub unsafe extern "C" fn putc(c: c_int, fp: *mut File) -> c_int {
    // SAFETY: the caller's stream.
    unsafe { fputc(c, fp) }
}

/// `int putc_unlocked(int c, FILE *stream)`
///
/// # Safety
///
/// As [`fputc`], with the caller holding the lock if it is shared.
pub unsafe extern "C" fn putc_unlocked(c: c_int, fp: *mut File) -> c_int {
    // SAFETY: the caller's stream.
    unsafe { engine::putc(c, fp) }
}

/// `int putchar(int c)`
pub unsafe extern "C" fn putchar(c: c_int) -> c_int {
    // SAFETY: the guest's `stdout`.
    unsafe { fputc(c, STDOUTP.get()) }
}

/// `int putchar_unlocked(int c)`
pub unsafe extern "C" fn putchar_unlocked(c: c_int) -> c_int {
    // SAFETY: the guest's `stdout`.
    unsafe { engine::putc(c, STDOUTP.get()) }
}

/// `int __srget(FILE *)`: the refill the inlined `getc` calls. Unlocked, as
/// its caller is.
///
/// # Safety
///
/// `stream` is open.
pub unsafe extern "C" fn __srget(fp: *mut File) -> c_int {
    // SAFETY: the caller's stream.
    unsafe {
        engine::orient(fp, -1);
        engine::srget(fp)
    }
}

/// `int __swbuf(int c, FILE *)`: the flush the inlined `putc` calls.
///
/// # Safety
///
/// `stream` is open.
pub unsafe extern "C" fn __swbuf(c: c_int, fp: *mut File) -> c_int {
    // SAFETY: the caller's stream.
    unsafe {
        engine::orient(fp, -1);
        engine::swbuf(c, fp)
    }
}

/// `char *fgets(char *buf, int n, FILE *stream)`
///
/// # Safety
///
/// `buf` is writable for `n` bytes and `stream` is open.
pub unsafe extern "C" fn fgets(buf: *mut c_char, n: c_int, fp: *mut File) -> *mut c_char {
    if n <= 0 {
        crate::errno::set(libc::EINVAL);
        return std::ptr::null_mut();
    }
    // SAFETY: the caller's buffer and stream; every copy stays within the
    // `n - 1` bytes left for the line.
    unsafe {
        let _guard = bytes(fp);
        let mut s = buf.cast::<u8>();
        let mut left = (n - 1) as usize;
        while left != 0 {
            if (*fp).r <= 0 && !engine::refill(fp) {
                // A partial line ends at end-of-file; an error loses it.
                if s == buf.cast() || (*fp).flags & flag::ERR != 0 {
                    return std::ptr::null_mut();
                }
                break;
            }
            let len = ((*fp).r as usize).min(left);
            let here = std::slice::from_raw_parts((*fp).p, len);
            let take = here.iter().position(|&b| b == b'\n').map_or(len, |i| i + 1);
            std::ptr::copy_nonoverlapping((*fp).p, s, take);
            (*fp).r -= take as c_int;
            (*fp).p = (*fp).p.add(take);
            s = s.add(take);
            left -= take;
            if take < len || *s.sub(1) == b'\n' {
                break;
            }
        }
        *s = 0;
        buf
    }
}

/// `char *gets(char *buf)`
///
/// # Safety
///
/// `buf` holds the whole line, which is why nobody should call this.
pub unsafe extern "C" fn gets(buf: *mut c_char) -> *mut c_char {
    let fp = STDINP.get();
    // SAFETY: the caller's buffer and the guest's `stdin`.
    unsafe {
        let _guard = bytes(fp);
        let mut s = buf.cast::<u8>();
        loop {
            let c = engine::getc(fp);
            if c == EOF {
                if s == buf.cast() || (*fp).flags & flag::ERR != 0 {
                    return std::ptr::null_mut();
                }
                break;
            }
            if c == c_int::from(b'\n') {
                break;
            }
            *s = c as u8;
            s = s.add(1);
        }
        *s = 0;
        buf
    }
}

/// `int fputs(const char *s, FILE *stream)`
///
/// # Safety
///
/// `s` is a NUL-terminated string and `stream` is open.
pub unsafe extern "C" fn fputs(s: *const c_char, fp: *mut File) -> c_int {
    // SAFETY: the caller's string and stream.
    unsafe {
        let len = libc::strlen(s);
        let _guard = bytes(fp);
        match engine::write_all(fp, s.cast(), len) == len {
            true => len.min(c_int::MAX as usize) as c_int,
            false => EOF,
        }
    }
}

/// `int puts(const char *s)`
///
/// # Safety
///
/// `s` is a NUL-terminated string.
pub unsafe extern "C" fn puts(s: *const c_char) -> c_int {
    let fp = STDOUTP.get();
    // SAFETY: the caller's string and the guest's `stdout`.
    unsafe {
        let len = libc::strlen(s);
        let _guard = bytes(fp);
        if engine::write_all(fp, s.cast(), len) != len
            || engine::write_all(fp, b"\n".as_ptr(), 1) != 1
        {
            return EOF;
        }
        c_int::from(b'\n')
    }
}

/// `int ungetc(int c, FILE *stream)`
///
/// # Safety
///
/// `stream` is open.
pub unsafe extern "C" fn ungetc(c: c_int, fp: *mut File) -> c_int {
    // SAFETY: the caller's stream.
    unsafe {
        let _guard = bytes(fp);
        engine::ungetc(c, fp)
    }
}

/// `int getw(FILE *stream)`
///
/// # Safety
///
/// `stream` is open.
pub unsafe extern "C" fn getw(fp: *mut File) -> c_int {
    let mut word: c_int = 0;
    // SAFETY: a local `int` and the caller's stream.
    match unsafe { fread((&raw mut word).cast(), size_of::<c_int>(), 1, fp) } {
        1 => word,
        _ => EOF,
    }
}

/// `int putw(int w, FILE *stream)`
///
/// # Safety
///
/// `stream` is open.
pub unsafe extern "C" fn putw(word: c_int, fp: *mut File) -> c_int {
    // SAFETY: a local `int` and the caller's stream.
    match unsafe { fwrite((&raw const word).cast(), size_of::<c_int>(), 1, fp) } {
        1 => 0,
        _ => EOF,
    }
}

/// Make `*linep` hold at least `need` bytes, keeping the first `keep`.
///
/// The buffer is the guest's to free, so it comes from the guest's allocator.
unsafe fn expand(linep: *mut *mut c_char, capp: *mut usize, need: usize, keep: usize) -> bool {
    // SAFETY: the caller's two variables and the block they describe.
    unsafe {
        if !(*linep).is_null() && *capp >= need {
            return true;
        }
        let size = need.checked_next_power_of_two().unwrap_or(need).max(128);
        let grown = crate::heap::malloc(size).cast::<c_char>();
        if grown.is_null() {
            crate::errno::set(libc::ENOMEM);
            return false;
        }
        if !(*linep).is_null() {
            std::ptr::copy_nonoverlapping(*linep, grown, keep);
            crate::heap::free((*linep).cast());
        }
        *linep = grown;
        *capp = size;
        true
    }
}

/// `ssize_t getdelim(char **linep, size_t *capp, int delim, FILE *stream)`
///
/// # Safety
///
/// `linep` and `capp` are writable and describe a block the guest's
/// allocator made, or `*linep` is null; `stream` is open.
pub unsafe extern "C" fn getdelim(
    linep: *mut *mut c_char,
    capp: *mut usize,
    delim: c_int,
    fp: *mut File,
) -> isize {
    // SAFETY: the caller's variables and stream.
    unsafe {
        let _guard = bytes(fp);
        if linep.is_null() || capp.is_null() {
            crate::errno::set(libc::EINVAL);
            (*fp).flags |= flag::ERR;
            return -1;
        }
        if (*linep).is_null() {
            *capp = 0;
        }
        let delim = delim as u8;
        let mut len = 0usize;
        if (*fp).r <= 0 && !engine::refill(fp) {
            // Nothing read: still hand back an empty string.
            if expand(linep, capp, 1, 0) {
                **linep = 0;
            }
            return -1;
        }
        loop {
            let avail = (*fp).r as usize;
            let here = std::slice::from_raw_parts((*fp).p, avail);
            let found = here.iter().position(|&b| b == delim);
            let take = found.map_or(avail, |i| i + 1);
            let Some(need) = len.checked_add(take + 1) else {
                crate::errno::set(libc::EOVERFLOW);
                (*fp).flags |= flag::ERR;
                return -1;
            };
            if !expand(linep, capp, need, len) {
                (*fp).flags |= flag::ERR;
                return -1;
            }
            std::ptr::copy_nonoverlapping((*fp).p, (*linep).add(len).cast(), take);
            (*fp).p = (*fp).p.add(take);
            (*fp).r -= take as c_int;
            len += take;
            if found.is_some() {
                break;
            }
            if !engine::refill(fp) {
                if (*fp).flags & flag::ERR != 0 {
                    return -1;
                }
                break;
            }
        }
        *(*linep).add(len) = 0;
        len as isize
    }
}

/// `ssize_t getline(char **linep, size_t *capp, FILE *stream)`
///
/// # Safety
///
/// As [`getdelim`].
pub unsafe extern "C" fn getline(
    linep: *mut *mut c_char,
    capp: *mut usize,
    fp: *mut File,
) -> isize {
    // SAFETY: the caller's obligations, passed on.
    unsafe { getdelim(linep, capp, c_int::from(b'\n'), fp) }
}

/// Make `fgetln`'s line buffer hold at least `need` bytes.
unsafe fn lb_expand(fp: *mut File, need: usize) -> bool {
    // SAFETY: the caller's locked stream; `lb` is this module's own block.
    unsafe {
        if (*fp).lb.size as usize >= need {
            return true;
        }
        let Ok(size) = c_int::try_from(need) else {
            crate::errno::set(libc::EOVERFLOW);
            return false;
        };
        let grown = libc::realloc((*fp).lb.base.cast(), need).cast::<u8>();
        if grown.is_null() {
            crate::errno::set(libc::ENOMEM);
            return false;
        }
        (*fp).lb.base = grown;
        (*fp).lb.size = size;
        true
    }
}

/// `char *fgetln(FILE *stream, size_t *len)`
///
/// The line, newline included and not terminated, valid until the next read
/// of `stream`; null at end-of-file or on an error.
///
/// # Safety
///
/// `stream` is open and `len` is writable.
pub unsafe extern "C" fn fgetln(fp: *mut File, lenp: *mut usize) -> *mut c_char {
    // SAFETY: the caller's stream and variable.
    unsafe {
        let _guard = bytes(fp);
        if (*fp).r <= 0 && !engine::refill(fp) {
            *lenp = 0;
            return std::ptr::null_mut();
        }
        let here = std::slice::from_raw_parts((*fp).p, (*fp).r as usize);
        // A whole line already buffered is handed out in place.
        if let Some(i) = here.iter().position(|&b| b == b'\n') {
            let line = (*fp).p;
            let len = i + 1;
            (*fp).flags |= flag::MOD;
            (*fp).r -= len as c_int;
            (*fp).p = (*fp).p.add(len);
            *lenp = len;
            return line.cast();
        }
        // Otherwise it is gathered in the line buffer across refills.
        let mut len = 0usize;
        loop {
            let avail = (*fp).r as usize;
            let here = std::slice::from_raw_parts((*fp).p, avail);
            let found = here.iter().position(|&b| b == b'\n');
            let take = found.map_or(avail, |i| i + 1);
            if !lb_expand(fp, len + take) {
                *lenp = 0;
                return std::ptr::null_mut();
            }
            std::ptr::copy_nonoverlapping((*fp).p, (*fp).lb.base.add(len), take);
            (*fp).p = (*fp).p.add(take);
            (*fp).r -= take as c_int;
            len += take;
            if found.is_some() {
                break;
            }
            if !engine::refill(fp) {
                if (*fp).flags & flag::EOF != 0 && len > 0 {
                    break;
                }
                *lenp = 0;
                return std::ptr::null_mut();
            }
        }
        *lenp = len;
        (*fp).lb.base.cast()
    }
}

/// `void perror(const char *s)`
///
/// # Safety
///
/// `s` is null or a NUL-terminated string.
pub unsafe extern "C" fn perror(s: *const c_char) {
    let errno = crate::errno::guest_errno();
    let mut line = Vec::new();
    // SAFETY: the caller's string; `strerror` answers a NUL-terminated one.
    unsafe {
        if !s.is_null() && *s != 0 {
            line.extend_from_slice(std::ffi::CStr::from_ptr(s).to_bytes());
            line.extend_from_slice(b": ");
        }
        line.extend_from_slice(std::ffi::CStr::from_ptr(crate::string::strerror(errno)).to_bytes());
    }
    line.push(b'\n');
    let fp = STDERRP.get();
    // SAFETY: the guest's `stderr`. Whatever it buffered goes first.
    unsafe {
        let _guard = Locked::new(fp);
        engine::sflush(fp);
        engine::write_all(fp, line.as_ptr(), line.len());
        engine::sflush(fp);
    }
    crate::errno::set(crate::errno::to_host(errno));
}

/// Flush every stream; `blocking` waits for streams another thread holds.
pub(super) fn flush_all(blocking: bool) -> c_int {
    let mut rc = 0;
    for fp in super::every_stream() {
        // SAFETY: a registered stream.
        unsafe {
            if (*fp).flags & flag::WR == 0 {
                continue;
            }
            let extra = &*(*fp).extra;
            match blocking {
                true => extra.lock.lock(),
                false if extra.lock.try_lock() => {}
                false => continue,
            }
            if !engine::sflush(fp) {
                rc = EOF;
            }
            extra.lock.unlock();
        }
    }
    rc
}

/// `int fflush(FILE *stream)`, or every stream for a null one.
///
/// # Safety
///
/// `stream` is null or open.
pub unsafe extern "C" fn fflush(fp: *mut File) -> c_int {
    if fp.is_null() {
        return flush_all(true);
    }
    // SAFETY: the caller's stream.
    unsafe {
        let _guard = Locked::new(fp);
        if (*fp).flags & (flag::WR | flag::RW) == 0 {
            crate::errno::set(libc::EBADF);
            return EOF;
        }
        match engine::sflush(fp) {
            true => 0,
            false => EOF,
        }
    }
}

/// `int fpurge(FILE *stream)`
///
/// # Safety
///
/// `stream` is open.
pub unsafe extern "C" fn fpurge(fp: *mut File) -> c_int {
    // SAFETY: the caller's stream.
    unsafe {
        let _guard = Locked::new(fp);
        if (*fp).flags == 0 {
            crate::errno::set(libc::EBADF);
            return EOF;
        }
        engine::purge(fp);
        0
    }
}

/// `int fseeko(FILE *stream, off_t offset, int whence)`
///
/// # Safety
///
/// `stream` is open.
pub unsafe extern "C" fn fseeko(fp: *mut File, offset: i64, whence: c_int) -> c_int {
    // SAFETY: the caller's stream.
    unsafe {
        let _guard = Locked::new(fp);
        engine::seek(fp, offset, whence)
    }
}

/// `int fseek(FILE *stream, long offset, int whence)`
///
/// # Safety
///
/// `stream` is open.
pub unsafe extern "C" fn fseek(fp: *mut File, offset: c_long, whence: c_int) -> c_int {
    // SAFETY: the caller's stream.
    unsafe { fseeko(fp, offset as i64, whence) }
}

/// `off_t ftello(FILE *stream)`
///
/// # Safety
///
/// `stream` is open.
pub unsafe extern "C" fn ftello(fp: *mut File) -> i64 {
    // SAFETY: the caller's stream.
    unsafe {
        let _guard = Locked::new(fp);
        engine::tell(fp)
    }
}

/// `long ftell(FILE *stream)`
///
/// # Safety
///
/// `stream` is open.
pub unsafe extern "C" fn ftell(fp: *mut File) -> c_long {
    // SAFETY: the caller's stream.
    let at = unsafe { ftello(fp) };
    match c_long::try_from(at) {
        Ok(at) => at,
        Err(_) => {
            crate::errno::set(libc::EOVERFLOW);
            -1
        }
    }
}

/// `void rewind(FILE *stream)`
///
/// # Safety
///
/// `stream` is open.
pub unsafe extern "C" fn rewind(fp: *mut File) {
    let held = crate::errno::Held::now();
    // SAFETY: the caller's stream.
    unsafe {
        let _guard = Locked::new(fp);
        if engine::seek(fp, 0, libc::SEEK_SET) == 0 {
            (*fp).flags &= !(flag::ERR | flag::EOF);
        } else {
            std::mem::forget(held);
        }
    }
}

/// `int fgetpos(FILE *stream, fpos_t *pos)`
///
/// # Safety
///
/// `stream` is open and `pos` is writable.
pub unsafe extern "C" fn fgetpos(fp: *mut File, pos: *mut i64) -> c_int {
    // SAFETY: the caller's stream and variable.
    unsafe {
        let at = ftello(fp);
        if at < 0 {
            return -1;
        }
        *pos = at;
        0
    }
}

/// `int fsetpos(FILE *stream, const fpos_t *pos)`
///
/// # Safety
///
/// `stream` is open and `pos` is readable.
pub unsafe extern "C" fn fsetpos(fp: *mut File, pos: *const i64) -> c_int {
    // SAFETY: the caller's stream and variable.
    unsafe { fseeko(fp, *pos, libc::SEEK_SET) }
}

/// `int feof_unlocked(FILE *stream)`
///
/// # Safety
///
/// `stream` is open.
pub unsafe extern "C" fn feof_unlocked(fp: *mut File) -> c_int {
    // SAFETY: the caller's stream.
    c_int::from(unsafe { (*fp).flags } & flag::EOF != 0)
}

/// `int feof(FILE *stream)`
///
/// # Safety
///
/// `stream` is open.
pub unsafe extern "C" fn feof(fp: *mut File) -> c_int {
    // SAFETY: the caller's stream.
    unsafe {
        let _guard = Locked::new(fp);
        feof_unlocked(fp)
    }
}

/// `int ferror_unlocked(FILE *stream)`
///
/// # Safety
///
/// `stream` is open.
pub unsafe extern "C" fn ferror_unlocked(fp: *mut File) -> c_int {
    // SAFETY: the caller's stream.
    c_int::from(unsafe { (*fp).flags } & flag::ERR != 0)
}

/// `int ferror(FILE *stream)`
///
/// # Safety
///
/// `stream` is open.
pub unsafe extern "C" fn ferror(fp: *mut File) -> c_int {
    // SAFETY: the caller's stream.
    unsafe {
        let _guard = Locked::new(fp);
        ferror_unlocked(fp)
    }
}

/// `void clearerr_unlocked(FILE *stream)`
///
/// # Safety
///
/// `stream` is open.
pub unsafe extern "C" fn clearerr_unlocked(fp: *mut File) {
    // SAFETY: the caller's stream.
    unsafe { (*fp).flags &= !(flag::ERR | flag::EOF) };
}

/// `void clearerr(FILE *stream)`
///
/// # Safety
///
/// `stream` is open.
pub unsafe extern "C" fn clearerr(fp: *mut File) {
    // SAFETY: the caller's stream.
    unsafe {
        let _guard = Locked::new(fp);
        clearerr_unlocked(fp);
    }
}

/// `int fileno_unlocked(FILE *stream)`
///
/// # Safety
///
/// `stream` is open.
pub unsafe extern "C" fn fileno_unlocked(fp: *mut File) -> c_int {
    // SAFETY: the caller's stream.
    let fd = c_int::from(unsafe { (*fp).file });
    if fd < 0 {
        crate::errno::set(libc::EBADF);
    }
    fd
}

/// `int fileno(FILE *stream)`
///
/// # Safety
///
/// `stream` is open.
pub unsafe extern "C" fn fileno(fp: *mut File) -> c_int {
    // SAFETY: the caller's stream.
    unsafe {
        let _guard = Locked::new(fp);
        fileno_unlocked(fp)
    }
}

/// `int setvbuf(FILE *stream, char *buf, int mode, size_t size)`
///
/// # Safety
///
/// `stream` is open, and `buf` is null or `size` bytes that outlive it.
pub unsafe extern "C" fn setvbuf(
    fp: *mut File,
    buf: *mut c_char,
    mode: c_int,
    size: usize,
) -> c_int {
    if mode != IONBF && ((mode != IOFBF && mode != IOLBF) || size > c_int::MAX as usize) {
        return EOF;
    }
    // SAFETY: the caller's stream and buffer.
    unsafe {
        let _guard = Locked::new(fp);
        let mut rc = 0;
        engine::sflush(fp);
        if engine::has_ub(fp) {
            engine::free_ub(fp);
        }
        (*fp).r = 0;
        (*fp).lbfsize = 0;
        let mut flags = (*fp).flags;
        if flags & flag::MBF != 0 {
            libc::free((*fp).bf.base.cast());
        }
        flags &=
            !(flag::LBF | flag::NBF | flag::MBF | flag::OPT | flag::OFF | flag::NPT | flag::EOF);
        let unbuffered = |fp: *mut File, flags: i16| {
            (*fp).flags = flags | flag::NBF;
            (*fp).w = 0;
            engine::use_nbuf(fp);
        };
        if mode == IONBF {
            unbuffered(fp, flags);
            return rc;
        }
        let (more, natural, _) = engine::whatbuf(fp);
        flags |= more;
        let mut size = size;
        let mut buf = buf.cast::<u8>();
        if size == 0 {
            buf = std::ptr::null_mut();
            size = natural as usize;
        }
        if buf.is_null() {
            buf = libc::malloc(size).cast();
            if buf.is_null() {
                rc = EOF;
                if size != natural as usize {
                    size = natural as usize;
                    buf = libc::malloc(size).cast();
                }
            }
            if buf.is_null() {
                unbuffered(fp, flags);
                return rc;
            }
            flags |= flag::MBF;
        }
        if size != natural as usize {
            flags |= flag::NPT;
        }
        if mode == IOLBF {
            flags |= flag::LBF;
        }
        (*fp).flags = flags;
        (*fp).bf.base = buf;
        (*fp).p = buf;
        (*fp).bf.size = size as c_int;
        if flags & flag::WR != 0 {
            if flags & flag::LBF != 0 {
                (*fp).w = 0;
                (*fp).lbfsize = -(*fp).bf.size;
            } else {
                (*fp).w = size as c_int;
            }
        } else {
            (*fp).w = 0;
        }
        super::register_cleanup();
        rc
    }
}

/// `void setbuf(FILE *stream, char *buf)`
///
/// # Safety
///
/// As [`setvbuf`], with `BUFSIZ` bytes.
pub unsafe extern "C" fn setbuf(fp: *mut File, buf: *mut c_char) {
    let mode = if buf.is_null() { IONBF } else { IOFBF };
    // SAFETY: the caller's obligations, passed on.
    unsafe { setvbuf(fp, buf, mode, super::BUFSIZ as usize) };
}

/// `void setbuffer(FILE *stream, char *buf, int size)`
///
/// # Safety
///
/// As [`setvbuf`].
pub unsafe extern "C" fn setbuffer(fp: *mut File, buf: *mut c_char, size: c_int) {
    let mode = if buf.is_null() { IONBF } else { IOFBF };
    // SAFETY: the caller's obligations, passed on.
    unsafe { setvbuf(fp, buf, mode, size.max(0) as usize) };
}

/// `int setlinebuf(FILE *stream)`
///
/// # Safety
///
/// `stream` is open.
pub unsafe extern "C" fn setlinebuf(fp: *mut File) -> c_int {
    // SAFETY: the caller's stream.
    unsafe { setvbuf(fp, std::ptr::null_mut(), IOLBF, 0) }
}

/// `void flockfile(FILE *stream)`
///
/// # Safety
///
/// `stream` is open.
pub unsafe extern "C" fn flockfile(fp: *mut File) {
    // SAFETY: the caller's stream.
    unsafe { (*(*fp).extra).lock.lock() };
}

/// `int ftrylockfile(FILE *stream)`: 0 if the lock was taken.
///
/// # Safety
///
/// `stream` is open.
pub unsafe extern "C" fn ftrylockfile(fp: *mut File) -> c_int {
    // SAFETY: the caller's stream.
    c_int::from(!unsafe { (*(*fp).extra).lock.try_lock() })
}

/// `void funlockfile(FILE *stream)`
///
/// # Safety
///
/// `stream` is open.
pub unsafe extern "C" fn funlockfile(fp: *mut File) {
    // SAFETY: the caller's stream.
    unsafe { (*(*fp).extra).lock.unlock() };
}
