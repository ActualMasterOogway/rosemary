//! `setsockopt`/`getsockopt`, where neither the level nor the option name nor
//! one of the values is the same number.
//!
//! `SOL_SOCKET` is `0xffff` on Darwin and `1` here - and `1` on Darwin is
//! `IPPROTO_ICMP`, so a guest setting a socket option ends up setting an ICMP
//! one. Below the level, essentially nothing lines up: Darwin numbers the
//! boolean options as single bits (`SO_REUSEADDR` is `0x0004`) and Linux numbers
//! them sequentially (`SO_REUSEADDR` is `2`, which is Darwin's `SO_ACCEPTCONN`).
//!
//! The `IP_*` and `IPV6_*` names are the same story one layer up: `IP_TTL` is 4
//! on Darwin and 2 here, where 4 is `IP_OPTIONS`.
//!
//! An option this module has not been taught is **refused** with `ENOPROTOOPT`
//! rather than passed through, because a wrong option number does not fail - it
//! sets a different, real option.

use std::ffi::c_void;

/// Darwin's `SOL_SOCKET`.
const SOL_SOCKET: i32 = 0xffff;
/// The `IPPROTO_*` levels, which do agree: they are the IANA protocol numbers.
const IPPROTO_IP: i32 = 0;
const IPPROTO_TCP: i32 = 6;
const IPPROTO_IPV6: i32 = 41;

/// Darwin's `SO_*` names, paired with the host's.
///
/// `SO_NOSIGPIPE` (`0x1022`) is absent: it has no host counterpart at all. See
/// [`nosigpipe`].
const SOCKET_OPTIONS: &[(i32, i32)] = &[
    (0x0001, libc::SO_DEBUG),
    (0x0002, libc::SO_ACCEPTCONN),
    (0x0004, libc::SO_REUSEADDR),
    (0x0008, libc::SO_KEEPALIVE),
    (0x0010, libc::SO_DONTROUTE),
    (0x0020, libc::SO_BROADCAST),
    (0x0080, libc::SO_LINGER),
    (0x0100, libc::SO_OOBINLINE),
    (0x0200, libc::SO_REUSEPORT),
    (0x0400, libc::SO_TIMESTAMP),
    (0x1001, libc::SO_SNDBUF),
    (0x1002, libc::SO_RCVBUF),
    (0x1003, libc::SO_SNDLOWAT),
    (0x1004, libc::SO_RCVLOWAT),
    (0x1005, libc::SO_SNDTIMEO),
    (0x1006, libc::SO_RCVTIMEO),
    (0x1007, libc::SO_ERROR),
    (0x1008, libc::SO_TYPE),
];

/// Darwin's `IP_*`, paired with the host's.
const IP_OPTIONS: &[(i32, i32)] = &[
    (1, libc::IP_OPTIONS),
    (2, libc::IP_HDRINCL),
    (3, libc::IP_TOS),
    (4, libc::IP_TTL),
    (5, libc::IP_RECVOPTS),
    (9, libc::IP_MULTICAST_IF),
    (10, libc::IP_MULTICAST_TTL),
    (11, libc::IP_MULTICAST_LOOP),
    (12, libc::IP_ADD_MEMBERSHIP),
    (13, libc::IP_DROP_MEMBERSHIP),
    (27, libc::IP_PKTINFO),
];

/// Darwin's `IPV6_*`, paired with the host's.
const IPV6_OPTIONS: &[(i32, i32)] = &[
    (9, libc::IPV6_MULTICAST_IF),
    (10, libc::IPV6_MULTICAST_HOPS),
    (11, libc::IPV6_MULTICAST_LOOP),
    (12, libc::IPV6_ADD_MEMBERSHIP),
    (13, libc::IPV6_DROP_MEMBERSHIP),
    (36, libc::IPV6_UNICAST_HOPS),
    (27, libc::IPV6_V6ONLY),
    (61, libc::IPV6_RECVPKTINFO),
];

/// Darwin's `TCP_*`, paired with the host's.
///
/// `TCP_NOPUSH` (4) is `TCP_CORK` here in effect but not in contract - Darwin's
/// is advisory and Linux's holds the data until it is cleared - so it is not
/// listed and a guest asking for it is refused.
const TCP_OPTIONS: &[(i32, i32)] = &[
    (0x01, libc::TCP_NODELAY),
    (0x02, libc::TCP_MAXSEG),
    (0x10, libc::TCP_KEEPIDLE),
    (0x101, libc::TCP_KEEPINTVL),
    (0x102, libc::TCP_KEEPCNT),
];

