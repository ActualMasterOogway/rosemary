use std::ffi::{c_int, c_void};

use super::{BUFSIZ, EOF, File, flag, state};

/// Whether an `ungetc` buffer is active.
pub(super) unsafe fn has_ub(fp: *mut File) -> bool {
    // SAFETY: the caller's live stream.
    unsafe { !(*fp).ub.base.is_null() }
}

/// Drop the `ungetc` buffer.
pub(super) unsafe fn free_ub(fp: *mut File) {
    // SAFETY: the caller's live, locked stream; `ub.base` is `ubuf` or a block
    // `submore` allocated.
    unsafe {
        let ub = (*fp).ub.base;
        if !ub.is_null() && ub != (&raw mut (*fp).ubuf).cast() {
            libc::free(ub.cast());
        }
        (*fp).ub.base = std::ptr::null_mut();
    }
}

/// Drop `fgetln`'s line buffer.
pub(super) unsafe fn free_lb(fp: *mut File) {
    // SAFETY: as [`free_ub`]; `lb.base` is null or `libc::realloc`'s.
    unsafe {
        libc::free((*fp).lb.base.cast());
        (*fp).lb = super::Sbuf {
            base: std::ptr::null_mut(),
            size: 0,
        };
    }
}

/// Mark the stream's orientation if it has none yet, as `ORIENT` does.
pub(super) unsafe fn orient(fp: *mut File, mode: i8) {
    // SAFETY: the caller's locked stream.
    let state = unsafe { state(fp) };
    if state.orientation == 0 {
        state.orientation = mode;
    }
}

/// Call the read callback, keeping `offset` in step.
pub(super) unsafe fn sread(fp: *mut File, buf: *mut u8, n: c_int) -> c_int {
    // SAFETY: the caller's locked stream, and `n` writable bytes at `buf`.
    unsafe {
        let Some(read) = (*fp).read else {
            crate::errno::set(libc::EBADF);
            return -1;
        };
        let got = read((*fp).cookie, buf.cast(), n);
        if got > 0 {
            if (*fp).flags & flag::OFF != 0 {
                (*fp).offset = (*fp).offset.saturating_add(got as i64);
            }
        } else if got < 0 {
            (*fp).flags &= !flag::OFF;
        }
        got
    }
}

/// Call the write callback, seeking to the end first in append mode.
pub(super) unsafe fn swrite(fp: *mut File, buf: *const u8, n: c_int) -> c_int {
    // SAFETY: the caller's locked stream, and `n` readable bytes at `buf`.
    unsafe {
        let Some(write) = (*fp).write else {
            crate::errno::set(libc::EBADF);
            return -1;
        };
        if (*fp).flags & flag::APP != 0 {
            // A stream that cannot seek is appended to wherever it is.
            let _ = sseek(fp, 0, libc::SEEK_END);
        }
        let put = write((*fp).cookie, buf.cast(), n);
        // Only an append just seeked to the end knows where the bytes went;
        // anything else may have been written elsewhere by `O_APPEND`.
        if put >= 0 && (*fp).flags & (flag::APP | flag::OFF) == (flag::APP | flag::OFF) {
            (*fp).offset = (*fp).offset.saturating_add(put as i64);
        } else {
            (*fp).flags &= !flag::OFF;
        }
        put
    }
}

/// Call the seek callback, recording the answer in `offset`.
pub(super) unsafe fn sseek(fp: *mut File, offset: i64, whence: c_int) -> i64 {
    // SAFETY: the caller's locked stream.
    unsafe {
        let Some(seek) = (*fp).seek else {
            crate::errno::set(libc::ESPIPE);
            return -1;
        };
        let at = seek((*fp).cookie, offset, whence);
        if at < 0 {
            (*fp).flags &= !flag::OFF;
            return -1;
        }
        (*fp).flags |= flag::OFF;
        (*fp).offset = at;
        at
    }
}

