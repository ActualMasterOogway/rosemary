//! `select` and `pselect`, whose `fd_set` is the same 128 bytes either way.
//!
//! Denied here for a long time on the strength of the declaration: an `fd_set`
//! is `__int32_t fds_bits[32]` on Darwin and `long fds_bits[16]` here, so the
//! word width differs. The *byte* layout does not. Both number the descriptors
//! from the least significant bit of the first word up, and on a little-endian
//! machine that puts descriptor `n` in bit `n % 8` of byte `n / 8` in both. Two
//! structures with different fields and the same 128 bytes, which is the
//! opposite of this crate's usual problem - so both sets pass straight through,
//! and the assertion in the tests below is what keeps that true.
//!
//! What does *not* pass through is the timeout. `select` takes a
//! `struct timeval`, whose `tv_usec` is a 32-bit `int` on Darwin and a 64-bit
//! `long` here, so a guest asking for 500ms passed through asks for 500000
//! seconds - see [`crate::net::sockopt`], which converts the same value for
//! `SO_RCVTIMEO`. The guest's own `timeval` is left alone afterwards, because
//! Darwin's `select` does not report the time remaining and this host's does:
//! writing back what the host wrote would be a change a guest never asked for.
//!
//! `pselect`'s signal mask is Darwin's 32-bit `sigset_t`, numbered Darwin's way
//! - `SIGBUS` and `SIGINFO` are not the same signals here - so it goes through
//! the same conversion `sigprocmask` does.

#![cfg(not(target_os = "macos"))]

use std::ffi::{c_int, c_void};

use machish::resolve::SyntheticDylib;

/// `int select(int, fd_set *, fd_set *, fd_set *, struct timeval *)`
///
/// # Safety
///
/// Each of `readfds`, `writefds` and `errorfds` is a writable `fd_set` or null,
/// and `timeout` is a readable Darwin `struct timeval` or null.
unsafe extern "C" fn select(
    nfds: c_int,
    readfds: *mut c_void,
    writefds: *mut c_void,
    errorfds: *mut c_void,
    timeout: *const c_void,
) -> c_int {
    // SAFETY: the caller's guarantee; sixteen readable bytes when non-null.
    let mut host = match timeout.is_null() {
        true => None,
        false => Some(unsafe { crate::net::sockopt::timeval_to_host(timeout) }),
    };
    let timeout = match &mut host {
        Some(t) => &raw mut *t,
        None => core::ptr::null_mut(),
    };
    // SAFETY: the caller's sets, whose bytes are this host's, and a `timeval`
    // this function owns - which is what keeps the host's rewrite of the time
    // remaining off the guest's own.
    unsafe {
        libc::select(
            nfds,
            readfds.cast(),
            writefds.cast(),
            errorfds.cast(),
            timeout,
        )
    }
}

/// `int pselect(int, fd_set *, fd_set *, fd_set *, const struct timespec *,
/// const sigset_t *)`
///
/// # Safety
///
/// As [`select`], with `timeout` a `struct timespec` - the same two 64-bit
/// words on both platforms - and `sigmask` a readable Darwin `sigset_t`, which
/// is one 32-bit word.
unsafe extern "C" fn pselect(
    nfds: c_int,
    readfds: *mut c_void,
    writefds: *mut c_void,
    errorfds: *mut c_void,
    timeout: *const libc::timespec,
    sigmask: *const u32,
) -> c_int {
    // SAFETY: the caller's guarantee; one readable 32-bit word when non-null.
    let mut mask = match sigmask.is_null() {
        true => None,
        false => Some(crate::signal::entry::mask_to_host(unsafe {
            sigmask.read_unaligned()
        })),
    };
    let mask = match &mut mask {
        Some(m) => &raw const *m,
        None => core::ptr::null(),
    };
    // SAFETY: the caller's sets and timeout, and a `sigset_t` this function
    // owns.
    unsafe {
        libc::pselect(
            nfds,
            readfds.cast(),
            writefds.cast(),
            errorfds.cast(),
            timeout,
            mask,
        )
    }
}

pub const NAMES: &[&str] = &["select", "pselect"];

