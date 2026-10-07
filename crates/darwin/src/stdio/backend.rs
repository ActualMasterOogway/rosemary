//! What a stream reads from and writes to: a descriptor, a guest's `funopen`
//! callbacks, or memory.

use std::ffi::{c_char, c_int, c_void};

use super::{CloseFn, File, ReadFn, SeekFn, WriteFn, flag};

unsafe extern "C" {
    /// Declared here because the bindings do not have it on every host;
    /// `wchar_t` is 32 bits on all of them.
    pub(super) fn mbrtowc(wc: *mut u32, s: *const c_char, n: usize, state: *mut c_void) -> usize;
    pub(super) fn wcrtomb(s: *mut c_char, wc: u32, state: *mut c_void) -> usize;
}

/// The descriptor of a stream whose cookie is itself.
unsafe fn fd_of(cookie: *mut c_void) -> c_int {
    // SAFETY: a descriptor stream's cookie is its own `File`.
    c_int::from(unsafe { (*cookie.cast::<File>()).file })
}

pub(super) unsafe extern "C" fn fd_read(cookie: *mut c_void, buf: *mut c_char, n: c_int) -> c_int {
    // SAFETY: the engine's buffer of `n` bytes and the stream's descriptor.
    unsafe { libc::read(fd_of(cookie), buf.cast(), n as usize) as c_int }
}

pub(super) unsafe extern "C" fn fd_write(
    cookie: *mut c_void,
    buf: *const c_char,
    n: c_int,
) -> c_int {
    // SAFETY: as [`fd_read`]. The guest-facing `write`, so a socket the guest
    // asked not to raise `SIGPIPE` does not.
    unsafe { crate::net::socket::write(fd_of(cookie), buf.cast(), n as usize) as c_int }
}

pub(super) unsafe extern "C" fn fd_seek(cookie: *mut c_void, offset: i64, whence: c_int) -> i64 {
    // SAFETY: the stream's descriptor. `SEEK_*` are 0, 1 and 2 everywhere.
    unsafe { libc::lseek(fd_of(cookie), offset as libc::off_t, whence) as i64 }
}

pub(super) unsafe extern "C" fn fd_close(cookie: *mut c_void) -> c_int {
    // SAFETY: the stream's descriptor, closed through the guest-facing
    // `close` so per-descriptor state goes with it.
    unsafe { crate::net::descriptor::close(fd_of(cookie)) }
}

/// Point `fp` at descriptor `fd`.
///
/// # Safety
///
/// `fp` is a live stream.
pub(super) unsafe fn attach_fd(fp: *mut File, fd: c_int, flags: i16) {
    // SAFETY: the caller's stream.
    unsafe {
        (*fp).flags = flags;
        (*fp).file = fd as i16;
        (*fp).cookie = fp.cast();
        (*fp).read = Some(fd_read);
        (*fp).write = Some(fd_write);
        (*fp).seek = Some(fd_seek);
        (*fp).close = Some(fd_close);
    }
}

/// A stream over arbitrary callbacks, after `funopen`.
fn open_cookie(
    cookie: *mut c_void,
    read: Option<ReadFn>,
    write: Option<WriteFn>,
    seek: Option<SeekFn>,
    close: Option<CloseFn>,
) -> *mut File {
    let flags = match (read.is_some(), write.is_some()) {
        (true, true) => flag::RW,
        (true, false) => flag::RD,
        (false, true) => flag::WR,
        (false, false) => {
            crate::errno::set(libc::EINVAL);
            return std::ptr::null_mut();
        }
    };
    let fp = super::allocate();
    // SAFETY: a fresh stream nothing else has seen.
    unsafe {
        (*fp).flags = flags;
        (*fp).file = -1;
        (*fp).cookie = cookie;
        (*fp).read = read;
        (*fp).write = write;
        (*fp).seek = seek;
        (*fp).close = close;
    }
    fp
}

/// `FILE *funopen(const void *cookie, int (*readfn)(void *, char *, int),
/// int (*writefn)(void *, const char *, int), fpos_t (*seekfn)(void *, fpos_t, int),
/// int (*closefn)(void *))`
///
/// # Safety
///
/// Each callback is null or callable with `cookie` for as long as the stream
/// is open.
pub unsafe extern "C" fn funopen(
    cookie: *const c_void,
    read: Option<ReadFn>,
    write: Option<WriteFn>,
    seek: Option<SeekFn>,
    close: Option<CloseFn>,
) -> *mut File {
    open_cookie(cookie.cast_mut(), read, write, seek, close)
}