/// The buffer size and flags a stream should get, from its descriptor.
///
/// Returns `(flags, size, could_be_tty)`.
pub(super) unsafe fn whatbuf(fp: *mut File) -> (i16, c_int, bool) {
    // SAFETY: the caller's live stream.
    let fd = unsafe { (*fp).file };
    if fd < 0 {
        return (flag::NPT, BUFSIZ, false);
    }
    // SAFETY: a zeroed host `stat` for `fstat` to fill.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: a live local and the stream's descriptor.
    if unsafe { libc::fstat(fd as c_int, &mut st) } < 0 {
        return (flag::NPT, BUFSIZ, false);
    }
    let tty = st.st_mode & libc::S_IFMT == libc::S_IFCHR;
    if st.st_blksize <= 0 {
        return (flag::NPT, BUFSIZ, tty);
    }
    let size = c_int::try_from(st.st_blksize).unwrap_or(BUFSIZ);
    // SAFETY: as above.
    unsafe { (*fp).blksize = size };
    let regular = st.st_mode & libc::S_IFMT == libc::S_IFREG;
    // A descriptor stream is its own cookie; a `funopen` one over the same
    // descriptor is not, and its seeks are not known to be `lseek`.
    // SAFETY: as above.
    let own_seek = unsafe { (*fp).cookie } == fp.cast();
    match regular && own_seek {
        true => (flag::OPT, size, tty),
        false => (flag::NPT, size, tty),
    }
}

/// Give the stream a buffer, as `__smakebuf` does.
pub(super) unsafe fn makebuf(fp: *mut File) {
    // SAFETY: the caller's locked stream.
    unsafe {
        if (*fp).flags & flag::NBF != 0 {
            use_nbuf(fp);
            return;
        }
        let (mut flags, size, tty) = whatbuf(fp);
        let buf = libc::malloc(size as usize).cast::<u8>();
        if buf.is_null() {
            (*fp).flags |= flag::NBF;
            use_nbuf(fp);
            return;
        }
        super::register_cleanup();
        flags |= flag::MBF;
        (*fp).bf.base = buf;
        (*fp).p = buf;
        (*fp).bf.size = size;
        if tty && libc::isatty((*fp).file as c_int) == 1 {
            flags |= flag::LBF;
        }
        (*fp).flags |= flags;
    }
}

/// The one-byte buffer of an unbuffered stream.
pub(super) unsafe fn use_nbuf(fp: *mut File) {
    // SAFETY: the caller's locked stream.
    unsafe {
        let nbuf = (&raw mut (*fp).nbuf).cast::<u8>();
        (*fp).bf.base = nbuf;
        (*fp).p = nbuf;
        (*fp).bf.size = 1;
    }
}

/// Prepare to write, as `__swsetup` does; false with `EBADF` if the stream
/// cannot be written.
pub(super) unsafe fn swsetup(fp: *mut File) -> bool {
    // SAFETY: the caller's locked stream.
    unsafe {
        if (*fp).flags & flag::WR == 0 {
            if (*fp).flags & flag::RW == 0 {
                crate::errno::set(libc::EBADF);
                (*fp).flags |= flag::ERR;
                return false;
            }
            if (*fp).flags & flag::RD != 0 {
                if has_ub(fp) {
                    free_ub(fp);
                }
                (*fp).flags &= !(flag::RD | flag::EOF);
                (*fp).r = 0;
                (*fp).p = (*fp).bf.base;
            }
            (*fp).flags |= flag::WR;
        }
        if (*fp).bf.base.is_null() {
            makebuf(fp);
        }
        if (*fp).flags & flag::LBF != 0 {
            (*fp).w = 0;
            (*fp).lbfsize = -(*fp).bf.size;
        } else {
            (*fp).w = match (*fp).flags & flag::NBF != 0 {
                true => 0,
                false => (*fp).bf.size,
            };
        }
        true
    }
}

/// Whether a write must first go through [`swsetup`], and it failed.
pub(super) unsafe fn cant_write(fp: *mut File) -> bool {
    // SAFETY: the caller's locked stream.
    unsafe { ((*fp).flags & flag::WR == 0 || (*fp).bf.base.is_null()) && !swsetup(fp) }
}

