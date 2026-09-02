//! The socket calls that carry an address, a family or a `MSG_*` flag.
//!
//! `listen`, `shutdown` and `close` are absent on purpose: their arguments are
//! plain integers that agree on both platforms, so the host's own are the
//! answer and [`crate::passthrough`] claims them.
//!
//! The `MSG_*` flags are the collision class again. `MSG_EOR` is `0x8` on Darwin
//! and `MSG_CTRUNC` here; `MSG_WAITALL` is `0x40` on Darwin and `MSG_DONTWAIT`
//! here. A `recv` asking to wait for the whole buffer, passed through, asks not
//! to wait at all - and gets a short read that looks like a slow network.

use std::ffi::c_void;

use super::addr::{self, HostAddr};

/// Darwin's `MSG_*`, from `<sys/socket.h>`.
mod msg {
    pub const OOB: i32 = 0x1;
    pub const PEEK: i32 = 0x2;
    pub const DONTROUTE: i32 = 0x4;
    pub const EOR: i32 = 0x8;
    pub const TRUNC: i32 = 0x10;
    pub const CTRUNC: i32 = 0x20;
    pub const WAITALL: i32 = 0x40;
    pub const DONTWAIT: i32 = 0x80;
    pub const NOSIGNAL: i32 = 0x80000;
}

/// Each flag that means the same thing on both, as `(darwin, host)`.
///
/// Darwin's `MSG_EOF` (`0x100`) has no host counterpart and is dropped: it asks
/// for a half-close after the send, which here is a separate `shutdown`.
const FLAGS: &[(i32, i32)] = &[
    (msg::OOB, libc::MSG_OOB),
    (msg::PEEK, libc::MSG_PEEK),
    (msg::DONTROUTE, libc::MSG_DONTROUTE),
    (msg::EOR, libc::MSG_EOR),
    (msg::TRUNC, libc::MSG_TRUNC),
    (msg::CTRUNC, libc::MSG_CTRUNC),
    (msg::WAITALL, libc::MSG_WAITALL),
    (msg::DONTWAIT, libc::MSG_DONTWAIT),
    (msg::NOSIGNAL, libc::MSG_NOSIGNAL),
];

fn flags_to_host(darwin: i32) -> i32 {
    let mut out = 0;
    for (theirs, ours) in FLAGS {
        if darwin & theirs != 0 {
            out |= ours;
        }
    }
    out
}

fn flags_from_host(host: i32) -> i32 {
    let mut out = 0;
    for (theirs, ours) in FLAGS {
        if host & ours != 0 {
            out |= theirs;
        }
    }
    out
}

/// `int socket(int domain, int type, int protocol)`
///
/// The `SOCK_*` types and the `IPPROTO_*` numbers agree; only the domain does
/// not. Darwin has no `SOCK_NONBLOCK`/`SOCK_CLOEXEC` bits, so `type` carries
/// nothing this host would misread.
unsafe extern "C" fn socket(domain: i32, kind: i32, protocol: i32) -> i32 {
    let Some(domain) = addr::family_to_host(domain) else {
        crate::errno::set(libc::EAFNOSUPPORT);
        return -1;
    };
    // SAFETY: three integers; the call touches no caller memory.
    let fd = unsafe { libc::socket(domain, kind, protocol) };
    super::sockopt::forget(fd);
    fd
}

/// `int socketpair(int domain, int type, int protocol, int sv[2])`
///
/// # Safety
///
/// `sv` is two writable `int`s.
unsafe extern "C" fn socketpair(domain: i32, kind: i32, protocol: i32, sv: *mut i32) -> i32 {
    let Some(domain) = addr::family_to_host(domain) else {
        crate::errno::set(libc::EAFNOSUPPORT);
        return -1;
    };
    // SAFETY: the caller's two-element array.
    let rc = unsafe { libc::socketpair(domain, kind, protocol, sv) };
    if rc == 0 && !sv.is_null() {
        // SAFETY: two `int`s the call just filled in.
        unsafe {
            super::sockopt::forget(sv.read());
            super::sockopt::forget(sv.add(1).read());
        }
    }
    rc
}