/// `fmemopen`'s cookie.
struct Memory {
    buf: *mut u8,
    /// The buffer is this module's to free.
    own: bool,
    /// `b` in the mode: no terminating NUL is maintained.
    binary: bool,
    size: usize,
    /// How much of the buffer holds data.
    len: usize,
    off: usize,
}

unsafe extern "C" fn memory_read(cookie: *mut c_void, buf: *mut c_char, n: c_int) -> c_int {
    // SAFETY: `fmemopen`'s cookie and the engine's buffer of `n` bytes.
    unsafe {
        let m = &mut *cookie.cast::<Memory>();
        let n = (n.max(0) as usize).min(m.len.saturating_sub(m.off));
        std::ptr::copy_nonoverlapping(m.buf.add(m.off), buf.cast::<u8>(), n);
        m.off += n;
        n as c_int
    }
}

unsafe extern "C" fn memory_write(cookie: *mut c_void, buf: *const c_char, n: c_int) -> c_int {
    // SAFETY: as [`memory_read`].
    unsafe {
        let m = &mut *cookie.cast::<Memory>();
        let n = (n.max(0) as usize).min(m.size - m.off);
        if n == 0 {
            return 0;
        }
        std::ptr::copy_nonoverlapping(buf.cast::<u8>(), m.buf.add(m.off), n);
        m.off += n;
        m.len = m.len.max(m.off);
        // A text stream keeps its contents a C string while there is room.
        if !m.binary && m.off < m.size && *m.buf.add(m.off - 1) != 0 {
            *m.buf.add(m.off) = 0;
        }
        n as c_int
    }
}

unsafe extern "C" fn memory_seek(cookie: *mut c_void, offset: i64, whence: c_int) -> i64 {
    // SAFETY: `fmemopen`'s cookie.
    let m = unsafe { &mut *cookie.cast::<Memory>() };
    let base = match whence {
        libc::SEEK_SET => 0,
        libc::SEEK_CUR => m.off as i64,
        libc::SEEK_END => m.len as i64,
        _ => {
            crate::errno::set(libc::EINVAL);
            return -1;
        }
    };
    match base.checked_add(offset) {
        Some(at) if (0..=m.size as i64).contains(&at) => {
            m.off = at as usize;
            at
        }
        _ => {
            crate::errno::set(libc::EINVAL);
            -1
        }
    }
}

unsafe extern "C" fn memory_close(cookie: *mut c_void) -> c_int {
    // SAFETY: the stream's last use of its cookie.
    let m = unsafe { Box::from_raw(cookie.cast::<Memory>()) };
    if m.own {
        // SAFETY: `fmemopen` allocated it with the host's `malloc`.
        unsafe { libc::free(m.buf.cast()) };
    }
    0
}

/// Parsed `fopen` mode: the stream flags and how to open the file.
pub(super) struct Mode {
    pub flags: i16,
    /// 0, 1 or 2: `O_RDONLY`, `O_WRONLY`, `O_RDWR`.
    pub access: c_int,
    pub create: bool,
    pub truncate: bool,
    pub append: bool,
    pub exclusive: bool,
    pub cloexec: bool,
}

impl Mode {
    /// `__sflags`. `None` with `EINVAL` for a mode that does not start with
    /// `r`, `w` or `a`.
    ///
    /// # Safety
    ///
    /// `mode` is null or a NUL-terminated string.
    pub(super) unsafe fn parse(mode: *const c_char) -> Option<Mode> {
        if mode.is_null() {
            crate::errno::set(libc::EINVAL);
            return None;
        }
        // SAFETY: the caller's obligation.
        let bytes = unsafe { std::ffi::CStr::from_ptr(mode) }.to_bytes();
        let mut parsed = match bytes.first() {
            Some(b'r') => Mode::new(flag::RD, 0, false, false, false),
            Some(b'w') => Mode::new(flag::WR, 1, true, true, false),
            Some(b'a') => Mode::new(flag::WR, 1, true, false, true),
            _ => {
                crate::errno::set(libc::EINVAL);
                return None;
            }
        };
        for &b in &bytes[1..] {
            match b {
                b'+' => {
                    parsed.flags = flag::RW;
                    parsed.access = 2;
                }
                b'x' => parsed.exclusive = true,
                b'e' => parsed.cloexec = true,
                _ => {}
            }
        }
        // `x` means "create, and fail if it exists", which `r` never does.
        if parsed.exclusive && !parsed.create {
            crate::errno::set(libc::EINVAL);
            return None;
        }
        Some(parsed)
    }

    fn new(flags: i16, access: c_int, create: bool, truncate: bool, append: bool) -> Mode {
        Mode {
            flags,
            access,
            create,
            truncate,
            append,
            exclusive: false,
            cloexec: false,
        }
    }