/// Write out the buffer, as `__sflush` does. False on an error, with `ERR` set.
pub(super) unsafe fn sflush(fp: *mut File) -> bool {
    // SAFETY: the caller's locked stream; `p` lies within `bf`.
    unsafe {
        let t = (*fp).flags;
        if t & flag::WR == 0 {
            return true;
        }
        let mut p = (*fp).bf.base;
        if p.is_null() {
            return true;
        }
        let mut n = (*fp).p.offset_from(p) as c_int;
        (*fp).p = p;
        (*fp).w = match t & (flag::LBF | flag::NBF) != 0 {
            true => 0,
            false => (*fp).bf.size,
        };
        while n > 0 {
            let put = swrite(fp, p, n);
            if put <= 0 {
                // Keep what was not written, at the front of the buffer.
                if p > (*fp).p {
                    std::ptr::copy(p, (*fp).p, n as usize);
                }
                (*fp).p = (*fp).p.add(n as usize);
                if (*fp).flags & (flag::LBF | flag::NBF) == 0 {
                    (*fp).w -= n;
                }
                (*fp).flags |= flag::ERR;
                return false;
            }
            n -= put;
            p = p.add(put as usize);
        }
        true
    }
}

/// The first half of `__srefill`: everything before the read itself.
///
/// `Err(())` is end-of-file or an error. `Ok(true)` means the `ungetc` buffer
/// was popped and there is data again; `Ok(false)` means a read is due.
pub(super) unsafe fn refill_prepare(fp: *mut File) -> Result<bool, ()> {
    // SAFETY: the caller's locked stream.
    unsafe {
        (*fp).r = 0;
        if (*fp).flags & flag::EOF != 0 {
            return Err(());
        }
        if (*fp).flags & flag::RD == 0 {
            if (*fp).flags & flag::RW == 0 {
                crate::errno::set(libc::EBADF);
                (*fp).flags |= flag::ERR;
                return Err(());
            }
            if (*fp).flags & flag::WR != 0 {
                if !sflush(fp) {
                    return Err(());
                }
                (*fp).flags &= !flag::WR;
                (*fp).w = 0;
                (*fp).lbfsize = 0;
            }
            (*fp).flags |= flag::RD;
        } else if has_ub(fp) {
            free_ub(fp);
            (*fp).r = (*fp).ur;
            if (*fp).r != 0 {
                (*fp).p = state(fp).up;
                return Ok(true);
            }
        }
        if (*fp).bf.base.is_null() {
            makebuf(fp);
        }
        // Reading from a line-buffered or unbuffered stream flushes every
        // line-buffered output stream first, so a prompt appears before the
        // read that answers it.
        if (*fp).flags & (flag::LBF | flag::NBF) != 0 {
            (*fp).flags |= flag::IGN;
            flush_line_buffered();
            (*fp).flags &= !flag::IGN;
            if (*fp).flags & (flag::LBF | flag::WR) == (flag::LBF | flag::WR) {
                sflush(fp);
            }
        }
        Ok(false)
    }
}

/// Flush every line-buffered stream that is writing, skipping any another
/// thread holds rather than waiting on it.
fn flush_line_buffered() {
    for fp in super::every_stream() {
        // SAFETY: a registered stream, read before its lock is tried only for
        // the flags, which a race can at worst make this skip or flush once
        // more.
        unsafe {
            let flags = (*fp).flags;
            if flags & flag::IGN != 0 || flags & (flag::LBF | flag::WR) != (flag::LBF | flag::WR) {
                continue;
            }
            let extra = &*(*fp).extra;
            if extra.lock.try_lock() {
                sflush(fp);
                extra.lock.unlock();
            }
        }
    }
}

/// Fill the buffer, as `__srefill` does. False at end-of-file or on an error.
pub(super) unsafe fn refill(fp: *mut File) -> bool {
    // SAFETY: the caller's locked stream.
    unsafe {
        match refill_prepare(fp) {
            Err(()) => false,
            Ok(true) => true,
            Ok(false) => fill(fp),
        }
    }
}