/// Darwin's `SO_NOSIGPIPE`, which this host has no option for.
///
/// Linux asks for the same thing per *call*, with `MSG_NOSIGNAL`. So the switch
/// is kept here and [`crate::net::socket`] reads it: a `send` on a socket the
/// guest set the option on is sent with `MSG_NOSIGNAL`, and the guest gets
/// `EPIPE` instead of a signal, which is what it asked for.
///
/// The gap, and it is a real one: `write` is not a socket call and takes no
/// flags, so writing to a closed socket still raises `SIGPIPE` however this
/// option was set. Nothing can be done about that short of changing the
/// process-wide disposition, which is not one socket's to change.
///
/// Refusing instead - which this module used to do, `ENOPROTOOPT` being what a
/// kernel without the option would say - turned out not to be survivable:
/// Crashpad `CHECK`s the return value and aborts the process, so the honest
/// failure was a crash before the program had started.
const NOSIGPIPE: i32 = 0x1022;

fn nosigpipe(level: i32, name: i32) -> bool {
    level == SOL_SOCKET && name == NOSIGPIPE
}

/// The descriptors `SO_NOSIGPIPE` is set on.
///
/// Entries are cleared when a descriptor number is *issued* rather than when
/// one is closed, because `close` is not this crate's call to see - and a stale
/// entry would otherwise suppress `SIGPIPE` on whatever socket inherited the
/// number.
static SUPPRESSED: std::sync::Mutex<Option<std::collections::HashSet<i32>>> =
    std::sync::Mutex::new(None);

fn with_suppressed<R>(f: impl FnOnce(&mut std::collections::HashSet<i32>) -> R) -> R {
    let mut guard = SUPPRESSED.lock().unwrap_or_else(|e| e.into_inner());
    f(guard.get_or_insert_with(Default::default))
}

/// Whether `fd` was given `SO_NOSIGPIPE`, and so wants `MSG_NOSIGNAL`.
pub fn suppresses_sigpipe(fd: i32) -> bool {
    with_suppressed(|held| held.contains(&fd))
}

/// Forget any option set on `fd`, because it names a new socket now.
pub fn forget(fd: i32) {
    if fd >= 0 {
        with_suppressed(|held| held.remove(&fd));
    }
}

/// # Safety
///
/// `value` is `len` readable bytes, or is null with `len` zero.
unsafe fn set_nosigpipe(fd: i32, value: *const c_void, len: u32) -> i32 {
    if value.is_null() || (len as usize) < core::mem::size_of::<i32>() {
        crate::errno::set(libc::EINVAL);
        return -1;
    }
    // SAFETY: at least an `int` of the caller's, just checked.
    let wanted = unsafe { value.cast::<i32>().read_unaligned() } != 0;
    with_suppressed(|held| match wanted {
        true => held.insert(fd),
        false => held.remove(&fd),
    });
    0
}

/// # Safety
///
/// `value` is `*len` writable bytes and `len` is a writable `socklen_t`.
unsafe fn get_nosigpipe(fd: i32, value: *mut c_void, len: *mut u32) -> i32 {
    if value.is_null() || len.is_null() {
        crate::errno::set(libc::EFAULT);
        return -1;
    }
    // SAFETY: the caller's length slot.
    if (unsafe { len.read() } as usize) < core::mem::size_of::<i32>() {
        crate::errno::set(libc::EINVAL);
        return -1;
    }
    // SAFETY: at least an `int` of the caller's, just checked.
    unsafe {
        value
            .cast::<i32>()
            .write_unaligned(suppresses_sigpipe(fd) as i32);
        len.write(core::mem::size_of::<i32>() as u32);
    }
    0
}

/// `(level, name)` in the host's numbering, or `None` for a pair this module has
/// not been taught.
fn option_to_host(level: i32, name: i32) -> Option<(i32, i32)> {
    let (host_level, table) = match level {
        SOL_SOCKET => (libc::SOL_SOCKET, SOCKET_OPTIONS),
        IPPROTO_IP => (IPPROTO_IP, IP_OPTIONS),
        IPPROTO_IPV6 => (IPPROTO_IPV6, IPV6_OPTIONS),
        IPPROTO_TCP => (IPPROTO_TCP, TCP_OPTIONS),
        _ => return None,
    };
    let host_name = table.iter().find(|(theirs, _)| *theirs == name)?.1;
    Some((host_level, host_name))
}

/// Whether an option's *value* is a `struct timeval`, whose two platforms differ
/// in the width of `tv_usec`.
///
/// Darwin's `suseconds_t` is a 32-bit `int` followed by four bytes of padding;
/// this host's is a 64-bit `long`. The bytes are the same length and not the
/// same value: a guest asking for a 500 ms timeout is asking here for one of
/// 500000 seconds plus whatever the padding held.
fn is_timeval(level: i32, name: i32) -> bool {
    level == SOL_SOCKET && (name == 0x1005 || name == 0x1006)
}