/// The shape `bind` and `connect` share: an address in, nothing out.
///
/// # Safety
///
/// `address` is `len` readable bytes.
unsafe fn with_address(
    fd: i32,
    address: *const c_void,
    len: u32,
    call: unsafe extern "C" fn(i32, *const libc::sockaddr, libc::socklen_t) -> i32,
) -> i32 {
    // SAFETY: the caller's guarantee.
    let Some(host) = (unsafe { addr::to_host(address, len) }) else {
        crate::errno::set(libc::EAFNOSUPPORT);
        return -1;
    };
    // SAFETY: an address this function owns, with the length it was built with.
    unsafe { call(fd, host.as_ptr(), host.len()) }
}

/// `int bind(int, const struct sockaddr *, socklen_t)`
///
/// # Safety
///
/// `address` is `len` readable bytes.
unsafe extern "C" fn bind(fd: i32, address: *const c_void, len: u32) -> i32 {
    // SAFETY: the caller's guarantee, passed along.
    unsafe { with_address(fd, address, len, libc::bind) }
}

/// `int connect(int, const struct sockaddr *, socklen_t)`
///
/// # Safety
///
/// As [`bind`].
unsafe extern "C" fn connect(fd: i32, address: *const c_void, len: u32) -> i32 {
    // SAFETY: the caller's guarantee, passed along.
    unsafe { with_address(fd, address, len, libc::connect) }
}

/// The shape `accept`, `getsockname` and `getpeername` share: an address out.
///
/// # Safety
///
/// `address` is `*len` writable bytes and `len` a writable `socklen_t`, or both
/// are null.
unsafe fn returning_address(
    fd: i32,
    address: *mut c_void,
    len: *mut u32,
    call: unsafe extern "C" fn(i32, *mut libc::sockaddr, *mut libc::socklen_t) -> i32,
) -> i32 {
    if address.is_null() || len.is_null() {
        // A caller that does not want the address still wants the call.
        // SAFETY: null for both is what the host reads as "no address wanted".
        return unsafe { call(fd, core::ptr::null_mut(), core::ptr::null_mut()) };
    }
    let mut host = HostAddr::empty();
    // SAFETY: a buffer this function owns, with its own length beside it.
    let rc = unsafe { call(fd, host.as_mut_ptr(), host.len_mut()) };
    if rc < 0 {
        return rc;
    }
    // SAFETY: the caller's buffer and length slot.
    unsafe { addr::from_host(&host, address, len) };
    rc
}

/// `int accept(int, struct sockaddr *, socklen_t *)`
///
/// # Safety
///
/// As [`returning_address`].
unsafe extern "C" fn accept(fd: i32, address: *mut c_void, len: *mut u32) -> i32 {
    // SAFETY: the caller's guarantee, passed along.
    let accepted = unsafe { returning_address(fd, address, len, libc::accept) };
    super::sockopt::forget(accepted);
    accepted
}

/// `int getsockname(int, struct sockaddr *, socklen_t *)`
///
/// # Safety
///
/// As [`returning_address`].
unsafe extern "C" fn getsockname(fd: i32, address: *mut c_void, len: *mut u32) -> i32 {
    // SAFETY: the caller's guarantee, passed along.
    unsafe { returning_address(fd, address, len, libc::getsockname) }
}

/// `int getpeername(int, struct sockaddr *, socklen_t *)`
///
/// # Safety
///
/// As [`returning_address`].
unsafe extern "C" fn getpeername(fd: i32, address: *mut c_void, len: *mut u32) -> i32 {
    // SAFETY: the caller's guarantee, passed along.
    unsafe { returning_address(fd, address, len, libc::getpeername) }
}