/// The read half of `__srefill`.
unsafe fn fill(fp: *mut File) -> bool {
    // SAFETY: the caller's locked stream with a buffer.
    unsafe {
        (*fp).p = (*fp).bf.base;
        (*fp).r = sread(fp, (*fp).p, (*fp).bf.size);
        (*fp).flags &= !flag::MOD;
        if (*fp).r <= 0 {
            if (*fp).r == 0 {
                (*fp).flags |= flag::EOF;
            } else {
                (*fp).r = 0;
                (*fp).flags |= flag::ERR;
            }
            return false;
        }
        true
    }
}

/// `__srget`'s body: the next byte once the inlined `getc` found none.
pub(super) unsafe fn srget(fp: *mut File) -> c_int {
    // SAFETY: the caller's locked stream.
    unsafe {
        if refill(fp) {
            (*fp).r -= 1;
            let c = *(*fp).p;
            (*fp).p = (*fp).p.add(1);
            return c_int::from(c);
        }
        EOF
    }
}

/// `getc_unlocked`, written out.
pub(super) unsafe fn getc(fp: *mut File) -> c_int {
    // SAFETY: the caller's locked stream.
    unsafe {
        (*fp).r -= 1;
        if (*fp).r < 0 {
            return srget(fp);
        }
        let c = *(*fp).p;
        (*fp).p = (*fp).p.add(1);
        c_int::from(c)
    }
}

/// `__swbuf`'s body: store `c` once the inlined `putc` found no room.
pub(super) unsafe fn swbuf(c: c_int, fp: *mut File) -> c_int {
    // SAFETY: the caller's locked stream.
    unsafe {
        (*fp).w = (*fp).lbfsize;
        if cant_write(fp) {
            crate::errno::set(libc::EBADF);
            return EOF;
        }
        let c = c as u8;
        let mut n = (*fp).p.offset_from((*fp).bf.base) as c_int;
        if n >= (*fp).bf.size {
            if !sflush(fp) {
                return EOF;
            }
            n = 0;
        }
        (*fp).w -= 1;
        *(*fp).p = c;
        (*fp).p = (*fp).p.add(1);
        n += 1;
        if (n == (*fp).bf.size || ((*fp).flags & flag::LBF != 0 && c == b'\n')) && !sflush(fp) {
            return EOF;
        }
        c_int::from(c)
    }
}

/// `putc_unlocked`, written out.
pub(super) unsafe fn putc(c: c_int, fp: *mut File) -> c_int {
    // SAFETY: the caller's locked stream.
    unsafe {
        (*fp).w -= 1;
        if (*fp).w >= 0 || ((*fp).w >= (*fp).lbfsize && c as u8 != b'\n') {
            *(*fp).p = c as u8;
            (*fp).p = (*fp).p.add(1);
            return c_int::from(c as u8);
        }
        swbuf(c, fp)
    }
}

