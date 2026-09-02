//! What a Mach port is made of on a host that has no Mach: one unix datagram
//! socket per receive right.
//!
//! The choice of transport is what makes the rest of this directory possible.
//! Mach's two hard properties are that a message keeps its boundaries and that a
//! *right* can travel inside one, and a `SOCK_DGRAM` unix socket has both: it
//! preserves datagram framing, and `SCM_RIGHTS` moves a descriptor between
//! processes. A receive right therefore **is** a bound socket, and moving one to
//! another process is passing its descriptor - not a simulation of the move but
//! the same thing under a different name.
//!
//! Ports are named in the abstract namespace (a leading NUL), so nothing is left
//! on disk when a process dies and a port's name is released the moment its last
//! descriptor closes. That in turn is why a send right can be *just* a 64-bit
//! id: the id names an address, and a send to an address nothing is bound to
//! fails with `ECONNREFUSED`, which is the same event as sending to a dead port.
//!
//! The id is random rather than sequential because the namespace is shared by
//! every process on the host: a counter would collide across two runs, and a
//! collision hands one run's message to another run's port.

#![cfg(not(target_os = "macos"))]

use std::ffi::c_int;
use std::sync::Mutex;

/// A port's address: the abstract socket name is derived from it and nothing
/// else, so this is the whole of a send right's contents.
pub type Address = u64;

/// A datagram as it crossed, with any rights that travelled with it still as
/// descriptors.
pub struct Datagram {
    pub bytes: Vec<u8>,
    pub fds: Vec<c_int>,
}

impl Drop for Datagram {
    /// Any right that was received and never claimed is destroyed, which is what
    /// Mach does with the rights in a message a receiver discards.
    fn drop(&mut self) {
        for fd in self.fds.drain(..) {
            // SAFETY: a descriptor this process received and has not handed on.
            unsafe { libc::close(fd) };
        }
    }
}

impl Datagram {
    /// Take the descriptor at `index`, leaving nothing behind for [`Drop`].
    pub fn take_fd(&mut self, index: usize) -> Option<c_int> {
        let slot = self.fds.get_mut(index)?;
        match *slot {
            -1 => None,
            fd => {
                *slot = -1;
                Some(fd)
            }
        }
    }
}

/// The largest message this transport carries.
///
/// A unix datagram's real limit is the socket buffer, which is tunable and not
/// worth guessing at; this is a ceiling of our own so that an absurd
/// `msgh_size` is refused by name rather than truncated by the kernel. Mach's
/// own inline limit is of the same order.
pub const MAX_MESSAGE: usize = 64 * 1024;

/// A fresh, unused port address.
pub fn fresh_address() -> Address {
    let mut bytes = [0u8; 8];
    // SAFETY: eight writable bytes, and the length matches.
    let read = unsafe { libc::getrandom(bytes.as_mut_ptr().cast(), bytes.len(), 0) };
    if read == bytes.len() as isize {
        // Never zero: zero is `MACH_PORT_NULL`'s address and a null send right
        // must not be indistinguishable from a real one.
        let id = u64::from_ne_bytes(bytes);
        if id != 0 {
            return id;
        }
    }
    // The kernel declining to answer is not a reason to hand out a colliding
    // name, so fall back to something still unique within this host and boot:
    // the process, the thread, and a counter.
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let count = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    // SAFETY: `getpid` takes no argument and only reads.
    let pid = unsafe { libc::getpid() } as u64;
    (pid << 32) | count
}

/// The abstract socket name for `address`, and its length.
///
/// The uid is in the name only so that two users' ports are visibly separate in
/// `ss -x`; the random address is what actually keeps them apart.
fn sockaddr(address: Address) -> (libc::sockaddr_un, libc::socklen_t) {
    let mut addr: libc::sockaddr_un = unsafe { core::mem::zeroed() };
    addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
    // SAFETY: `getuid` takes no argument and cannot fail.
    let uid = unsafe { libc::getuid() };
    let name = format!("rosetta3.mach.{uid}.{address:016x}");
    // The leading NUL is the abstract namespace; the name is what follows it and
    // is *not* NUL-terminated, so the length has to be exact.
    let bytes = name.as_bytes();
    for (slot, byte) in addr.sun_path[1..].iter_mut().zip(bytes) {
        *slot = *byte as _;
    }
    let len = core::mem::size_of::<libc::sa_family_t>() + 1 + bytes.len();
    (addr, len as libc::socklen_t)
}