/// A guest's `struct timeval` as this host's.
///
/// # Safety
///
/// `value` is at least sixteen readable bytes.
unsafe fn timeval_to_host(value: *const c_void) -> libc::timeval {
    // SAFETY: the caller's guarantee; both fields are read at Darwin's offsets.
    unsafe {
        libc::timeval {
            tv_sec: value.cast::<i64>().read_unaligned(),
            tv_usec: i64::from(value.cast::<u8>().add(8).cast::<i32>().read_unaligned()),
        }
    }
}

/// `int setsockopt(int, int, int, const void *, socklen_t)`
///
/// # Safety
///
/// `value` is `len` readable bytes, or is null with `len` zero.
unsafe extern "C" fn setsockopt(
    fd: i32,
    level: i32,
    name: i32,
    value: *const c_void,
    len: u32,
) -> i32 {
    if nosigpipe(level, name) {
        // SAFETY: the caller's buffer, with the length they gave for it.
        return unsafe { set_nosigpipe(fd, value, len) };
    }
    let Some((level, name)) = option_to_host(level, name) else {
        crate::errno::set(libc::ENOPROTOOPT);
        return -1;
    };
    if level == libc::SOL_SOCKET && (name == libc::SO_SNDTIMEO || name == libc::SO_RCVTIMEO) {
        if value.is_null() || (len as usize) < 16 {
            crate::errno::set(libc::EINVAL);
            return -1;
        }
        // SAFETY: sixteen readable bytes, just checked.
        let host = unsafe { timeval_to_host(value) };
        // SAFETY: a `timeval` this function owns, with its own size.
        return unsafe {
            libc::setsockopt(
                fd,
                level,
                name,
                (&raw const host).cast(),
                core::mem::size_of::<libc::timeval>() as u32,
            )
        };
    }
    // SAFETY: the caller's buffer, with the length they gave for it. Every
    // remaining option's value is an `int` or a `struct` that agrees on both.
    unsafe { libc::setsockopt(fd, level, name, value, len) }
}

/// `int getsockopt(int, int, int, void *, socklen_t *)`
///
/// # Safety
///
/// `value` is `*len` writable bytes and `len` is a writable `socklen_t`.
unsafe extern "C" fn getsockopt(
    fd: i32,
    level: i32,
    name: i32,
    value: *mut c_void,
    len: *mut u32,
) -> i32 {
    if nosigpipe(level, name) {
        // SAFETY: the caller's buffer and length slot.
        return unsafe { get_nosigpipe(fd, value, len) };
    }
    let Some((host_level, host_name)) = option_to_host(level, name) else {
        crate::errno::set(libc::ENOPROTOOPT);
        return -1;
    };
    if is_timeval(level, name) {
        if value.is_null() || len.is_null() {
            crate::errno::set(libc::EFAULT);
            return -1;
        }
        let mut host = libc::timeval {
            tv_sec: 0,
            tv_usec: 0,
        };
        let mut host_len = core::mem::size_of::<libc::timeval>() as u32;
        // SAFETY: a `timeval` this function owns, with its own length beside it.
        let rc = unsafe {
            libc::getsockopt(
                fd,
                host_level,
                host_name,
                (&raw mut host).cast(),
                &mut host_len,
            )
        };
        if rc == 0 {
            // SAFETY: the caller's buffer, which must be Darwin's sixteen bytes.
            unsafe {
                if (*len as usize) < 16 {
                    crate::errno::set(libc::EINVAL);
                    return -1;
                }
                value.cast::<i64>().write_unaligned(host.tv_sec);
                value
                    .cast::<u8>()
                    .add(8)
                    .cast::<i32>()
                    .write_unaligned(host.tv_usec as i32);
                value.cast::<u8>().add(12).cast::<i32>().write_unaligned(0);
                *len = 16;
            }
        }
        return rc;
    }
    // SAFETY: the caller's buffer and length slot.
    unsafe { libc::getsockopt(fd, host_level, host_name, value, len) }
}

pub const NAMES: &[&str] = &["setsockopt", "getsockopt"];