/// Write `len` bytes, as `__sfvwrite` does for one vector. Returns how many
/// were accepted; fewer than `len` means an error, with `ERR` set.
pub(super) unsafe fn write_all(fp: *mut File, mut p: *const u8, len: usize) -> usize {
    if len == 0 {
        return 0;
    }
    // SAFETY: the caller's locked stream, and `len` readable bytes at `p`.
    unsafe {
        if cant_write(fp) {
            crate::errno::set(libc::EBADF);
            return 0;
        }
        let total = len;
        let mut left = len;
        let chunk = |n: usize| n.min(c_int::MAX as usize) as c_int;
        if (*fp).flags & flag::NBF != 0 {
            while left > 0 {
                let put = swrite(fp, p, chunk(left).min(BUFSIZ));
                if put <= 0 {
                    (*fp).flags |= flag::ERR;
                    return total - left;
                }
                p = p.add(put as usize);
                left -= put as usize;
            }
        } else if (*fp).flags & flag::LBF == 0 {
            while left > 0 {
                let room = (*fp).w;
                let size = (*fp).bf.size;
                let taken = if (*fp).p > (*fp).bf.base && left > room as usize {
                    // Fill the buffer and flush it.
                    std::ptr::copy(p, (*fp).p, room as usize);
                    (*fp).p = (*fp).p.add(room as usize);
                    if !sflush(fp) {
                        return total - left + room as usize;
                    }
                    room as usize
                } else if left >= size as usize {
                    // Large and the buffer is empty: write past it, in whole
                    // buffers so a regular file stays block aligned.
                    let whole = chunk(left) / size * size;
                    let put = swrite(fp, p, whole);
                    if put <= 0 {
                        (*fp).flags |= flag::ERR;
                        return total - left;
                    }
                    put as usize
                } else {
                    std::ptr::copy(p, (*fp).p, left);
                    (*fp).w -= left as c_int;
                    (*fp).p = (*fp).p.add(left);
                    left
                };
                p = p.add(taken);
                left -= taken;
            }
        } else {
            // Line buffered: as above, and flush after each newline.
            let mut to_newline: Option<usize> = None;
            while left > 0 {
                let distance = *to_newline.get_or_insert_with(|| {
                    let rest = std::slice::from_raw_parts(p, left);
                    rest.iter()
                        .position(|&b| b == b'\n')
                        .map_or(left + 1, |i| i + 1)
                });
                let s = left.min(distance);
                let room = ((*fp).w + (*fp).bf.size) as usize;
                let size = (*fp).bf.size as usize;
                let taken = if (*fp).p > (*fp).bf.base && s > room {
                    std::ptr::copy(p, (*fp).p, room);
                    (*fp).p = (*fp).p.add(room);
                    if !sflush(fp) {
                        return total - left + room;
                    }
                    room
                } else if s >= size {
                    let put = swrite(fp, p, chunk(s));
                    if put <= 0 {
                        (*fp).flags |= flag::ERR;
                        return total - left;
                    }
                    put as usize
                } else {
                    std::ptr::copy(p, (*fp).p, s);
                    (*fp).w -= s as c_int;
                    (*fp).p = (*fp).p.add(s);
                    s
                };
                p = p.add(taken);
                left -= taken;
                let remaining = distance - taken;
                if remaining == 0 {
                    if !sflush(fp) {
                        return total - left;
                    }
                    to_newline = None;
                } else {
                    to_newline = Some(remaining);
                }
            }
        }
        total
    }
}

/// Read up to `len` bytes, as `__fread` does. Short means end-of-file or an
/// error, which the flags say.
pub(super) unsafe fn read_all(fp: *mut File, mut p: *mut u8, len: usize) -> usize {
    // SAFETY: the caller's locked stream, and `len` writable bytes at `p`.
    unsafe {
        if (*fp).r < 0 {
            (*fp).r = 0;
        }
        let mut left = len;
        while left > (*fp).r as usize {
            let r = (*fp).r as usize;
            std::ptr::copy_nonoverlapping((*fp).p, p, r);
            (*fp).p = (*fp).p.add(r);
            (*fp).r = 0;
            p = p.add(r);
            left -= r;
            match refill_prepare(fp) {
                Err(()) => return len - left,
                Ok(true) => continue,
                Ok(false) => {}
            }
            // A request at least a buffer long skips the buffer.
            if left >= (*fp).bf.size as usize {
                (*fp).p = (*fp).bf.base;
                let got = sread(fp, p, left.min(c_int::MAX as usize) as c_int);
                if got <= 0 {
                    match got {
                        0 => (*fp).flags |= flag::EOF,
                        _ => (*fp).flags |= flag::ERR,
                    }
                    return len - left;
                }
                p = p.add(got as usize);
                left -= got as usize;
                continue;
            }
            if !fill(fp) {
                return len - left;
            }
        }
        std::ptr::copy_nonoverlapping((*fp).p, p, left);
        (*fp).r -= left as c_int;
        (*fp).p = (*fp).p.add(left);
        len
    }
}