/// Whether anything still holds the receive right for `address`.
///
/// Asked by the name server, which has to tell a service that is running from
/// one whose process died without unregistering. A datagram `connect` is the
/// cheap way to find out: it fails with `ECONNREFUSED` when nothing is bound
/// and sends nothing when it succeeds, so a live holder never sees it.
pub fn is_bound(address: Address) -> bool {
    let Ok(fd) = socket() else { return false };
    let (addr, len) = sockaddr(address);
    // SAFETY: a socket this call just opened, and an address whose length is
    // computed from the name written into it.
    let rc = unsafe { libc::connect(fd, (&raw const addr).cast(), len) };
    // SAFETY: the descriptor opened above, which nothing else holds.
    unsafe { libc::close(fd) };
    rc == 0
}

/// The end of a port that messages arrive at: a receive right.
pub struct Receiver {
    fd: c_int,
    address: Address,
    /// A message that was too large for the buffer a caller offered.
    ///
    /// Mach leaves such a message queued when the caller asked for
    /// `MACH_RCV_LARGE`, so that a second call with a bigger buffer gets it. A
    /// datagram socket has no way to put one back, so it is held here instead
    /// and handed to the next receive on this port.
    held: Mutex<Option<Datagram>>,
}

impl Drop for Receiver {
    fn drop(&mut self) {
        // SAFETY: this type's own descriptor, not shared with any other value:
        // a `Receiver` is only ever reached through an `Arc`.
        unsafe { libc::close(self.fd) };
    }
}

impl Receiver {
    /// Bind a new port. The address it answers on is fresh and unused.
    pub fn bind() -> std::io::Result<Self> {
        // Retried because two processes can draw the same address, however
        // unlikely - and the failure mode of not retrying is one run receiving
        // another's messages, which no caller could diagnose.
        for _ in 0..8 {
            let address = fresh_address();
            let fd = socket()?;
            let (addr, len) = sockaddr(address);
            // SAFETY: a socket this call just opened, and an address whose
            // length is computed from the name written into it.
            let rc = unsafe { libc::bind(fd, (&raw const addr).cast(), len) };
            if rc == 0 {
                return Ok(Receiver {
                    fd,
                    address,
                    held: Mutex::new(None),
                });
            }
            let taken = std::io::Error::last_os_error();
            // SAFETY: the descriptor opened above, which nothing else holds.
            unsafe { libc::close(fd) };
            if taken.raw_os_error() != Some(libc::EADDRINUSE) {
                return Err(taken);
            }
        }
        Err(std::io::Error::other("no free Mach port address"))
    }

    /// Adopt a receive right that arrived in a message: the descriptor is the
    /// same socket the sender had bound, so this end answers on the same
    /// address.
    ///
    /// # Safety
    ///
    /// `fd` is an owned descriptor for the socket bound to `address`.
    pub unsafe fn adopt(fd: c_int, address: Address) -> Self {
        Receiver {
            fd,
            address,
            held: Mutex::new(None),
        }
    }

    pub fn address(&self) -> Address {
        self.address
    }

    pub fn fd(&self) -> c_int {
        self.fd
    }

    /// Give up the descriptor, so it can be sent to another process.
    ///
    /// The `Receiver` must be dropped without closing after this, which is why
    /// it consumes the value.
    pub fn into_fd(self) -> c_int {
        let fd = self.fd;
        core::mem::forget(self);
        fd
    }

    /// A message held back because the last caller's buffer was too small.
    pub fn take_held(&self) -> Option<Datagram> {
        self.held.lock().unwrap_or_else(|e| e.into_inner()).take()
    }

    /// Keep `message` for the next receive on this port.
    pub fn hold(&self, message: Datagram) {
        *self.held.lock().unwrap_or_else(|e| e.into_inner()) = Some(message);
    }

    /// Whether a message is waiting, without taking it.
    pub fn ready(&self) -> bool {
        self.held.lock().unwrap_or_else(|e| e.into_inner()).is_some() || poll_one(self.fd, 0)
    }

    /// Take the next message, or `None` if none arrived within `timeout_ms`.
    ///
    /// `timeout_ms` of `-1` waits forever, which is what a `mach_msg` without
    /// `MACH_RCV_TIMEOUT` asks for.
    pub fn receive(&self, timeout_ms: i32) -> std::io::Result<Option<Datagram>> {
        if let Some(held) = self.take_held() {
            return Ok(Some(held));
        }
        if !poll_one(self.fd, timeout_ms) {
            return Ok(None);
        }
        receive_from(self.fd)
    }
}

/// A socket suitable for a port: datagrams, close-on-exec.
///
/// `SOCK_CLOEXEC` because a port must not leak into a process that was never
/// given the right - a `posix_spawn` of an unrelated program would otherwise
/// inherit every port this process holds.
fn socket() -> std::io::Result<c_int> {
    // SAFETY: a plain socket call with constant arguments.
    let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) };
    match fd < 0 {
        true => Err(std::io::Error::last_os_error()),
        false => Ok(fd),
    }
}