    /// The `open` flags, in Darwin's numbering, for the guest-facing `open`.
    pub(super) fn darwin_open_flags(&self) -> c_int {
        const APPEND: c_int = 0x0008;
        const CREAT: c_int = 0x0200;
        const TRUNC: c_int = 0x0400;
        const EXCL: c_int = 0x0800;
        const CLOEXEC: c_int = 0x0100_0000;
        let mut flags = self.access;
        for (set, bit) in [
            (self.append, APPEND),
            (self.create, CREAT),
            (self.truncate, TRUNC),
            (self.exclusive, EXCL),
            (self.cloexec, CLOEXEC),
        ] {
            if set {
                flags |= bit;
            }
        }
        flags
    }
}

/// `FILE *fmemopen(void *buf, size_t size, const char *mode)`
///
/// # Safety
///
/// `buf` is null or `size` bytes the caller keeps alive while the stream is
/// open; `mode` is a NUL-terminated string.
pub unsafe extern "C" fn fmemopen(buf: *mut c_void, size: usize, mode: *const c_char) -> *mut File {
    // SAFETY: the caller's mode.
    let Some(parsed) = (unsafe { Mode::parse(mode) }) else {
        return std::ptr::null_mut();
    };
    if size == 0 || (buf.is_null() && parsed.access != 2) {
        crate::errno::set(libc::EINVAL);
        return std::ptr::null_mut();
    }
    let own = buf.is_null();
    let buf = match own {
        // SAFETY: a fresh block of `size` bytes, nonzero.
        true => unsafe { libc::calloc(1, size) }.cast::<u8>(),
        false => buf.cast::<u8>(),
    };
    if buf.is_null() {
        crate::errno::set(libc::ENOMEM);
        return std::ptr::null_mut();
    }
    // SAFETY: the caller's mode, checked above.
    let mode_bytes = unsafe { std::ffi::CStr::from_ptr(mode) }.to_bytes();
    let binary = mode_bytes.contains(&b'b');
    let (len, off) = match mode_bytes[0] {
        b'a' => {
            // SAFETY: `size` bytes at `buf`.
            let held = unsafe { std::slice::from_raw_parts(buf, size) };
            let len = held.iter().position(|&b| b == 0).unwrap_or(size);
            (len, len)
        }
        b'r' => (size, 0),
        _ => {
            if !binary {
                // SAFETY: at least one byte, since `size` is nonzero.
                unsafe { *buf = 0 };
            }
            (0, 0)
        }
    };
    let cookie = Box::into_raw(Box::new(Memory {
        buf,
        own,
        binary,
        size,
        len,
        off,
    }));
    let fp = open_cookie(
        cookie.cast(),
        (parsed.access != 1).then_some(memory_read as ReadFn),
        (parsed.access != 0).then_some(memory_write as WriteFn),
        Some(memory_seek),
        Some(memory_close),
    );
    // SAFETY: a fresh stream; unbuffered, so a write past the end returns a
    // short count at the call that made it.
    unsafe {
        if parsed.append {
            (*fp).flags |= flag::APP;
        }
        super::setvbuf(fp, std::ptr::null_mut(), super::api::IONBF, 0);
    }
    fp
}

/// `open_memstream`'s and `open_wmemstream`'s cookie: the caller's two
/// variables, which always describe the data so far.
struct MemStream {
    bufp: *mut *mut u8,
    sizep: *mut usize,
    /// Units the buffer holds, not counting the terminator.
    len: usize,
    offset: usize,
    /// Bytes per unit: 1, or 4 for `wchar_t`.
    unit: usize,
    mbstate: [u64; 16],
}

impl MemStream {
    /// Make room for `to` units plus the terminator, zero filled.
    ///
    /// # Safety
    ///
    /// `*bufp` is the stream's own buffer.
    unsafe fn grow(&mut self, to: usize) -> bool {
        if to <= self.len {
            return true;
        }
        let Some(bytes) = to.checked_add(1).and_then(|n| n.checked_mul(self.unit)) else {
            crate::errno::set(libc::EOVERFLOW);
            return false;
        };
        // SAFETY: the guest frees the result, so it comes from the guest's
        // allocator; the old block holds `len + 1` units.
        unsafe {
            let grown = crate::heap::malloc(bytes).cast::<u8>();
            if grown.is_null() {
                crate::errno::set(libc::ENOMEM);
                return false;
            }
            let kept = (self.len + 1) * self.unit;
            std::ptr::copy_nonoverlapping(*self.bufp, grown, kept);
            std::ptr::write_bytes(grown.add(kept), 0, bytes - kept);
            crate::heap::free((*self.bufp).cast());
            *self.bufp = grown;
        }
        self.len = to;
        true
    }