/// Grow the `ungetc` buffer, keeping its contents at the end. False if out
/// of memory.
unsafe fn submore(fp: *mut File) -> bool {
    // SAFETY: the caller's locked stream with an active `ungetc` buffer.
    unsafe {
        let ubuf = (&raw mut (*fp).ubuf).cast::<u8>();
        if (*fp).ub.base == ubuf {
            const FIRST: usize = 128;
            let p = libc::malloc(FIRST).cast::<u8>();
            if p.is_null() {
                return false;
            }
            (*fp).ub.base = p;
            (*fp).ub.size = FIRST as c_int;
            let held = (*fp).ubuf.len();
            let at = p.add(FIRST - held);
            std::ptr::copy_nonoverlapping(ubuf, at, held);
            (*fp).p = at;
            return true;
        }
        let size = (*fp).ub.size as usize;
        let p = libc::realloc((*fp).ub.base.cast(), size * 2).cast::<u8>();
        if p.is_null() {
            return false;
        }
        std::ptr::copy_nonoverlapping(p, p.add(size), size);
        (*fp).p = p.add(size);
        (*fp).ub.base = p;
        (*fp).ub.size = (size * 2) as c_int;
        true
    }
}

/// `ungetc`'s body.
pub(super) unsafe fn ungetc(c: c_int, fp: *mut File) -> c_int {
    if c == EOF {
        return EOF;
    }
    // SAFETY: the caller's locked stream.
    unsafe {
        if (*fp).flags & flag::RD == 0 {
            if (*fp).flags & flag::RW == 0 {
                return EOF;
            }
            if (*fp).flags & flag::WR != 0 {
                if !sflush(fp) {
                    return EOF;
                }
                (*fp).flags &= !flag::WR;
                (*fp).w = 0;
                (*fp).lbfsize = 0;
            }
            (*fp).flags |= flag::RD;
        }
        let c = c as u8;
        if has_ub(fp) {
            if (*fp).r >= (*fp).ub.size && !submore(fp) {
                return EOF;
            }
            (*fp).p = (*fp).p.sub(1);
            *(*fp).p = c;
            (*fp).r += 1;
            return c_int::from(c);
        }
        (*fp).flags &= !flag::EOF;
        // Backing up over the byte that is being pushed back costs nothing.
        if !(*fp).bf.base.is_null() && (*fp).p > (*fp).bf.base && *(*fp).p.sub(1) == c {
            (*fp).p = (*fp).p.sub(1);
            (*fp).r += 1;
            return c_int::from(c);
        }
        (*fp).ur = (*fp).r;
        state(fp).up = (*fp).p;
        let ubuf = (&raw mut (*fp).ubuf).cast::<u8>();
        (*fp).ub.base = ubuf;
        (*fp).ub.size = (*fp).ubuf.len() as c_int;
        let last = ubuf.add((*fp).ubuf.len() - 1);
        *last = c;
        (*fp).p = last;
        (*fp).r = 1;
        c_int::from(c)
    }
}

/// `ftello`'s body.
pub(super) unsafe fn tell(fp: *mut File) -> i64 {
    // SAFETY: the caller's locked stream.
    unsafe {
        if (*fp).seek.is_none() {
            crate::errno::set(libc::ESPIPE);
            return -1;
        }
        let mut pos = if (*fp).flags & flag::OFF != 0 {
            (*fp).offset
        } else {
            let at = sseek(fp, 0, libc::SEEK_CUR);
            if at < 0 {
                return -1;
            }
            at
        };
        if (*fp).flags & flag::RD != 0 {
            // Unread bytes, the pushed-back ones included, are still ahead.
            pos -= i64::from(match has_ub(fp) {
                true => (*fp).ur,
                false => (*fp).r,
            });
            if pos < 0 {
                (*fp).flags |= flag::ERR;
                crate::errno::set(libc::EIO);
                return -1;
            }
            if has_ub(fp) {
                pos -= i64::from((*fp).r);
            }
        } else if (*fp).flags & flag::WR != 0 && !(*fp).p.is_null() {
            let held = (*fp).p.offset_from((*fp).bf.base) as i64;
            // Buffered appends land at the end, wherever that now is.
            if held > 0 && ((*fp).flags & flag::APP != 0 || state(fp).opened_append) {
                let end = sseek(fp, 0, libc::SEEK_END);
                if end < 0 {
                    return -1;
                }
                pos = end;
            }
            pos += held;
        }
        if pos < 0 {
            crate::errno::set(libc::EOVERFLOW);
            return -1;
        }
        pos
    }
}