pub fn symbol(name: &str) -> Option<u64> {
    let f: *const () = match name {
        "setsockopt" => setsockopt as *const (),
        "getsockopt" => getsockopt as *const (),
        _ => return None,
    };
    Some(f as usize as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The level itself collides: Darwin's `SOL_SOCKET` passed through names an
    /// IP protocol here, and this host's names one on Darwin.
    #[test]
    fn the_level_is_translated_rather_than_passed_through() {
        assert_ne!(SOL_SOCKET, libc::SOL_SOCKET);
        assert_eq!(
            option_to_host(SOL_SOCKET, 0x0004),
            Some((libc::SOL_SOCKET, libc::SO_REUSEADDR))
        );
    }

    /// Darwin's `SO_REUSEADDR` is this host's `SO_ERROR` - a real option, so
    /// passing it through succeeds and reads or writes something else.
    #[test]
    fn a_boolean_option_is_not_the_option_the_guest_named() {
        assert_eq!(0x0004, libc::SO_ERROR);
        assert_eq!(0x0008, libc::SO_RCVBUF, "Darwin's SO_KEEPALIVE");
    }

    #[test]
    fn an_option_this_module_has_not_been_taught_is_refused() {
        assert_eq!(option_to_host(SOL_SOCKET, 0x4242), None);
        assert_eq!(option_to_host(1234, 1), None);
    }

    /// Through the real stack: `SO_REUSEADDR` set in Darwin's spelling comes
    /// back set, in Darwin's spelling.
    #[test]
    fn a_socket_option_set_in_darwins_numbering_reads_back_set() {
        // SAFETY: a TCP socket of this host's own making.
        let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0) };
        assert!(fd >= 0);

        let on: i32 = 1;
        // SAFETY: four readable bytes, and the length says four.
        let rc = unsafe { setsockopt(fd, SOL_SOCKET, 0x0004, (&raw const on).cast(), 4) };
        assert_eq!(rc, 0, "SO_REUSEADDR in Darwin's numbering");

        let mut back: i32 = 0;
        let mut len = 4u32;
        // SAFETY: four writable bytes and a live length.
        let rc = unsafe { getsockopt(fd, SOL_SOCKET, 0x0004, (&raw mut back).cast(), &mut len) };
        assert_eq!(rc, 0);
        assert_ne!(back, 0, "the option the guest set is the one that is on");

        // SAFETY: a descriptor this test opened and no longer uses.
        unsafe { libc::close(fd) };
    }

    /// The timeout's `tv_usec` is thirty-two bits wide on Darwin and sixty-four
    /// here, so the value has to be rebuilt rather than handed along.
    #[test]
    fn a_timeout_survives_the_narrower_microseconds_field() {
        // SAFETY: a UDP socket of this host's own making.
        let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0) };
        assert!(fd >= 0);

        // Darwin's sixteen bytes: eight of seconds, four of microseconds, four
        // of padding left deliberately non-zero.
        let mut guest = [0u8; 16];
        guest[0..8].copy_from_slice(&2i64.to_le_bytes());
        guest[8..12].copy_from_slice(&500_000i32.to_le_bytes());
        guest[12..16].copy_from_slice(&(-1i32).to_le_bytes());

        // SAFETY: sixteen readable bytes, and the length says sixteen.
        let rc = unsafe { setsockopt(fd, SOL_SOCKET, 0x1006, guest.as_ptr().cast(), 16) };
        assert_eq!(rc, 0, "SO_RCVTIMEO in Darwin's numbering and shape");

        let mut back = [0u8; 16];
        let mut len = 16u32;
        // SAFETY: sixteen writable bytes and a live length.
        let rc = unsafe { getsockopt(fd, SOL_SOCKET, 0x1006, back.as_mut_ptr().cast(), &mut len) };
        assert_eq!(rc, 0);
        assert_eq!(i64::from_le_bytes(back[0..8].try_into().unwrap()), 2);
        assert_eq!(i32::from_le_bytes(back[8..12].try_into().unwrap()), 500_000);

        // SAFETY: a descriptor this test opened and no longer uses.
        unsafe { libc::close(fd) };
    }

    /// `SO_NOSIGPIPE` is honoured per send rather than refused, and a
    /// descriptor number reused by a new socket does not inherit it.
    #[test]
    fn nosigpipe_is_recorded_read_back_and_forgotten_when_the_number_is_reused() {
        // SAFETY: a plain socket call with constant arguments.
        let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
        assert!(fd >= 0);

        let on = 1i32;
        // SAFETY: one readable `int`, and the length says four.
        let rc = unsafe {
            setsockopt(fd, SOL_SOCKET, NOSIGPIPE, (&raw const on).cast(), 4)
        };
        assert_eq!(rc, 0, "refusing this aborts Crashpad before it starts");
        assert!(suppresses_sigpipe(fd));

        let mut back = 0i32;
        let mut len = 4u32;
        // SAFETY: one writable `int` and a live length.
        let rc = unsafe {
            getsockopt(fd, SOL_SOCKET, NOSIGPIPE, (&raw mut back).cast(), &mut len)
        };
        assert_eq!((rc, back, len), (0, 1, 4));

        forget(fd);
        assert!(!suppresses_sigpipe(fd));
        // SAFETY: a descriptor this test opened and no longer uses.
        unsafe { libc::close(fd) };
    }
}
