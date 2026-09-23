//! Ancillary data: the `msg_control` buffer of a `sendmsg` or a `recvmsg`.
//!
//! The one part of a `struct msghdr` whose *contents* diverge rather than its
//! shape. `struct cmsghdr` is
//! `{ uint32_t cmsg_len; int cmsg_level; int cmsg_type; }` on Darwin - twelve
//! bytes, padded to four - and `{ size_t cmsg_len; int; int; }` here - sixteen,
//! padded to eight. So the payload of a Darwin control message starts four bytes
//! before this host looks for it, and the length it reads is the length *and*
//! the level.
//!
//! The types diverge on top of that, in the way that collides: `SCM_RIGHTS` is 1
//! on both, but Darwin's `SCM_TIMESTAMP` is 2, which is `SCM_CREDENTIALS` here,
//! and Darwin's `SCM_CREDS` is 3, which is `SCM_SECURITY` here. A descriptor
//! array handed over unread would be read as a credentials structure.
//!
//! Known types are translated and the rest are refused by number, because a
//! payload is only convertible if this module knows what is in it:
//!
//! - `SCM_RIGHTS`, whose payload is an array of `int` and is the same array on
//!   both. This is the one that matters - it is how a descriptor is passed over
//!   a socket, which is what a helper process is given its side of a connection
//!   with.
//! - `SCM_TIMESTAMP` on the way back, whose payload is a `struct timeval` and so
//!   diverges again in the width of `tv_usec`; see [`crate::net::sockopt`] for
//!   the same conversion one call over.

//! - IP TOS / IPv6 traffic class for ECN, and IPv4/IPv6 packet information.
//!   Packet information has the same field layout on both platforms.

use std::ffi::c_void;

/// Darwin's `struct cmsghdr`.
#[derive(Clone, Copy)]
#[repr(C)]
struct DarwinControlHeader {
    len: u32,
    level: i32,
    kind: i32,
}

impl DarwinControlHeader {
    /// # Safety
    ///
    /// `at` is readable for one potentially unaligned header.
    unsafe fn read(at: *const u8) -> Self {
        unsafe { at.cast::<Self>().read_unaligned() }
    }

    /// # Safety
    ///
    /// `at` is writable for one potentially unaligned header.
    unsafe fn write(self, at: *mut u8) {
        unsafe { at.cast::<Self>().write_unaligned(self) }
    }
}

const DARWIN_HDR: usize = core::mem::size_of::<DarwinControlHeader>();
const _: () = assert!(DARWIN_HDR == 12);

/// Darwin's `CMSG_ALIGN`, which rounds to `sizeof(uint32_t)`.
fn darwin_align(len: usize) -> usize {
    len.next_multiple_of(4)
}

/// This host's `CMSG_ALIGN`, which rounds to `sizeof(size_t)`.
fn host_align(len: usize) -> usize {
    len.next_multiple_of(core::mem::size_of::<usize>())
}

/// Darwin's `SOL_SOCKET`.
const DARWIN_SOL_SOCKET: i32 = 0xffff;

const DARWIN_IP_TOS: i32 = 3;
// Not reachable on a host with no `IP_PKTINFO` of its own, and still named:
// the point of this list is that it is the whole set Darwin can send.
#[allow(dead_code)]
const DARWIN_IP_PKTINFO: i32 = 26;
const DARWIN_IP_RECVTOS: i32 = 27;
const DARWIN_IPV6_TCLASS: i32 = 36;
const DARWIN_IPV6_PKTINFO: i32 = 46;

const SCM_RIGHTS: i32 = 0x01;
/// Darwin's `SCM_TIMESTAMP`, which is this host's `SCM_CREDENTIALS`.
const DARWIN_SCM_TIMESTAMP: i32 = 0x02;

/// A `(level, type)` this module cannot convert, which the caller names.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Untranslatable {
    pub level: i32,
    pub kind: i32,
}

/// One control message, as its header fields and the bytes of its payload.
struct Message {
    level: i32,
    kind: i32,
    payload: Vec<u8>,
}