/// `fseeko`'s body.
pub(super) unsafe fn seek(fp: *mut File, offset: i64, whence: c_int) -> c_int {
    // SAFETY: the caller's locked stream.
    unsafe {
        if (*fp).seek.is_none() {
            crate::errno::set(libc::ESPIPE);
            return -1;
        }
        let (offset, whence) = match whence {
            libc::SEEK_CUR => {
                let here = tell(fp);
                if here < 0 {
                    return -1;
                }
                let Some(to) = here.checked_add(offset) else {
                    crate::errno::set(libc::EOVERFLOW);
                    return -1;
                };
                (to, libc::SEEK_SET)
            }
            libc::SEEK_SET | libc::SEEK_END => (offset, whence),
            _ => {
                crate::errno::set(libc::EINVAL);
                return -1;
            }
        };
        if whence == libc::SEEK_SET && offset < 0 {
            crate::errno::set(libc::EINVAL);
            return -1;
        }
        if !sflush(fp) || sseek(fp, offset, whence) < 0 {
            return -1;
        }
        if has_ub(fp) {
            free_ub(fp);
        }
        (*fp).p = (*fp).bf.base;
        (*fp).r = 0;
        if (*fp).flags & flag::WR != 0 && !(*fp).bf.base.is_null() {
            (*fp).w = match (*fp).flags & (flag::LBF | flag::NBF) != 0 {
                true => 0,
                false => (*fp).bf.size,
            };
        }
        (*fp).flags &= !flag::EOF;
        state(fp).mbstate = [0; 16];
        0
    }
}

/// Discard buffered data without writing it, as `fpurge` does.
pub(super) unsafe fn purge(fp: *mut File) {
    // SAFETY: the caller's locked stream.
    unsafe {
        if has_ub(fp) {
            free_ub(fp);
        }
        (*fp).p = (*fp).bf.base;
        (*fp).r = 0;
        (*fp).w = match (*fp).flags & (flag::LBF | flag::NBF | flag::RD) != 0 {
            true => 0,
            false => (*fp).bf.size,
        };
    }
}

/// Undo everything a stream holds and run its close callback, as `fclose`
/// does before the `FILE` is released. Returns `EOF` if either the flush or
/// the close failed.
pub(super) unsafe fn teardown(fp: *mut File) -> c_int {
    // SAFETY: the caller's locked stream.
    unsafe {
        let mut rc = 0;
        if (*fp).flags & flag::WR != 0 && !sflush(fp) {
            rc = EOF;
        }
        if let Some(close) = (*fp).close
            && close((*fp).cookie) < 0
        {
            rc = EOF;
        }
        reset_buffers(fp);
        (*fp).file = -1;
        (*fp).flags = 0;
        (*fp).cookie = std::ptr::null_mut::<c_void>();
        (*fp).read = None;
        (*fp).write = None;
        (*fp).seek = None;
        (*fp).close = None;
        rc
    }
}

/// Free every buffer and return the pointers to their empty state.
pub(super) unsafe fn reset_buffers(fp: *mut File) {
    // SAFETY: the caller's locked stream.
    unsafe {
        if (*fp).flags & flag::MBF != 0 {
            libc::free((*fp).bf.base.cast());
        }
        if has_ub(fp) {
            free_ub(fp);
        }
        free_lb(fp);
        (*fp).bf = super::Sbuf {
            base: std::ptr::null_mut(),
            size: 0,
        };
        (*fp).ub.size = 0;
        (*fp).p = std::ptr::null_mut();
        (*fp).r = 0;
        (*fp).w = 0;
        (*fp).lbfsize = 0;
        let state = state(fp);
        state.orientation = 0;
        state.mbstate = [0; 16];
        state.wide_line = Vec::new();
        state.opened_append = false;
    }
}
