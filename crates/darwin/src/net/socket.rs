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
    /// Half-close after the send; see [`super::half_close_if_asked`].
    pub const EOF: i32 = 0x100;
    pub const NOSIGNAL: i32 = 0x80000;
}

/// Each flag that means the same thing on both, as `(darwin, host)`.
///
/// `MSG_EOF` is not here and must not be: `0x100` here is `MSG_WAITALL`. It is
/// the one Darwin flag this host answers with a *second call* rather than with
/// a bit - see [`half_close_if_asked`] - so it is stripped on the way through
/// and acted on afterwards.
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

/// Darwin's `SOCK_CLOEXEC`, which this host spells with a different bit.
const SOCK_CLOEXEC: i32 = 0x1000_0000;
/// Darwin's `SOCK_NONBLOCK`, likewise.
const SOCK_NONBLOCK: i32 = 0x2000_0000;

/// The guest's `type` argument as this host's.
///
/// The `SOCK_*` type numbers and the `IPPROTO_*` protocol numbers agree. The two
/// flag bits Darwin ORs into `type` do not, and they are the top of the word
/// where this host's are `0x80000`/`0x800` - so a guest asking for a
/// non-blocking socket in one call gets `EINVAL` from a host that has never seen
/// that bit. Anything else above the type is left alone, so a value neither
/// platform defines is refused by the kernel rather than quietly dropped here.
fn kind_to_host(darwin: i32) -> i32 {
    let mut host = darwin & !(SOCK_CLOEXEC | SOCK_NONBLOCK);
    if darwin & SOCK_CLOEXEC != 0 {
        host |= libc::SOCK_CLOEXEC;
    }
    if darwin & SOCK_NONBLOCK != 0 {
        host |= libc::SOCK_NONBLOCK;
    }
    host
}