/// The guest's control buffer as this host's, or the first message in it that
/// cannot be converted.
///
/// # Safety
///
/// `control` is `len` readable bytes, or is null with `len` zero.
pub unsafe fn to_host(control: *const c_void, len: u32) -> Result<Vec<u8>, Untranslatable> {
    let mut out = Vec::new();
    let mut at = 0usize;
    let len = len as usize;
    while at + DARWIN_HDR <= len {
        // SAFETY: the caller's bytes, read a header at a time and never past
        // the length they gave. Unaligned because a guest's control buffer is
        // only ever four-byte aligned.
        let header = unsafe { DarwinControlHeader::read(control.cast::<u8>().add(at)) };
        let message_len = header.len as usize;
        let level = header.level;
        let kind = header.kind;
        if message_len < DARWIN_HDR || message_len > len - at {
            // A length that does not describe a message ends the walk, which is
            // what the kernel's own `CMSG_NXTHDR` does with one.
            break;
        }
        let payload_len = message_len - DARWIN_HDR;
        // SAFETY: the header and its payload are within the caller's buffer.
        let payload = unsafe {
            core::slice::from_raw_parts(control.cast::<u8>().add(at + DARWIN_HDR), payload_len)
        };
        let host_kind = match (level, kind, payload.len()) {
            (DARWIN_SOL_SOCKET, SCM_RIGHTS, _) => SCM_RIGHTS,
            (libc::IPPROTO_IP, DARWIN_IP_TOS, 1 | 4) => libc::IP_TOS,
            // FreeBSD has no `IP_PKTINFO`, and nothing there carries Darwin's
            // `struct in_pktinfo`, so the message is refused rather than
            // rewritten into a different shape.
            #[cfg(not(target_os = "freebsd"))]
            (libc::IPPROTO_IP, DARWIN_IP_PKTINFO, 12) => libc::IP_PKTINFO,
            (libc::IPPROTO_IPV6, DARWIN_IPV6_TCLASS, 4) => libc::IPV6_TCLASS,
            (libc::IPPROTO_IPV6, DARWIN_IPV6_PKTINFO, 20) => libc::IPV6_PKTINFO,
            _ => return Err(Untranslatable { level, kind }),
        };
        let host_level = if level == DARWIN_SOL_SOCKET {
            libc::SOL_SOCKET
        } else {
            level
        };
        push_host(&mut out, host_level, host_kind, |into| {
            into.extend_from_slice(payload);
        });
        at += darwin_align(message_len);
    }
    Ok(out)
}

/// Append one host control message, whose payload `fill` writes.
fn push_host(out: &mut Vec<u8>, level: i32, kind: i32, fill: impl FnOnce(&mut Vec<u8>)) {
    let header = out.len();
    out.extend_from_slice(&[0; core::mem::size_of::<libc::cmsghdr>()]);
    fill(out);
    let len = out.len() - header;
    let mut value: libc::cmsghdr = unsafe { core::mem::zeroed() };
    value.cmsg_len = len as _;
    value.cmsg_level = level;
    value.cmsg_type = kind;
    // SAFETY: the placeholder above reserved a complete host header. A byte
    // buffer provides no alignment guarantee, so write the structure unaligned.
    unsafe {
        out.as_mut_ptr()
            .add(header)
            .cast::<libc::cmsghdr>()
            .write_unaligned(value)
    };
    out.resize(header + host_align(len), 0);
}

/// This host's control messages in Darwin's shape.
///
/// `None` for a message this module cannot convert; the caller decides what to
/// do with the rest, which for a received `SCM_RIGHTS` means closing the
/// descriptors rather than leaking them.
fn message_from_host(level: i32, kind: i32, payload: &[u8]) -> Option<Message> {
    match (level, kind) {
        (libc::SOL_SOCKET, SCM_RIGHTS) => Some(Message {
            level: DARWIN_SOL_SOCKET,
            kind: SCM_RIGHTS,
            payload: payload.to_vec(),
        }),
        (libc::SOL_SOCKET, libc::SCM_TIMESTAMP) if payload.len() >= 16 => {
            // Darwin's `tv_usec` is a 32-bit `int` followed by four bytes of
            // padding where this host's is a 64-bit `long`.
            let seconds = &payload[0..8];
            let micros = i64::from_ne_bytes(payload[8..16].try_into().ok()?) as i32;
            let mut darwin = seconds.to_vec();
            darwin.extend_from_slice(&micros.to_ne_bytes());
            darwin.extend_from_slice(&[0; 4]);
            Some(Message {
                level: DARWIN_SOL_SOCKET,
                kind: DARWIN_SCM_TIMESTAMP,
                payload: darwin,
            })
        }
        (libc::IPPROTO_IP, libc::IP_TOS) if payload.len() == 1 => Some(Message {
            level,
            // Darwin receives IP_RECVTOS, not the IP_TOS used on send.
            kind: DARWIN_IP_RECVTOS,
            payload: payload.to_vec(),
        }),
        // As above; there is no FreeBSD message to recognise here.
        #[cfg(not(target_os = "freebsd"))]
        (libc::IPPROTO_IP, libc::IP_PKTINFO) if payload.len() == 12 => Some(Message {
            level,
            kind: DARWIN_IP_PKTINFO,
            payload: payload.to_vec(),
        }),
        (libc::IPPROTO_IPV6, libc::IPV6_TCLASS) if payload.len() == 4 => Some(Message {
            level,
            kind: DARWIN_IPV6_TCLASS,
            payload: payload.to_vec(),
        }),
        (libc::IPPROTO_IPV6, libc::IPV6_PKTINFO) if payload.len() == 20 => Some(Message {
            level,
            kind: DARWIN_IPV6_PKTINFO,
            payload: payload.to_vec(),
        }),
        _ => None,
    }
}