/// Wait for `fd` to have a datagram. `timeout_ms` of `-1` waits forever.
fn poll_one(fd: c_int, timeout_ms: i32) -> bool {
    let mut entry = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    loop {
        // SAFETY: one live `struct pollfd`, and the count matches.
        let rc = unsafe { libc::poll(&mut entry, 1, timeout_ms) };
        if rc < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
            // A signal is not a timeout. Restarting with the original timeout
            // can wait longer than asked; waking a `mach_msg` early would be
            // read as `MACH_RCV_TIMED_OUT`, which is the worse answer.
            continue;
        }
        return rc > 0;
    }
}

/// Wait for any of `fds` and answer with the index of the first that is ready.
///
/// This is what a port *set* receives on: Mach's set has no queue of its own, it
/// just names the members whose queues to look at.
pub fn poll_any(fds: &[c_int], timeout_ms: i32) -> Option<usize> {
    let mut entries: Vec<libc::pollfd> = fds
        .iter()
        .map(|fd| libc::pollfd {
            fd: *fd,
            events: libc::POLLIN,
            revents: 0,
        })
        .collect();
    if entries.is_empty() {
        return None;
    }
    loop {
        // SAFETY: `entries` is exactly the number of live `struct pollfd` the
        // count names.
        let rc = unsafe { libc::poll(entries.as_mut_ptr(), entries.len() as libc::nfds_t, timeout_ms) };
        if rc < 0 {
            if std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return None;
        }
        if rc == 0 {
            return None;
        }
        return entries.iter().position(|e| e.revents & libc::POLLIN != 0);
    }
}

/// The largest number of rights one message can carry.
///
/// Mach has no such limit; `SCM_RIGHTS` does, and a fixed control buffer is what
/// keeps the receive path from allocating on a length the sender chose.
pub const MAX_RIGHTS: usize = 16;

fn control_len() -> usize {
    // SAFETY: `CMSG_SPACE` is arithmetic on its argument.
    unsafe { libc::CMSG_SPACE((MAX_RIGHTS * core::mem::size_of::<c_int>()) as u32) as usize }
}

/// Send `bytes` to `destination`, moving `fds` with it.
///
/// The descriptors are *not* closed here: a caller that moved a right closes its
/// own copy once the send has succeeded, and one that failed still holds the
/// right it was asked to move.
pub fn send(destination: Address, bytes: &[u8], fds: &[c_int]) -> std::io::Result<()> {
    /// One sending socket for the whole process.
    ///
    /// Unbound: a reply travels to a port named inside the message, never back
    /// to the source address of the datagram, so this end never needs a name.
    /// `sendmsg` is atomic per datagram, so the descriptor is shared freely.
    static SENDER: std::sync::OnceLock<Option<c_int>> = std::sync::OnceLock::new();

    let Some(sender) = *SENDER.get_or_init(|| socket().ok()) else {
        return Err(std::io::Error::other("no Mach sending socket"));
    };
    if fds.len() > MAX_RIGHTS {
        return Err(std::io::Error::from_raw_os_error(libc::EMSGSIZE));
    }

    let (addr, addr_len) = sockaddr(destination);
    let mut iov = libc::iovec {
        iov_base: bytes.as_ptr() as *mut _,
        iov_len: bytes.len(),
    };
    let mut control = vec![0u8; control_len()];
    // SAFETY: `msghdr` is a plain C structure; zeroing it is what the header's
    // own examples do before filling in the fields used below.
    let mut msg: libc::msghdr = unsafe { core::mem::zeroed() };
    msg.msg_name = (&raw const addr) as *mut _;
    msg.msg_namelen = addr_len;
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;

    if !fds.is_empty() {
        let payload = fds.len() * core::mem::size_of::<c_int>();
        // SAFETY: `control` is `control_len()` bytes, which is `CMSG_SPACE` for
        // the largest payload allowed, and `fds.len()` is within that.
        unsafe {
            msg.msg_control = control.as_mut_ptr().cast();
            msg.msg_controllen = libc::CMSG_SPACE(payload as u32) as _;
            let cmsg = libc::CMSG_FIRSTHDR(&msg);
            (*cmsg).cmsg_level = libc::SOL_SOCKET;
            (*cmsg).cmsg_type = libc::SCM_RIGHTS;
            (*cmsg).cmsg_len = libc::CMSG_LEN(payload as u32) as _;
            core::ptr::copy_nonoverlapping(
                fds.as_ptr().cast::<u8>(),
                libc::CMSG_DATA(cmsg),
                payload,
            );
        }
    }

    loop {
        // SAFETY: `msg` describes `bytes` and the control block above, both
        // still live.
        let sent = unsafe { libc::sendmsg(sender, &msg, 0) };
        if sent >= 0 {
            return Ok(());
        }
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::EINTR) {
            continue;
        }
        return Err(error);
    }
}