/// `int socket(int domain, int type, int protocol)`
unsafe extern "C" fn socket(domain: i32, kind: i32, protocol: i32) -> i32 {
    let Some(domain) = addr::family_to_host(domain) else {
        crate::errno::set(libc::EAFNOSUPPORT);
        return -1;
    };
    // SAFETY: three integers; the call touches no caller memory.
    let fd = unsafe { libc::socket(domain, kind_to_host(kind), protocol) };
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
    let rc = unsafe { libc::socketpair(domain, kind_to_host(kind), protocol, sv) };
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
    let sent = unsafe { libc::send(fd, buf, len, sending_flags(fd, flags)) };
    half_close_if_asked(fd, flags, sent)
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
    let host_flags = sending_flags(fd, flags);
    if address.is_null() {
        // SAFETY: a connected socket, which is what a null address means.
        let sent = unsafe { libc::sendto(fd, buf, len, host_flags, core::ptr::null(), 0) };
        return half_close_if_asked(fd, flags, sent);
    }
    // SAFETY: the caller's guarantee.
    let Some(host) = (unsafe { addr::to_host(address, addrlen) }) else {
        crate::errno::set(libc::EAFNOSUPPORT);
        return -1;
    };
    // SAFETY: the caller's buffer and an address this function owns.
    let sent = unsafe { libc::sendto(fd, buf, len, host_flags, host.as_ptr(), host.len()) };
    half_close_if_asked(fd, flags, sent)
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

/// Say once that a control message was refused, and which one.
fn say_control_refused(refused: super::control::Untranslatable) {
    use std::sync::Mutex;
    static SAID: Mutex<Vec<super::control::Untranslatable>> = Mutex::new(Vec::new());
    let mut said = SAID.lock().unwrap_or_else(|e| e.into_inner());
    if said.contains(&refused) {
        return;
    }
    said.push(refused);
    let (level, kind) = (refused.level, refused.kind);
    eprintln!(
        "rosetta3: sendmsg with ancillary data of level {level:#x} type {kind:#x}: refused - \
         the control message type or payload shape is unsupported; types can have \
         different numbers on the two platforms, so a payload it does not know the shape of \
         cannot be handed across unread"
    );
}

/// The two halves `sendmsg` and `recvmsg` share: a guest `msghdr` as the host's.
///
/// `None` when the message carries ancillary data this host cannot be given -
/// see [`super::control`], which owns that decision and the conversion behind it. `control` is the host buffer the returned
/// `msghdr` points into, so the caller has to keep it alive across the call.
///
/// # Safety
///
/// `guest` is a readable Darwin `struct msghdr`.
unsafe fn msghdr_to_host(
    guest: *const MsgHdr,
    name: &mut Option<HostAddr>,
    control: &mut Vec<u8>,
    sending: bool,
) -> Option<libc::msghdr> {
    // SAFETY: the caller's guarantee.
    let g = unsafe { guest.read() };
    if sending && !g.control.is_null() && g.controllen != 0 {
        // SAFETY: the guest's control buffer, with the length it gave for it.
        match unsafe { super::control::to_host(g.control, g.controllen) } {
            Ok(host) => *control = host,
            Err(refused) => {
                say_control_refused(refused);
                return None;
            }
        }
    }
    // SAFETY: as above; a null name is a message with no address.
    *name = match !sending || g.name.is_null() {
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
    if !control.is_empty() {
        host.msg_control = control.as_mut_ptr().cast();
        host.msg_controllen = control.len() as _;
    }
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
    let mut control = Vec::new();
    // SAFETY: the caller's guarantee.
    let Some(host) = (unsafe { msghdr_to_host(message.cast(), &mut name, &mut control, true) })
    else {
        crate::errno::set(libc::EOPNOTSUPP);
        return -1;
    };
    // SAFETY: a `msghdr` this function built, pointing at the caller's own
    // iovecs - which `struct iovec` has the same layout for on both platforms -
    // and at a control buffer that outlives the call.
    let sent = unsafe { libc::sendmsg(fd, &host, sending_flags(fd, flags)) };
    half_close_if_asked(fd, flags, sent)
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
    let mut sent_control = Vec::new();
    // SAFETY: the caller's guarantee.
    let Some(mut host) = (unsafe { msghdr_to_host(guest, &mut name, &mut sent_control, false) })
    else {
        crate::errno::set(libc::EOPNOTSUPP);
        return -1;
    };
    // The name is written by the kernel, so it needs room rather than a copy of
    // what the guest had there.
    let mut received = HostAddr::empty();
    // SAFETY: the caller's own `msghdr`, read for its name and control buffers.
    let (guest_name, guest_namelen, guest_control, guest_controllen) = unsafe {
        (
            (*guest).name,
            (*guest).namelen,
            (*guest).control,
            (*guest).controllen,
        )
    };
    if !guest_name.is_null() {
        host.msg_name = received.as_mut_ptr().cast();
        host.msg_namelen = received.len();
    }
    // As with the name: the kernel fills the control buffer in, and what it
    // writes there is wider than what the guest reserved room for.
    let mut control = vec![0u8; super::control::host_capacity(guest_controllen)];
    if guest_control.is_null() || guest_controllen == 0 {
        host.msg_control = core::ptr::null_mut();
        host.msg_controllen = 0;
    } else {
        host.msg_control = control.as_mut_ptr().cast();
        host.msg_controllen = control.len() as _;
    }

    // SAFETY: a `msghdr` this function built over the caller's iovecs.
    let rc = unsafe { libc::recvmsg(fd, &mut host, flags_to_host(flags)) };
    if rc < 0 {
        return rc;
    }
    if !guest_name.is_null() {
        let mut len = guest_namelen;
        // The kernel reports the address's length in `msg_namelen` rather than
        // beside the bytes, so it is moved into the buffer's own length before
        // the conversion reads it. The bytes are already there - `msg_name` is
        // this buffer - so nothing is copied.
        // SAFETY: the length slot of a buffer this function owns.
        unsafe { *received.len_mut() = host.msg_namelen.min(addr::STORAGE as u32) };
        // SAFETY: the caller's name buffer, with the length they reserved.
        unsafe { addr::from_host(&received, guest_name, &mut len) };
        // SAFETY: the caller's own `msghdr`.
        unsafe { (*guest).namelen = len };
    }
    let mut received_flags = flags_from_host(host.msg_flags);
    if !guest_control.is_null() && guest_controllen != 0 {
        // SAFETY: a `msghdr` the kernel has just filled in, and the caller's
        // control buffer with the length they reserved.
        let (written, dropped) =
            unsafe { super::control::from_host(&host, guest_control, guest_controllen) };
        // SAFETY: the caller's own `msghdr`.
        unsafe { (*guest).controllen = written };
        if dropped {
            received_flags |= msg::CTRUNC;
        }
    }
    // SAFETY: the caller's own `msghdr`.
    unsafe { (*guest).flags = received_flags };
    rc
}

/// Darwin's `MSG_EOF`: send, then half-close.
///
/// The bit asks for the send and a `shutdown(SHUT_WR)` after it, which is how
/// Darwin's `send` ends a request without a second call. There is no host flag
/// for it and there was no host meaning either - `0x100` here is `MSG_WAITALL`,
/// so passing it through made a send wait - so it is stripped by
/// [`flags_to_host`] and answered here instead.
///
/// A failed send is not half-closed: the caller will retry it.
fn half_close_if_asked(fd: i32, darwin_flags: i32, sent: isize) -> isize {
    if sent >= 0 && darwin_flags & msg::EOF != 0 {
        // SAFETY: a descriptor and a constant; the call touches no memory.
        unsafe { libc::shutdown(fd, libc::SHUT_WR) };
    }
    sent
}

/// `ssize_t write(int, const void *, size_t)`
///
/// Here, and not passed through, for one reason: `SO_NOSIGPIPE`. Darwin's is a
/// property of the socket and this host's `MSG_NOSIGNAL` is a property of the
/// call, so a guest that set the option and then wrote to a closed socket with
/// `write` - which takes no flags - still took a `SIGPIPE` and died. A write to
/// a descriptor the guest set the option on is sent instead, which is the same
/// call for a socket and is where the flag can be given.
///
/// The cost of being in this path at all is one relaxed load per write; see
/// [`super::sockopt::any_suppress_sigpipe`].
///
/// # Safety
///
/// `buf` is `len` readable bytes.
unsafe extern "C" fn write(fd: i32, buf: *const c_void, len: usize) -> isize {
    if super::sockopt::suppresses_sigpipe(fd) {
        // SAFETY: the caller's buffer. A `send` with no flags is a `write` on
        // anything this branch can be reached for - the descriptor is one the
        // guest set a socket option on.
        return unsafe { libc::send(fd, buf, len, libc::MSG_NOSIGNAL) };
    }
    // SAFETY: the caller's buffer.
    unsafe { libc::write(fd, buf, len) }
}

/// `ssize_t writev(int, const struct iovec *, int)`
///
/// As [`write`]: the gathered form needs the same flag and has the same
/// nowhere to put it, so it becomes a `sendmsg` when the option is set.
///
/// # Safety
///
/// `iov` is `iovcnt` readable `struct iovec`.
unsafe extern "C" fn writev(fd: i32, iov: *const libc::iovec, iovcnt: i32) -> isize {
    if super::sockopt::suppresses_sigpipe(fd) {
        // SAFETY: zeroed is a valid `msghdr` - no name, no control - and the
        // two fields that matter are the caller's own iovecs.
        let mut host: libc::msghdr = unsafe { core::mem::zeroed() };
        host.msg_iov = iov.cast_mut();
        host.msg_iovlen = iovcnt.max(0) as _;
        // SAFETY: a `msghdr` this function built over the caller's iovecs.
        return unsafe { libc::sendmsg(fd, &host, libc::MSG_NOSIGNAL) };
    }
    // SAFETY: the caller's iovecs, whose layout is the same on both platforms.
    unsafe { libc::writev(fd, iov, iovcnt) }
}

/// Darwin's `sa_endpoints_t`, the argument that makes `connectx` more than a
/// `connect`: a source and a destination in one call.
#[repr(C)]
#[derive(Clone, Copy)]
struct Endpoints {
    srcif: u32,
    srcaddr: *const c_void,
    srcaddrlen: u32,
    dstaddr: *const c_void,
    dstaddrlen: u32,
}

/// `int connectx(int, const sa_endpoints_t *, sae_associd_t, unsigned int,
/// const struct iovec *, unsigned int, size_t *, sae_connid_t *)`
///
/// Darwin's own multipath-aware connect. This host has no such call, and does
/// not need one for what a guest uses it for: the three things `connectx` adds
/// over `connect` are a source endpoint, which is a `bind` before the connect;
/// data sent with the handshake, which is a `writev` after it; and an
/// association, which only Multipath TCP has and which a guest asks for by
/// socket type rather than here.
///
/// So this is `bind`, `connect`, `writev` - the same sequence with the same
/// outcome, minus the round trip that fast-open would have saved. The `flags`
/// are `CONNECT_DATA_IDEMPOTENT` and its relatives, which say the payload may
/// be replayed; sending it after the handshake instead satisfies all of them.
///
/// `sae_srcif` is refused rather than dropped: binding to an interface by index
/// is a different call here - `SO_BINDTODEVICE`, which takes a name - and a
/// connection that silently ignored the interface it was pinned to would go out
/// of the wrong one and work.
///
/// # Safety
///
/// `endpoints` is a readable `sa_endpoints_t` whose addresses are readable at
/// their stated lengths; `iov` is `iovcnt` readable `struct iovec`; `len` and
/// `connid` are writable or null.
#[allow(clippy::too_many_arguments)]
unsafe extern "C" fn connectx(
    fd: i32,
    endpoints: *const Endpoints,
    _associd: u32,
    _flags: u32,
    iov: *const libc::iovec,
    iovcnt: u32,
    len: *mut usize,
    connid: *mut u32,
) -> i32 {
    if endpoints.is_null() {
        crate::errno::set(libc::EFAULT);
        return -1;
    }
    // SAFETY: the caller's guarantee.
    let ends = unsafe { endpoints.read() };
    if ends.srcif != 0 {
        crate::errno::set(libc::ENOPROTOOPT);
        return -1;
    }
    if !ends.srcaddr.is_null() {
        // SAFETY: the caller's source address, at the length they gave.
        let rc = unsafe { with_address(fd, ends.srcaddr, ends.srcaddrlen, libc::bind) };
        if rc < 0 {
            return rc;
        }
    }
    // SAFETY: the caller's destination address, at the length they gave.
    let rc = unsafe { with_address(fd, ends.dstaddr, ends.dstaddrlen, libc::connect) };
    if rc < 0 {
        return rc;
    }
    if !connid.is_null() {
        // SAFETY: the caller's slot. `SAE_CONNID_ANY` is the only connection
        // this socket has, there being no association to number it within.
        unsafe { connid.write(0) };
    }
    if iov.is_null() || iovcnt == 0 {
        return 0;
    }
    // SAFETY: the caller's iovecs, whose layout is the same on both platforms.
    let written = unsafe { libc::writev(fd, iov, iovcnt as i32) };
    if written < 0 {
        // The connection stands; only the data did not go. Darwin reports this
        // the same way - the descriptor is connected and `errno` says why the
        // payload did not follow.
        return -1;
    }
    if !len.is_null() {
        // SAFETY: the caller's slot.
        unsafe { len.write(written as usize) };
    }
    0
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
    "write",
    "writev",
    "connectx",
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
        "write" => write as *const (),
        "writev" => writev as *const (),
        "connectx" => connectx as *const (),
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
    #[test]
    fn receiving_does_not_parse_output_buffers() {
        let mut bytes = [0xffu8; 32];
        let guest = super::MsgHdr {
            name: bytes.as_mut_ptr().cast(),
            namelen: bytes.len() as u32,
            iov: core::ptr::null_mut(),
            iovlen: 0,
            control: bytes.as_mut_ptr().cast(),
            controllen: bytes.len() as u32,
            flags: 0,
        };
        let mut name = None;
        let mut control = Vec::new();
        // SAFETY: guest is initialized and its output buffers are writable.
        let host = unsafe { super::msghdr_to_host(&guest, &mut name, &mut control, false) };
        assert!(host.is_some());
        assert!(name.is_none());
        assert!(control.is_empty());
    }

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

    /// Darwin's `MSG_EOF` must not reach this host as a flag: `0x100` here is
    /// `MSG_WAITALL`, so passing it through would make a send wait. It is
    /// answered with a `shutdown` after the send instead, which the round trip
    /// below checks end to end.
    #[test]
    fn msg_eof_is_not_handed_to_this_host_as_a_flag() {
        assert_eq!(msg::EOF, libc::MSG_WAITALL, "the collision being avoided");
        assert_eq!(flags_to_host(msg::EOF), 0);
    }

    /// A pair of connected sockets, which is what the send-side tests need.
    fn pair() -> (i32, i32) {
        let mut ends = [0i32; 2];
        // SAFETY: a live array of two `int`.
        let rc =
            unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, ends.as_mut_ptr()) };
        assert_eq!(rc, 0);
        (ends[0], ends[1])
    }

    /// `MSG_EOF` asks for a half-close after the send, which is the peer seeing
    /// the bytes and then seeing end-of-file without anything being closed.
    #[test]
    fn msg_eof_half_closes_the_socket_after_the_send() {
        let (ours, theirs) = pair();
        // SAFETY: five readable bytes, and the length says five.
        let sent = unsafe { send(ours, c"hello".as_ptr().cast(), 5, msg::EOF) };
        assert_eq!(sent, 5);

        let mut buf = [0u8; 8];
        // SAFETY: eight writable bytes of this test's own.
        let read = unsafe { libc::read(theirs, buf.as_mut_ptr().cast(), 8) };
        assert_eq!(read, 5, "the bytes went before the half-close");
        // SAFETY: as above; the write half is shut, so this is end of file.
        assert_eq!(unsafe { libc::read(theirs, buf.as_mut_ptr().cast(), 8) }, 0);

        // SAFETY: two descriptors this test opened and no longer uses.
        unsafe {
            libc::close(ours);
            libc::close(theirs);
        }
    }

    /// The refusal this module used to make: a descriptor passed over a socket
    /// now arrives as a descriptor, in Darwin's twelve-byte control message on
    /// both sides.
    #[test]
    fn a_descriptor_crosses_a_socket_in_darwins_control_message() {
        let (ours, theirs) = pair();
        let mut ends = [0i32; 2];
        // SAFETY: a live array of two `int`.
        assert_eq!(unsafe { libc::pipe(ends.as_mut_ptr()) }, 0);

        // Darwin's `CMSG_SPACE(sizeof(int))`: a twelve-byte header and four
        // bytes of payload, which needs no padding.
        let mut control = [0u8; 16];
        control[0..4].copy_from_slice(&16u32.to_ne_bytes());
        control[4..8].copy_from_slice(&0xffffu32.to_ne_bytes());
        control[8..12].copy_from_slice(&1i32.to_ne_bytes());
        control[12..16].copy_from_slice(&ends[0].to_ne_bytes());

        let mut byte = *b"x";
        let mut iov = libc::iovec {
            iov_base: byte.as_mut_ptr().cast(),
            iov_len: 1,
        };
        let out = MsgHdr {
            name: core::ptr::null_mut(),
            namelen: 0,
            iov: &mut iov,
            iovlen: 1,
            control: control.as_mut_ptr().cast(),
            controllen: 16,
            flags: 0,
        };
        // SAFETY: a Darwin `msghdr` this test built, with one live iovec and a
        // control buffer of the length it states.
        assert_eq!(unsafe { sendmsg(ours, (&raw const out).cast(), 0) }, 1);

        let mut got = [0u8; 1];
        let mut got_iov = libc::iovec {
            iov_base: got.as_mut_ptr().cast(),
            iov_len: 1,
        };
        let mut back = [0u8; 32];
        let mut incoming = MsgHdr {
            name: core::ptr::null_mut(),
            namelen: 0,
            iov: &mut got_iov,
            iovlen: 1,
            control: back.as_mut_ptr().cast(),
            controllen: 32,
            flags: 0,
        };
        // SAFETY: as above, with the buffers writable.
        assert_eq!(unsafe { recvmsg(theirs, (&raw mut incoming).cast(), 0) }, 1);
        assert_eq!(got, *b"x");
        assert_eq!(incoming.controllen, 16, "one twelve-byte header and an int");
        assert_eq!(u32::from_ne_bytes(back[4..8].try_into().unwrap()), 0xffff);
        assert_eq!(i32::from_ne_bytes(back[8..12].try_into().unwrap()), 1);

        // The descriptor that arrived is a descriptor, and it is the read end
        // of the pipe rather than the number that was sent.
        let received = i32::from_ne_bytes(back[12..16].try_into().unwrap());
        // SAFETY: the write end of a pipe this test owns.
        assert_eq!(
            unsafe { libc::write(ends[1], c"pipe".as_ptr().cast(), 4) },
            4
        );
        let mut through = [0u8; 4];
        // SAFETY: a descriptor the kernel just installed, and four writable bytes.
        assert_eq!(
            unsafe { libc::read(received, through.as_mut_ptr().cast(), 4) },
            4
        );
        assert_eq!(&through, b"pipe");

        // SAFETY: descriptors this test opened and no longer uses.
        unsafe {
            for fd in [ours, theirs, ends[0], ends[1], received] {
                libc::close(fd);
            }
        }
    }

    /// The part of `SO_NOSIGPIPE` that a per-call flag did not reach: `write`
    /// takes no flags, so a guest that set the option and wrote to a socket
    /// whose peer had gone still died of a signal.
    #[test]
    fn a_write_to_a_closed_peer_reports_epipe_when_the_guest_asked_it_to() {
        let (ours, theirs) = pair();
        // SAFETY: the other end, which this test is finished with.
        unsafe { libc::close(theirs) };

        let on = 1i32;
        // SAFETY: one readable `int`, and the length says four. `0xffff` is
        // Darwin's `SOL_SOCKET` and `0x1022` its `SO_NOSIGPIPE`.
        let rc = unsafe {
            crate::net::sockopt::setsockopt(ours, 0xffff, 0x1022, (&raw const on).cast(), 4)
        };
        assert_eq!(rc, 0);

        // SAFETY: five readable bytes, and the length says five.
        let wrote = unsafe { write(ours, c"hello".as_ptr().cast(), 5) };
        assert_eq!((wrote, crate::errno::get()), (-1, libc::EPIPE));
        crate::errno::set(0);

        crate::net::sockopt::forget(ours);
        // SAFETY: a descriptor this test opened and no longer uses.
        unsafe { libc::close(ours) };
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

    /// The sender's address a `recvmsg` reports, which this module used to
    /// overwrite with zeroes between the call and the conversion.
    #[test]
    fn recvmsg_reports_the_senders_address_in_darwins_shape() {
        /// A Darwin `sockaddr_in` for `127.0.0.1:port`.
        fn loopback(port: u16) -> [u8; 16] {
            let mut a = [0u8; 16];
            a[0] = 16;
            a[1] = addr::family::INET as u8;
            a[2..4].copy_from_slice(&port.to_be_bytes());
            a[4..8].copy_from_slice(&[127, 0, 0, 1]);
            a
        }

        // SAFETY: Darwin's `AF_INET`; no caller memory is touched.
        let (rx, tx) = unsafe {
            (
                socket(addr::family::INET, libc::SOCK_DGRAM, 0),
                socket(addr::family::INET, libc::SOCK_DGRAM, 0),
            )
        };
        assert!(rx >= 0 && tx >= 0, "two datagram sockets");

        let any = loopback(0);
        // SAFETY: sixteen readable bytes, and the length matches.
        unsafe {
            assert_eq!(bind(rx, any.as_ptr().cast(), 16), 0);
            assert_eq!(bind(tx, any.as_ptr().cast(), 16), 0);
        }

        let mut bound = [0u8; 16];
        let mut len = 16u32;
        // SAFETY: sixteen writable bytes and a live length.
        assert_eq!(
            unsafe { getsockname(rx, bound.as_mut_ptr().cast(), &mut len) },
            0
        );
        let mut sender = [0u8; 16];
        let mut sender_len = 16u32;
        // SAFETY: as above.
        assert_eq!(
            unsafe { getsockname(tx, sender.as_mut_ptr().cast(), &mut sender_len) },
            0
        );

        // SAFETY: one readable byte and a sixteen-byte address of that length.
        assert_eq!(
            unsafe { sendto(tx, c"x".as_ptr().cast(), 1, 0, bound.as_ptr().cast(), 16) },
            1
        );

        let mut got = [0u8; 1];
        let mut iov = libc::iovec {
            iov_base: got.as_mut_ptr().cast(),
            iov_len: 1,
        };
        let mut name = [0u8; 128];
        let mut incoming = MsgHdr {
            name: name.as_mut_ptr().cast(),
            namelen: 128,
            iov: &mut iov,
            iovlen: 1,
            control: core::ptr::null_mut(),
            controllen: 0,
            flags: 0,
        };
        // SAFETY: a Darwin `msghdr` this test built, with one writable iovec
        // and a name buffer of the length it states.
        assert_eq!(unsafe { recvmsg(rx, (&raw mut incoming).cast(), 0) }, 1);
        assert_eq!(got, *b"x");
        assert_eq!(incoming.namelen, 16, "a `sockaddr_in`");
        assert_eq!(name[1], addr::family::INET as u8, "Darwin's family number");
        assert_eq!(&name[2..8], &sender[2..8], "the port and address that sent");

        // SAFETY: descriptors this test opened and no longer uses.
        unsafe {
            libc::close(rx);
            libc::close(tx);
        }
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