/// Close the descriptors a received `SCM_RIGHTS` carries.
///
/// The kernel has already installed them in this process, so a message that is
/// dropped - refused, or too large for the guest's buffer - leaks one
/// descriptor per entry unless they are closed here.
fn discard(level: i32, kind: i32, payload: &[u8]) {
    if (level, kind) != (libc::SOL_SOCKET, SCM_RIGHTS) {
        return;
    }
    for fd in payload.chunks_exact(4) {
        // SAFETY: a descriptor this process was just given and will not use.
        unsafe { libc::close(i32::from_ne_bytes(fd.try_into().unwrap())) };
    }
}

/// Write the control messages `host` received into the guest's buffer.
///
/// Answers with the bytes written and whether anything was dropped, which the
/// caller reports as `MSG_CTRUNC` - the same thing the kernel says when a
/// control buffer was too small, and the only word this interface has for it.
///
/// # Safety
///
/// `host` is a `msghdr` a `recvmsg` has just filled in, and `control` is
/// `capacity` writable bytes or is null with `capacity` zero.
pub unsafe fn from_host(host: &libc::msghdr, control: *mut c_void, capacity: u32) -> (u32, bool) {
    let mut written = 0usize;
    let mut dropped = false;
    let capacity = capacity as usize;

    // SAFETY: the caller's guarantee; the walk is the kernel's own, which is
    // what bounds it to the `msg_controllen` the kernel wrote.
    let mut entry = unsafe { libc::CMSG_FIRSTHDR(host) };
    while !entry.is_null() {
        // SAFETY: an entry the walk produced, whose `cmsg_len` covers its data.
        let (level, kind, payload) = unsafe {
            let len = (*entry).cmsg_len as usize;
            (
                (*entry).cmsg_level,
                (*entry).cmsg_type,
                core::slice::from_raw_parts(
                    libc::CMSG_DATA(entry),
                    len.saturating_sub(libc::CMSG_LEN(0) as usize),
                ),
            )
        };
        match message_from_host(level, kind, payload) {
            Some(message) if written + DARWIN_HDR + message.payload.len() <= capacity => {
                let len = DARWIN_HDR + message.payload.len();
                // SAFETY: `len` bytes within the caller's capacity, just
                // checked, written at Darwin's offsets and unaligned because
                // the guest's buffer need only be four-byte aligned.
                unsafe {
                    let at = control.cast::<u8>().add(written);
                    DarwinControlHeader {
                        len: len as u32,
                        level: message.level,
                        kind: message.kind,
                    }
                    .write(at);
                    core::ptr::copy_nonoverlapping(
                        message.payload.as_ptr(),
                        at.add(DARWIN_HDR),
                        message.payload.len(),
                    );
                }
                // The padding is not written: it is not part of the message,
                // and the next one starting where the alignment says is the
                // only thing that depends on it.
                written = (written + darwin_align(len)).min(capacity);
            }
            _ => {
                discard(level, kind, payload);
                dropped = true;
            }
        }
        // SAFETY: as above.
        entry = unsafe { libc::CMSG_NXTHDR(host, entry) };
    }
    (written as u32, dropped)
}