/// `ssize_t send(int, const void *, size_t, int)`
///
/// # Safety
///
/// `buf` is `len` readable bytes.
unsafe extern "C" fn send(fd: i32, buf: *const c_void, len: usize, flags: i32) -> isize {
    // SAFETY: the caller's buffer, with the flags translated.
    unsafe { libc::send(fd, buf, len, sending_flags(fd, flags)) }
}

/// `flags` for a send on `fd`, with `MSG_NOSIGNAL` added when the guest asked
/// for `SO_NOSIGPIPE` on it. See [`super::sockopt::suppresses_sigpipe`].
fn sending_flags(fd: i32, darwin: i32) -> i32 {
    let mut host = flags_to_host(darwin);
    if super::sockopt::suppresses_sigpipe(fd) {
        host |= libc::MSG_NOSIGNAL;
    }
    host
}

/// `ssize_t recv(int, void *, size_t, int)`
///
/// # Safety
///
/// `buf` is `len` writable bytes.
unsafe extern "C" fn recv(fd: i32, buf: *mut c_void, len: usize, flags: i32) -> isize {
    // SAFETY: the caller's buffer, with the flags translated.
    unsafe { libc::recv(fd, buf, len, flags_to_host(flags)) }
}

/// `ssize_t sendto(int, const void *, size_t, int, const struct sockaddr *, socklen_t)`
///
/// # Safety
///
/// `buf` is `len` readable bytes and `address` is `addrlen` readable bytes, or
/// `address` is null.
unsafe extern "C" fn sendto(
    fd: i32,
    buf: *const c_void,
    len: usize,
    flags: i32,
    address: *const c_void,
    addrlen: u32,
) -> isize {
    let flags = sending_flags(fd, flags);
    if address.is_null() {
        // SAFETY: a connected socket, which is what a null address means.
        return unsafe { libc::sendto(fd, buf, len, flags, core::ptr::null(), 0) };
    }
    // SAFETY: the caller's guarantee.
    let Some(host) = (unsafe { addr::to_host(address, addrlen) }) else {
        crate::errno::set(libc::EAFNOSUPPORT);
        return -1;
    };
    // SAFETY: the caller's buffer and an address this function owns.
    unsafe { libc::sendto(fd, buf, len, flags, host.as_ptr(), host.len()) }
}

/// `ssize_t recvfrom(int, void *, size_t, int, struct sockaddr *, socklen_t *)`
///
/// # Safety
///
/// `buf` is `len` writable bytes; `address` is `*len` writable bytes with `len`
/// a writable `socklen_t`, or both are null.
unsafe extern "C" fn recvfrom(
    fd: i32,
    buf: *mut c_void,
    len: usize,
    flags: i32,
    address: *mut c_void,
    addrlen: *mut u32,
) -> isize {
    let flags = flags_to_host(flags);
    if address.is_null() || addrlen.is_null() {
        // SAFETY: the caller's buffer; null for both is "the address is not
        // wanted", which is what the host reads it as too.
        return unsafe {
            libc::recvfrom(
                fd,
                buf,
                len,
                flags,
                core::ptr::null_mut(),
                core::ptr::null_mut(),
            )
        };
    }
    let mut host = HostAddr::empty();
    // SAFETY: the caller's buffer, and an address buffer this function owns.
    let rc = unsafe { libc::recvfrom(fd, buf, len, flags, host.as_mut_ptr(), host.len_mut()) };
    if rc >= 0 {
        // SAFETY: the caller's address buffer and length slot.
        unsafe { addr::from_host(&host, address, addrlen) };
    }
    rc
}

/// `struct msghdr` as *Darwin* lays it out: forty-eight bytes, with `msg_iovlen`
/// and `msg_controllen` as 32-bit values where this host has 64-bit ones.
///
/// Not the host's, which is fifty-six bytes. Passing one along would read the
/// iovec count out of the count *and* the padding after it, which is a length of
/// several billion.
#[repr(C)]
#[derive(Clone, Copy)]
struct MsgHdr {
    name: *mut c_void,
    namelen: u32,
    iov: *mut libc::iovec,
    iovlen: i32,
    control: *mut c_void,
    controllen: u32,
    flags: i32,
}