/// Take one datagram from `fd`, with any descriptors that came with it.
fn receive_from(fd: c_int) -> std::io::Result<Option<Datagram>> {
    let mut bytes = vec![0u8; MAX_MESSAGE];
    let mut control = vec![0u8; control_len()];
    let mut iov = libc::iovec {
        iov_base: bytes.as_mut_ptr().cast(),
        iov_len: bytes.len(),
    };
    // SAFETY: as in [`send`].
    let mut msg: libc::msghdr = unsafe { core::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast();
    msg.msg_controllen = control.len() as _;

    let received = loop {
        // SAFETY: `msg` describes `bytes` and `control`, both live and of the
        // lengths recorded in it.
        let rc = unsafe { libc::recvmsg(fd, &mut msg, libc::MSG_CMSG_CLOEXEC) };
        if rc >= 0 {
            break rc as usize;
        }
        let error = std::io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::EINTR) => continue,
            Some(libc::EAGAIN) => return Ok(None),
            _ => return Err(error),
        }
    };

    let mut fds = Vec::new();
    // SAFETY: `msg` was just filled in by `recvmsg`, so its control block is a
    // well-formed sequence of `cmsghdr`.
    unsafe {
        let mut cmsg = libc::CMSG_FIRSTHDR(&msg);
        while !cmsg.is_null() {
            if (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_RIGHTS {
                let payload = (*cmsg).cmsg_len as usize - libc::CMSG_LEN(0) as usize;
                let count = payload / core::mem::size_of::<c_int>();
                let data = libc::CMSG_DATA(cmsg).cast::<c_int>();
                for i in 0..count {
                    fds.push(data.add(i).read_unaligned());
                }
            }
            cmsg = libc::CMSG_NXTHDR(&msg, cmsg);
        }
    }

    bytes.truncate(received);
    Ok(Some(Datagram { bytes, fds }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_message_sent_to_a_bound_port_arrives_whole() {
        let port = Receiver::bind().expect("bind");
        send(port.address(), b"hello", &[]).expect("send");
        let message = port.receive(1000).expect("receive").expect("a message");
        assert_eq!(message.bytes, b"hello");
        assert!(message.fds.is_empty());
    }

    /// Nothing bound to an address is the same event as a dead port, and it has
    /// to be reported rather than silently dropped: it is how a client learns
    /// the other end is gone.
    #[test]
    fn a_send_to_an_address_nobody_holds_fails() {
        let error = send(fresh_address(), b"x", &[]).expect_err("no such port");
        assert_eq!(error.raw_os_error(), Some(libc::ECONNREFUSED));
    }

    #[test]
    fn a_receive_with_a_timeout_gives_up_rather_than_waiting() {
        let port = Receiver::bind().expect("bind");
        let started = std::time::Instant::now();
        assert!(port.receive(50).expect("receive").is_none());
        assert!(started.elapsed() >= std::time::Duration::from_millis(40));
    }

    /// The property the whole design rests on: a receive right is a descriptor,
    /// so passing the descriptor passes the right - the adopting end answers on
    /// the same address the sender was bound to.
    #[test]
    fn a_receive_right_survives_being_passed_as_a_descriptor() {
        let mailbox = Receiver::bind().expect("mailbox");
        let moved = Receiver::bind().expect("the right being moved");
        let address = moved.address();

        let fd = moved.into_fd();
        send(mailbox.address(), b"carrying a right", &[fd]).expect("send");
        // SAFETY: the sender's own copy, no longer used - the kernel holds the
        // in-flight reference until the receive below.
        unsafe { libc::close(fd) };

        let mut message = mailbox.receive(1000).expect("receive").expect("a message");
        let carried = message.take_fd(0).expect("a right came with it");
        // SAFETY: the descriptor just received, bound to `address`.
        let adopted = unsafe { Receiver::adopt(carried, address) };
        assert_eq!(adopted.address(), address);

        send(address, b"to the new holder", &[]).expect("send to the moved right");
        let arrived = adopted.receive(1000).expect("receive").expect("a message");
        assert_eq!(arrived.bytes, b"to the new holder");
    }

    #[test]
    fn a_message_too_large_for_the_caller_is_kept_for_the_next_receive() {
        let port = Receiver::bind().expect("bind");
        send(port.address(), b"first", &[]).expect("send");
        let held = port.receive(1000).expect("receive").expect("a message");
        port.hold(held);
        assert!(port.ready());
        let again = port.receive(0).expect("receive").expect("the held message");
        assert_eq!(again.bytes, b"first");
    }
}