/// Room for the host's answer to a guest that reserved `capacity` bytes.
///
/// Each message grows by four bytes of header and up to four more of padding,
/// so the guest's own capacity is not enough to receive into.
pub fn host_capacity(capacity: u32) -> usize {
    capacity as usize * 2 + core::mem::size_of::<libc::cmsghdr>()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn guest_message(level: i32, kind: i32, payload: &[u8]) -> Vec<u8> {
        let len = DARWIN_HDR + payload.len();
        let mut out = (len as u32).to_ne_bytes().to_vec();
        out.extend_from_slice(&level.to_ne_bytes());
        out.extend_from_slice(&kind.to_ne_bytes());
        out.extend_from_slice(payload);
        out.resize(darwin_align(len), 0);
        out
    }

    #[test]
    fn quic_control_messages_preserve_payloads_and_translate_types() {
        for (level, send_kind, recv_kind, host_kind, payload) in [
            (
                libc::IPPROTO_IP,
                DARWIN_IP_TOS,
                DARWIN_IP_RECVTOS,
                libc::IP_TOS,
                vec![3],
            ),
            // No FreeBSD counterpart; see `to_host`.
            #[cfg(not(target_os = "freebsd"))]
            (
                libc::IPPROTO_IP,
                DARWIN_IP_PKTINFO,
                DARWIN_IP_PKTINFO,
                libc::IP_PKTINFO,
                vec![1, 0, 0, 0, 127, 0, 0, 1, 127, 0, 0, 2],
            ),
            (
                libc::IPPROTO_IPV6,
                DARWIN_IPV6_TCLASS,
                DARWIN_IPV6_TCLASS,
                libc::IPV6_TCLASS,
                3i32.to_ne_bytes().to_vec(),
            ),
            (
                libc::IPPROTO_IPV6,
                DARWIN_IPV6_PKTINFO,
                DARWIN_IPV6_PKTINFO,
                libc::IPV6_PKTINFO,
                (0..20).collect(),
            ),
        ] {
            let guest = guest_message(level, send_kind, &payload);
            // SAFETY: guest is a complete control buffer.
            let host = unsafe { to_host(guest.as_ptr().cast(), guest.len() as u32) }.unwrap();
            assert_eq!(i32::from_ne_bytes(host[8..12].try_into().unwrap()), level);
            assert_eq!(
                i32::from_ne_bytes(host[12..16].try_into().unwrap()),
                host_kind
            );
            assert_eq!(&host[16..16 + payload.len()], payload);
            let received = message_from_host(level, host_kind, &payload).unwrap();
            assert_eq!((received.level, received.kind), (level, recv_kind));
            assert_eq!(received.payload, payload);

            let short = guest_message(level, send_kind, &[]);
            // SAFETY: short is a complete header with an empty payload.
            assert!(unsafe { to_host(short.as_ptr().cast(), short.len() as u32) }.is_err());
            assert!(message_from_host(level, host_kind, &[]).is_none());
        }
    }

    /// The host and guest option numbers for one address family, or `None`
    /// where this host cannot be asked for what the test needs: FreeBSD has no
    /// IPv4 `IP_PKTINFO`, so only the IPv6 half of this runs there.
    fn family_options(ipv6: bool) -> Option<(i32, i32, i32, i32, i32, i32)> {
        if ipv6 {
            return Some((
                libc::IPPROTO_IPV6,
                libc::IPV6_RECVTCLASS,
                libc::IPV6_RECVPKTINFO,
                DARWIN_IPV6_TCLASS,
                DARWIN_IPV6_TCLASS,
                DARWIN_IPV6_PKTINFO,
            ));
        }
        #[cfg(target_os = "freebsd")]
        return None;
        #[cfg(not(target_os = "freebsd"))]
        Some((
            libc::IPPROTO_IP,
            libc::IP_RECVTOS,
            libc::IP_PKTINFO,
            DARWIN_IP_TOS,
            DARWIN_IP_RECVTOS,
            DARWIN_IP_PKTINFO,
        ))
    }

    #[test]
    fn udp_ecn_and_packet_info_reach_the_guest() {
        use std::os::fd::AsRawFd;
        for ipv6 in [false, true] {
            let Some((level, recv_ecn, recv_info, send_kind, ecn_kind, info_kind)) =
                family_options(ipv6)
            else {
                continue;
            };
            let address = if ipv6 { "[::1]:0" } else { "127.0.0.1:0" };
            let receiver = std::net::UdpSocket::bind(address).unwrap();
            receiver
                .set_read_timeout(Some(std::time::Duration::from_secs(2)))
                .unwrap();
            let sender = std::net::UdpSocket::bind(address).unwrap();
            sender.connect(receiver.local_addr().unwrap()).unwrap();
            let enabled = 1i32;
            for option in [recv_ecn, recv_info] {
                // SAFETY: a live socket and a readable integer option.
                assert_eq!(
                    unsafe {
                        libc::setsockopt(
                            receiver.as_raw_fd(),
                            level,
                            option,
                            (&enabled as *const i32).cast(),
                            4,
                        )
                    },
                    0
                );
            }
            let guest = guest_message(level, send_kind, &3i32.to_ne_bytes());
            // SAFETY: all message buffers below are live for their respective calls.
            unsafe {
                let mut control = to_host(guest.as_ptr().cast(), guest.len() as u32).unwrap();
                let mut byte = 42u8;
                let mut iov = libc::iovec {
                    iov_base: (&mut byte as *mut u8).cast(),
                    iov_len: 1,
                };
                let mut msg: libc::msghdr = core::mem::zeroed();
                msg.msg_iov = &mut iov;
                msg.msg_iovlen = 1;
                msg.msg_control = control.as_mut_ptr().cast();
                msg.msg_controllen = control.len() as _;
                assert_eq!(libc::sendmsg(sender.as_raw_fd(), &msg, 0), 1);
                let mut incoming = vec![0u8; 256];
                msg.msg_control = incoming.as_mut_ptr().cast();
                msg.msg_controllen = incoming.len() as _;
                assert_eq!(libc::recvmsg(receiver.as_raw_fd(), &mut msg, 0), 1);
                let mut output = [0u8; 128];
                let (len, dropped) =
                    from_host(&msg, output.as_mut_ptr().cast(), output.len() as u32);
                assert!(!dropped);
                let mut at = 0;
                let mut kinds = Vec::new();
                while at < len as usize {
                    let size = u32::from_ne_bytes(output[at..at + 4].try_into().unwrap()) as usize;
                    let kind = i32::from_ne_bytes(output[at + 8..at + 12].try_into().unwrap());
                    kinds.push(kind);
                    if kind == ecn_kind {
                        assert_eq!(output[at + 12], 3);
                    }
                    at += darwin_align(size);
                }
                assert!(kinds.contains(&ecn_kind));
                assert!(kinds.contains(&info_kind));
                let (_, dropped) = from_host(&msg, output.as_mut_ptr().cast(), 12);
                assert!(dropped);
            }
        }
    }

    /// The claim the whole module rests on, as an assertion.
    #[test]
    fn the_guests_cmsghdr_is_not_the_hosts() {
        assert_eq!(DARWIN_HDR, 12);
        assert_eq!(core::mem::size_of::<libc::cmsghdr>(), 16);
        assert_eq!(darwin_align(13), 16);
        assert_eq!(host_align(13), 16);
        assert_eq!(darwin_align(20), 20);
        assert_eq!(host_align(20), 24);
    }

    /// A type Darwin numbers and this host numbers differently is refused by
    /// number rather than converted into whatever it means here.
    #[test]
    fn a_type_this_module_cannot_convert_is_named_rather_than_guessed_at() {
        // Darwin's `SCM_CREDS`, whose payload is BSD's `struct cmsgcred` and
        // this host's 3 is `SCM_SECURITY`.
        let mut guest = [0u8; 16];
        guest[0..4].copy_from_slice(&16u32.to_ne_bytes());
        guest[4..8].copy_from_slice(&DARWIN_SOL_SOCKET.to_ne_bytes());
        guest[8..12].copy_from_slice(&3i32.to_ne_bytes());
        // SAFETY: sixteen readable bytes, and the length says sixteen.
        assert_eq!(
            unsafe { to_host(guest.as_ptr().cast(), 16) },
            Err(Untranslatable {
                level: DARWIN_SOL_SOCKET,
                kind: 3
            })
        );
    }

    /// A descriptor array converts into one this host's kernel will read: the
    /// header grows by four bytes and the payload does not move relative to it.
    #[test]
    fn a_descriptor_array_lands_where_this_hosts_kernel_looks_for_it() {
        let mut guest = [0u8; 20];
        guest[0..4].copy_from_slice(&16u32.to_ne_bytes());
        guest[4..8].copy_from_slice(&DARWIN_SOL_SOCKET.to_ne_bytes());
        guest[8..12].copy_from_slice(&SCM_RIGHTS.to_ne_bytes());
        guest[12..16].copy_from_slice(&7i32.to_ne_bytes());

        // SAFETY: sixteen readable bytes of a twenty-byte array.
        let host = unsafe { to_host(guest.as_ptr().cast(), 16) }.expect("SCM_RIGHTS converts");
        assert_eq!(host.len(), unsafe { libc::CMSG_SPACE(4) } as usize);
        assert_eq!(
            usize::from_ne_bytes(host[0..8].try_into().unwrap()),
            unsafe { libc::CMSG_LEN(4) } as usize
        );
        assert_eq!(
            i32::from_ne_bytes(host[8..12].try_into().unwrap()),
            libc::SOL_SOCKET
        );
        assert_eq!(
            i32::from_ne_bytes(host[12..16].try_into().unwrap()),
            SCM_RIGHTS
        );
        assert_eq!(i32::from_ne_bytes(host[16..20].try_into().unwrap()), 7);
    }
}