pub fn symbol(name: &str) -> Option<u64> {
    let f: *const () = match name {
        "select" => select as *const (),
        "pselect" => pselect as *const (),
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

    /// What makes the sets a passthrough: two different declarations over the
    /// same bytes, with the descriptor in the same bit of the same byte.
    #[test]
    fn an_fd_set_is_the_same_bytes_on_both_platforms() {
        assert_eq!(core::mem::size_of::<libc::fd_set>(), 128);
        assert!(cfg!(target_endian = "little"), "the claim rests on this");

        // Descriptor 33 is bit 1 of the second 32-bit word on Darwin and bit 33
        // of the first 64-bit word here, which is byte 4, bit 1 in both.
        // SAFETY: a live `fd_set`, zeroed before anything is set in it.
        let mut host: libc::fd_set = unsafe { core::mem::zeroed() };
        // SAFETY: a live `fd_set` and a descriptor number within `FD_SETSIZE`.
        unsafe { libc::FD_SET(33, &mut host) };
        // SAFETY: reading the same 128 bytes as an array of bytes, which is what
        // a guest's `fd_set` is.
        let bytes = unsafe { core::slice::from_raw_parts((&raw const host).cast::<u8>(), 128) };
        assert_eq!(bytes[4], 0b10);
        assert!(bytes.iter().enumerate().all(|(i, b)| i == 4 || *b == 0));
    }

    /// A pipe with a byte in it is readable, through the entry point a guest is
    /// handed and with a timeout in the guest's own sixteen-byte shape.
    #[test]
    fn a_readable_pipe_is_reported_and_the_guests_timeout_is_not_rewritten() {
        let mut ends = [0 as c_int; 2];
        // SAFETY: a two-element array, which is what `pipe` fills in.
        assert_eq!(unsafe { libc::pipe(ends.as_mut_ptr()) }, 0);
        // SAFETY: the write end of a pipe this test owns.
        assert_eq!(unsafe { libc::write(ends[1], c"x".as_ptr().cast(), 1) }, 1);

        // SAFETY: a live `fd_set`, zeroed and then given one descriptor.
        let mut readfds: libc::fd_set = unsafe { core::mem::zeroed() };
        // SAFETY: as above.
        unsafe { libc::FD_SET(ends[0], &mut readfds) };

        // Darwin's `struct timeval`: eight bytes of seconds, four of
        // microseconds, four of padding left deliberately non-zero.
        let mut guest = [0u8; 16];
        guest[8..12].copy_from_slice(&500_000i32.to_le_bytes());
        guest[12..16].copy_from_slice(&(-1i32).to_le_bytes());

        // SAFETY: a live set and sixteen readable bytes of timeout.
        let ready = unsafe {
            select(
                ends[0] + 1,
                (&raw mut readfds).cast(),
                core::ptr::null_mut(),
                core::ptr::null_mut(),
                guest.as_ptr().cast(),
            )
        };
        assert_eq!(ready, 1);
        // SAFETY: a live set and a descriptor within `FD_SETSIZE`.
        assert!(unsafe { libc::FD_ISSET(ends[0], &readfds) });
        assert_eq!(
            i32::from_le_bytes(guest[8..12].try_into().unwrap()),
            500_000,
            "Darwin's select leaves the caller's timeout alone"
        );

        // SAFETY: two descriptors this test opened and no longer uses.
        unsafe {
            libc::close(ends[0]);
            libc::close(ends[1]);
        }
    }

    /// The half of `select` that is not a passthrough: a guest's `tv_usec` is
    /// half a word, so a timeout handed across unconverted is a wait of days.
    #[test]
    fn a_timeout_that_expires_expires_in_the_guests_units() {
        // SAFETY: a live `fd_set` with nothing in it, which is a sleep.
        let empty: libc::fd_set = unsafe { core::mem::zeroed() };
        let mut guest = [0u8; 16];
        guest[8..12].copy_from_slice(&1_000i32.to_le_bytes());

        let started = std::time::Instant::now();
        // SAFETY: nothing is being waited on and the timeout is readable.
        let ready = unsafe {
            select(
                0,
                core::ptr::null_mut(),
                core::ptr::null_mut(),
                core::ptr::null_mut(),
                guest.as_ptr().cast(),
            )
        };
        assert_eq!(ready, 0, "nothing became ready");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "a millisecond read as a thousand seconds"
        );
        let _ = empty;
    }

    #[test]
    fn every_listed_name_resolves() {
        for name in NAMES {
            assert!(symbol(name).is_some(), "{name}");
        }
    }
}