/// Say once that a control message was refused, and why.
fn say_control_refused() {
    static SAID: std::sync::Once = std::sync::Once::new();
    SAID.call_once(|| {
        eprintln!(
            "rosetta3: sendmsg/recvmsg with ancillary data: refused - a struct cmsghdr is \
             twelve bytes on Darwin and sixteen here, and the SCM_ types are different \
             numbers, so the payload cannot be handed across unread"
        );
    });
}

/// The two halves `sendmsg` and `recvmsg` share: a guest `msghdr` as the host's.
///
/// `None` when the message carries ancillary data, which is refused rather than
/// forwarded: `struct cmsghdr` differs in width and the `SCM_*` types differ in
/// value, so the control buffer would have to be parsed and rebuilt rather than
/// passed. A guest passing a descriptor over a socket gets `EOPNOTSUPP` and a
/// line on stderr instead of a descriptor number read out of a length field.
///
/// # Safety
///
/// `guest` is a readable Darwin `struct msghdr`.
unsafe fn msghdr_to_host(
    guest: *const MsgHdr,
    name: &mut Option<HostAddr>,
) -> Option<libc::msghdr> {
    // SAFETY: the caller's guarantee.
    let g = unsafe { guest.read() };
    if !g.control.is_null() && g.controllen != 0 {
        say_control_refused();
        return None;
    }
    // SAFETY: as above; a null name is a message with no address.
    *name = match g.name.is_null() {
        true => None,
        false => Some(unsafe { addr::to_host(g.name, g.namelen) }?),
    };
    // SAFETY: zeroed is a valid `struct msghdr` - no name, no iovec, no
    // control - and every field that matters is written below.
    let mut host: libc::msghdr = unsafe { core::mem::zeroed() };
    host.msg_name = match name {
        Some(a) => a.as_mut_ptr().cast(),
        None => core::ptr::null_mut(),
    };
    host.msg_namelen = name.as_ref().map_or(0, |a| a.len());
    host.msg_iov = g.iov;
    host.msg_iovlen = g.iovlen.max(0) as _;
    host.msg_flags = flags_to_host(g.flags);
    Some(host)
}

/// `ssize_t sendmsg(int, const struct msghdr *, int)`
///
/// # Safety
///
/// `message` is a readable Darwin `struct msghdr` whose `msg_iov` is
/// `msg_iovlen` readable `struct iovec`.
unsafe extern "C" fn sendmsg(fd: i32, message: *const c_void, flags: i32) -> isize {
    if message.is_null() {
        crate::errno::set(libc::EFAULT);
        return -1;
    }
    let mut name = None;
    // SAFETY: the caller's guarantee.
    let Some(host) = (unsafe { msghdr_to_host(message.cast(), &mut name) }) else {
        crate::errno::set(libc::EOPNOTSUPP);
        return -1;
    };
    // SAFETY: a `msghdr` this function built, pointing at the caller's own
    // iovecs, which `struct iovec` has the same layout for on both platforms.
    unsafe { libc::sendmsg(fd, &host, sending_flags(fd, flags)) }
}