    fn update(&self) {
        // SAFETY: the caller's variable, which outlives the stream.
        unsafe { *self.sizep = self.offset.min(self.len) };
    }
}

unsafe extern "C" fn memstream_write(cookie: *mut c_void, buf: *const c_char, n: c_int) -> c_int {
    // SAFETY: the stream's cookie and the engine's `n` bytes.
    unsafe {
        let ms = &mut *cookie.cast::<MemStream>();
        let n = n.max(0) as usize;
        if !ms.grow(ms.offset + n) {
            return -1;
        }
        if ms.unit == 1 {
            std::ptr::copy_nonoverlapping(buf.cast::<u8>(), (*ms.bufp).add(ms.offset), n);
            ms.offset += n;
        } else {
            // Bytes in, wide characters out, a partial sequence carried over.
            let mut at = 0;
            while at < n {
                let mut wc = 0u32;
                let used = mbrtowc(&mut wc, buf.add(at), n - at, (&raw mut ms.mbstate).cast());
                match used {
                    usize::MAX => return -1,
                    // Incomplete: every remaining byte went into the state.
                    x if x == usize::MAX - 1 => break,
                    _ => {
                        let slot = (*ms.bufp).cast::<u32>().add(ms.offset);
                        slot.write_unaligned(wc);
                        ms.offset += 1;
                        at += used.max(1);
                    }
                }
            }
        }
        ms.update();
        n as c_int
    }
}

unsafe extern "C" fn memstream_seek(cookie: *mut c_void, offset: i64, whence: c_int) -> i64 {
    // SAFETY: the stream's cookie.
    let ms = unsafe { &mut *cookie.cast::<MemStream>() };
    let base = match whence {
        libc::SEEK_SET => 0,
        libc::SEEK_CUR => ms.offset as i64,
        libc::SEEK_END => ms.len as i64,
        _ => {
            crate::errno::set(libc::EINVAL);
            return -1;
        }
    };
    match base.checked_add(offset) {
        Some(at) if at >= 0 => {
            ms.offset = at as usize;
            ms.mbstate = [0; 16];
            ms.update();
            at
        }
        Some(_) => {
            crate::errno::set(libc::EINVAL);
            -1
        }
        None => {
            crate::errno::set(libc::EOVERFLOW);
            -1
        }
    }
}

unsafe extern "C" fn memstream_close(cookie: *mut c_void) -> c_int {
    // SAFETY: the stream's last use of its cookie; the buffer is the guest's now.
    drop(unsafe { Box::from_raw(cookie.cast::<MemStream>()) });
    0
}

/// The shared half of the two `open_*memstream`s.
fn memstream(bufp: *mut *mut u8, sizep: *mut usize, unit: usize, orientation: i8) -> *mut File {
    if bufp.is_null() || sizep.is_null() {
        crate::errno::set(libc::EINVAL);
        return std::ptr::null_mut();
    }
    // SAFETY: one zeroed unit from the guest's allocator, which the guest frees.
    let first = unsafe { crate::heap::malloc(unit) }.cast::<u8>();
    if first.is_null() {
        crate::errno::set(libc::ENOMEM);
        return std::ptr::null_mut();
    }
    // SAFETY: `unit` writable bytes, and the caller's two variables.
    unsafe {
        std::ptr::write_bytes(first, 0, unit);
        *bufp = first;
    }
    let ms = Box::into_raw(Box::new(MemStream {
        bufp,
        sizep,
        len: 0,
        offset: 0,
        unit,
        mbstate: [0; 16],
    }));
    // SAFETY: the fresh cookie.
    unsafe { (*ms).update() };
    let fp = open_cookie(
        ms.cast(),
        None,
        Some(memstream_write),
        Some(memstream_seek),
        Some(memstream_close),
    );
    // SAFETY: a fresh stream, not yet shared.
    unsafe { super::state(fp).orientation = orientation };
    fp
}

/// `FILE *open_memstream(char **bufp, size_t *sizep)`
///
/// # Safety
///
/// Both pointers stay writable while the stream is open.
pub unsafe extern "C" fn open_memstream(bufp: *mut *mut c_char, sizep: *mut usize) -> *mut File {
    memstream(bufp.cast(), sizep, 1, -1)
}

/// `FILE *open_wmemstream(wchar_t **bufp, size_t *sizep)`
///
/// # Safety
///
/// As [`open_memstream`].
pub unsafe extern "C" fn open_wmemstream(bufp: *mut *mut u32, sizep: *mut usize) -> *mut File {
    memstream(bufp.cast(), sizep, 4, 1)
}
