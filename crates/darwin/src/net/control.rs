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
//! Two types are translated and the rest are refused by number, because a
//! payload is only convertible if this module knows what is in it:
//!
//! - `SCM_RIGHTS`, whose payload is an array of `int` and is the same array on
//!   both. This is the one that matters - it is how a descriptor is passed over
//!   a socket, which is what a helper process is given its side of a connection
//!   with.
//! - `SCM_TIMESTAMP` on the way back, whose payload is a `struct timeval` and so
//!   diverges again in the width of `tv_usec`; see [`crate::net::sockopt`] for
//!   the same conversion one call over.

use std::ffi::c_void;

/// `sizeof(struct cmsghdr)` on Darwin.
const DARWIN_HDR: usize = 12;

/// Darwin's `CMSG_ALIGN`, which rounds to `sizeof(uint32_t)`.
fn darwin_align(len: usize) -> usize {
    len.next_multiple_of(4)
}

/// This host's `CMSG_ALIGN`, which rounds to `sizeof(size_t)`.
fn host_align(len: usize) -> usize {
    len.next_multiple_of(core::mem::size_of::<usize>())
}

/// Darwin's `SOL_SOCKET`, the only level either platform sends these at.
const DARWIN_SOL_SOCKET: i32 = 0xffff;

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
        let (message_len, level, kind) = unsafe {
            let at = control.cast::<u8>().add(at);
            (
                at.cast::<u32>().read_unaligned() as usize,
                at.add(4).cast::<i32>().read_unaligned(),
                at.add(8).cast::<i32>().read_unaligned(),
            )
        };
        if message_len < DARWIN_HDR || message_len > len - at {
            // A length that does not describe a message ends the walk, which is
            // what the kernel's own `CMSG_NXTHDR` does with one.
            break;
        }
        if level != DARWIN_SOL_SOCKET || kind != SCM_RIGHTS {
            return Err(Untranslatable { level, kind });
        }
        let payload = message_len - DARWIN_HDR;
        // SAFETY: `payload` bytes after the header, both within `len`.
        let data = unsafe { control.cast::<u8>().add(at + DARWIN_HDR) };
        push_host(&mut out, libc::SOL_SOCKET, SCM_RIGHTS, |into| {
            // SAFETY: as above; the descriptor array is the same array here.
            into.extend_from_slice(unsafe { core::slice::from_raw_parts(data, payload) });
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
    out[header..header + core::mem::size_of::<usize>()].copy_from_slice(&len.to_ne_bytes());
    let fields = header + core::mem::size_of::<usize>();
    out[fields..fields + 4].copy_from_slice(&level.to_ne_bytes());
    out[fields + 4..fields + 8].copy_from_slice(&kind.to_ne_bytes());
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
                    at.cast::<u32>().write_unaligned(len as u32);
                    at.add(4).cast::<i32>().write_unaligned(message.level);
                    at.add(8).cast::<i32>().write_unaligned(message.kind);
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