/// `ssize_t recvmsg(int, struct msghdr *, int)`
///
/// # Safety
///
/// As [`sendmsg`], with the iovecs writable.
unsafe extern "C" fn recvmsg(fd: i32, message: *mut c_void, flags: i32) -> isize {
    if message.is_null() {
        crate::errno::set(libc::EFAULT);
        return -1;
    }
    let guest: *mut MsgHdr = message.cast();
    let mut name = None;
    // SAFETY: the caller's guarantee.
    let Some(mut host) = (unsafe { msghdr_to_host(guest, &mut name) }) else {
        crate::errno::set(libc::EOPNOTSUPP);
        return -1;
    };
    // The name is written by the kernel, so it needs room rather than a copy of
    // what the guest had there.
    let mut received = HostAddr::empty();
    // SAFETY: the caller's own `msghdr`, read for its name buffer.
    let (guest_name, guest_namelen) = unsafe { ((*guest).name, (*guest).namelen) };
    if !guest_name.is_null() {
        host.msg_name = received.as_mut_ptr().cast();
        host.msg_namelen = received.len();
    }

    // SAFETY: a `msghdr` this function built over the caller's iovecs.
    let rc = unsafe { libc::recvmsg(fd, &mut host, flags_to_host(flags)) };
    if rc < 0 {
        return rc;
    }
    if !guest_name.is_null() {
        let mut len = guest_namelen;
        // The kernel reports the address's length in `msg_namelen`, not in the
        // buffer, so it has to be moved across before the conversion reads it.
        received = HostAddr::empty();
        // SAFETY: both are `STORAGE`-byte buffers this function owns.
        unsafe {
            core::ptr::copy_nonoverlapping(
                host.msg_name.cast::<u8>(),
                received.as_mut_ptr().cast::<u8>(),
                (host.msg_namelen as usize).min(addr::STORAGE),
            )
        };
        // SAFETY: the caller's name buffer, with the length they reserved.
        unsafe { addr::from_host(&received, guest_name, &mut len) };
        // SAFETY: the caller's own `msghdr`.
        unsafe { (*guest).namelen = len };
    }
    // SAFETY: as above.
    unsafe { (*guest).flags = flags_from_host(host.msg_flags) };
    rc
}

// Not in the `libc` crate on any platform, so declared here. Both take an
// address family, which is the only reason they are wrapped at all.
unsafe extern "C" {
    #[link_name = "inet_pton"]
    fn host_inet_pton(af: i32, src: *const libc::c_char, dst: *mut c_void) -> i32;
    #[link_name = "inet_ntop"]
    fn host_inet_ntop(
        af: i32,
        src: *const c_void,
        dst: *mut libc::c_char,
        size: u32,
    ) -> *const libc::c_char;
}

/// `int inet_pton(int af, const char *src, void *dst)`
///
/// # Safety
///
/// `src` is a NUL-terminated string and `dst` is large enough for the family's
/// address - four bytes or sixteen.
unsafe extern "C" fn inet_pton(af: i32, src: *const libc::c_char, dst: *mut c_void) -> i32 {
    let Some(af) = addr::family_to_host(af) else {
        crate::errno::set(libc::EAFNOSUPPORT);
        return -1;
    };
    // SAFETY: the caller's string and buffer.
    unsafe { host_inet_pton(af, src, dst) }
}

/// `const char *inet_ntop(int af, const void *src, char *dst, socklen_t size)`
///
/// # Safety
///
/// `src` is the family's address and `dst` is `size` writable bytes.
unsafe extern "C" fn inet_ntop(
    af: i32,
    src: *const c_void,
    dst: *mut libc::c_char,
    size: u32,
) -> *const libc::c_char {
    let Some(af) = addr::family_to_host(af) else {
        crate::errno::set(libc::EAFNOSUPPORT);
        return core::ptr::null();
    };
    // SAFETY: the caller's address and buffer.
    unsafe { host_inet_ntop(af, src, dst, size) }
}

pub const NAMES: &[&str] = &[
    "socket",
    "socketpair",
    "bind",
    "connect",
    "accept",
    "getsockname",
    "getpeername",
    "send",
    "recv",
    "sendto",
    "recvfrom",
    "sendmsg",
    "recvmsg",
    "inet_pton",
    "inet_ntop",
];

pub fn symbol(name: &str) -> Option<u64> {
    let f: *const () = match name {
        "socket" => socket as *const (),
        "socketpair" => socketpair as *const (),
        "bind" => bind as *const (),
        "connect" => connect as *const (),
        "accept" => accept as *const (),
        "getsockname" => getsockname as *const (),
        "getpeername" => getpeername as *const (),
        "send" => send as *const (),
        "recv" => recv as *const (),
        "sendto" => sendto as *const (),
        "recvfrom" => recvfrom as *const (),
        "sendmsg" => sendmsg as *const (),
        "recvmsg" => recvmsg as *const (),
        "inet_pton" => inet_pton as *const (),
        "inet_ntop" => inet_ntop as *const (),
        _ => return None,
    };
    Some(f as usize as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two that collide, named: `MSG_WAITALL` passed through asks this host
    /// not to wait at all, and `MSG_EOR` asks about truncated control data.
    #[test]
    fn the_message_flags_are_translated_rather_than_passed_through() {
        assert_eq!(msg::WAITALL, libc::MSG_DONTWAIT);
        assert_eq!(flags_to_host(msg::WAITALL), libc::MSG_WAITALL);
        assert_eq!(msg::EOR, libc::MSG_CTRUNC);
        assert_eq!(flags_to_host(msg::EOR), libc::MSG_EOR);
    }

    #[test]
    fn every_shared_message_flag_round_trips() {
        for (darwin, _) in FLAGS {
            assert_eq!(flags_from_host(flags_to_host(*darwin)), *darwin);
        }
    }

    /// Darwin's `MSG_EOF` has no host meaning and must not become one: `0x100`
    /// here is `MSG_WAITALL`, so passing it through would make a send wait.
    #[test]
    fn msg_eof_is_dropped_rather_than_mapped_onto_a_neighbour() {
        const EOF: i32 = 0x100;
        assert_eq!(EOF, libc::MSG_WAITALL, "the collision being avoided");
        assert_eq!(flags_to_host(EOF), 0);
    }

    #[test]
    fn a_socket_of_a_family_this_host_lacks_is_refused_rather_than_opened() {
        // Darwin's `AF_LINK`.
        // SAFETY: no caller memory is touched on the refusing path.
        assert_eq!(unsafe { socket(18, libc::SOCK_DGRAM, 0) }, -1);
        assert_eq!(crate::errno::get(), libc::EAFNOSUPPORT);
        crate::errno::set(0);
    }

    /// A round trip through the real stack, in Darwin's spelling throughout: an
    /// IPv6 socket - the family whose number differs - bound to the loopback,
    /// asked for its own name back.
    #[test]
    fn an_ipv6_socket_binds_and_reports_its_address_in_darwins_shape() {
        // SAFETY: Darwin's `AF_INET6`, which this entry point translates.
        let fd = unsafe { socket(addr::family::INET6, libc::SOCK_DGRAM, 0) };
        assert!(fd >= 0, "an IPv6 datagram socket");

        // `sin6_len`, `sin6_family`, port 0, flow 0, `::1`, scope 0.
        let mut want = [0u8; 28];
        want[0] = 28;
        want[1] = addr::family::INET6 as u8;
        want[8 + 15] = 1;
        // SAFETY: twenty-eight readable bytes, and the length matches.
        assert_eq!(unsafe { bind(fd, want.as_ptr().cast(), 28) }, 0);

        let mut got = [0u8; 28];
        let mut len = 28u32;
        // SAFETY: twenty-eight writable bytes and a live length.
        assert_eq!(
            unsafe { getsockname(fd, got.as_mut_ptr().cast(), &mut len) },
            0
        );
        assert_eq!(len, 28);
        assert_eq!(got[1], addr::family::INET6 as u8, "Darwin's family number");
        assert_eq!(&got[8..24], &want[8..24], "the address itself");

        // SAFETY: a descriptor this test opened and no longer uses.
        unsafe { libc::close(fd) };
    }

    /// The layout claim this module rests on: a guest's `struct msghdr` is
    /// forty-eight bytes and the host's is not, so it is a value to convert.
    #[test]
    fn the_guests_msghdr_is_not_the_hosts() {
        assert_eq!(core::mem::size_of::<MsgHdr>(), 48);
        assert!(
            core::mem::size_of::<libc::msghdr>() > core::mem::size_of::<MsgHdr>(),
            "this host's struct msghdr is Darwin-shaped after all"
        );
    }
}
