//! Address families and the socket addresses that carry them.
//!
//! `struct sockaddr_in` is sixteen bytes on both platforms and
//! `struct sockaddr_in6` twenty-eight, and every field from byte two onward -
//! port, address, flow label, scope - is at the same offset and in the same
//! order. Only the first two bytes differ, and they differ completely:
//!
//! ```text
//! Darwin   uint8_t sa_len; uint8_t sa_family;   ->  [0x10, 0x02] for AF_INET
//! Linux    uint16_t sa_family;                  ->  [0x02, 0x00] for AF_INET
//! ```
//!
//! So a Darwin `sockaddr` handed to glibc has its **family read out of the
//! length byte** - `AF_INET` becomes 16, which is `AF_NETLINK` here - and one
//! filled in by glibc and read by the guest has family `AF_UNSPEC`. Neither
//! crashes; a `connect` simply fails with `EAFNOSUPPORT`, and a `getsockname`
//! reports an address of no family at all.
//!
//! The family *numbers* diverge on top of that: `AF_INET6` is 30 on Darwin and
//! 10 here, and 30 here is `AF_TIPC`. This is the collision class again - the
//! value exists on both sides and means something else.

use std::ffi::c_void;

/// `sizeof(struct sockaddr_storage)`, 128 on both platforms.
pub const STORAGE: usize = 128;

/// Darwin's `AF_*`, from `<sys/socket.h>`.
pub mod family {
    pub const UNSPEC: i32 = 0;
    pub const UNIX: i32 = 1;
    pub const INET: i32 = 2;
    pub const INET6: i32 = 30;
}

/// A guest's address family as this host's, or `None` for one it has not got.
///
/// Refused rather than passed through: Darwin's `AF_INET6` is a family this host
/// also defines, for something else.
pub fn family_to_host(darwin: i32) -> Option<i32> {
    Some(match darwin {
        family::UNSPEC => libc::AF_UNSPEC,
        family::UNIX => libc::AF_UNIX,
        family::INET => libc::AF_INET,
        family::INET6 => libc::AF_INET6,
        _ => return None,
    })
}

/// This host's address family as a guest's.
pub fn family_from_host(host: i32) -> Option<i32> {
    Some(match host {
        libc::AF_UNSPEC => family::UNSPEC,
        libc::AF_UNIX => family::UNIX,
        libc::AF_INET => family::INET,
        libc::AF_INET6 => family::INET6,
        _ => return None,
    })
}

/// A socket address in this host's shape, with the length the host wants.
///
/// Owned by value so a caller can hand `libc` a pointer to it without a second
/// allocation; `STORAGE` bytes is what `struct sockaddr_storage` reserves and is
/// the most any address either side defines needs.
#[repr(C, align(8))]
pub struct HostAddr {
    bytes: [u8; STORAGE],
    len: libc::socklen_t,
}

impl HostAddr {
    /// Room for an address the host is about to fill in.
    pub fn empty() -> HostAddr {
        HostAddr {
            bytes: [0; STORAGE],
            len: STORAGE as libc::socklen_t,
        }
    }

    pub fn as_ptr(&self) -> *const libc::sockaddr {
        self.bytes.as_ptr().cast()
    }

    pub fn as_mut_ptr(&mut self) -> *mut libc::sockaddr {
        self.bytes.as_mut_ptr().cast()
    }

    pub fn len(&self) -> libc::socklen_t {
        self.len
    }

    /// The length slot the host writes the answer's real length into.
    pub fn len_mut(&mut self) -> *mut libc::socklen_t {
        &mut self.len
    }
}

/// A guest's `sockaddr` as this host's, or `None` for a family this host has not
/// got or a length that is not an address.
///
/// # Safety
///
/// `addr` is `len` readable bytes, or is null with `len` zero.
pub unsafe fn to_host(addr: *const c_void, len: u32) -> Option<HostAddr> {
    if addr.is_null() || (len as usize) < 2 || len as usize > STORAGE {
        return None;
    }
    let mut out = HostAddr {
        bytes: [0; STORAGE],
        len: len as libc::socklen_t,
    };
    // SAFETY: the caller's `len` readable bytes, into a buffer of `STORAGE`
    // which `len` was just checked against.
    unsafe {
        core::ptr::copy_nonoverlapping(addr.cast::<u8>(), out.bytes.as_mut_ptr(), len as usize)
    };
    // Byte 0 is Darwin's `sa_len`, which the host has no field for; byte 1 is
    // the family, which the host reads as the low half of a sixteen-bit one.
    let host = family_to_host(i32::from(out.bytes[1]))?;
    out.bytes[0] = (host & 0xff) as u8;
    out.bytes[1] = ((host >> 8) & 0xff) as u8;
    Some(out)
}

/// A host `sockaddr` in Darwin's shape, as bytes and the length that goes with
/// them.
///
/// `None` for a null pointer or a length that is not an address. A family this
/// host has that Darwin has not becomes `AF_UNSPEC` rather than being dropped:
/// the address is still the kernel's answer, and a guest that cannot name the
/// family can still see there was one.
///
/// # Safety
///
/// `addr` is `len` readable bytes.
pub unsafe fn darwin_from_host(addr: *const c_void, len: u32) -> Option<([u8; STORAGE], usize)> {
    if addr.is_null() || (len as usize) < 2 {
        return None;
    }
    let real = (len as usize).min(STORAGE);
    let mut out = [0u8; STORAGE];
    // SAFETY: the caller's readable bytes, bounded by `STORAGE`.
    unsafe { core::ptr::copy_nonoverlapping(addr.cast::<u8>(), out.as_mut_ptr(), real) };
    let family = i32::from(out[0]) | (i32::from(out[1]) << 8);
    out[0] = real.min(u8::MAX as usize) as u8;
    out[1] = family_from_host(family).unwrap_or(family::UNSPEC) as u8;
    Some((out, real))
}

/// Write `host` into the guest's buffer in Darwin's shape.
///
/// Follows the kernel's own truncation rule: at most `*len` bytes are written
/// and `*len` is then set to the address's *real* length, so a caller that
/// passed too small a buffer can tell.
///
/// # Safety
///
/// `addr` is `*len` writable bytes and `len` is a writable `socklen_t`, or both
/// are null.
pub unsafe fn from_host(host: &HostAddr, addr: *mut c_void, len: *mut u32) {
    if addr.is_null() || len.is_null() {
        return;
    }
    // SAFETY: the caller's own slot.
    let capacity = unsafe { *len } as usize;
    let real = host.len as usize;
    // SAFETY: `host` holds `STORAGE` bytes and `len` no more than that.
    let Some((darwin, copied)) = (unsafe { darwin_from_host(host.as_ptr().cast(), host.len) })
    else {
        return;
    };

    let written = capacity.min(copied);
    // SAFETY: `written` is no more than the caller's capacity and no more than
    // the bytes prepared above.
    unsafe { core::ptr::copy_nonoverlapping(darwin.as_ptr(), addr.cast::<u8>(), written) };
    // SAFETY: the caller's own slot, as above.
    unsafe { *len = real as u32 };
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The collision this module exists for, stated as an assertion: Darwin's
    /// `AF_INET6` is a family this host also has, meaning something else.
    #[test]
    fn af_inet6_is_not_the_hosts_number() {
        assert_ne!(family::INET6, libc::AF_INET6);
        assert_eq!(family_to_host(family::INET6), Some(libc::AF_INET6));
    }

    #[test]
    fn a_family_this_host_has_not_got_is_refused() {
        // Darwin's `AF_LINK`, which is not this host's `AF_PACKET`.
        assert_eq!(family_to_host(18), None);
    }

    /// A guest's IPv4 address, converted and converted back, is the address it
    /// started as - port and octets included, which is what a raw pass-through
    /// gets right and the family byte does not.
    #[test]
    fn an_ipv4_address_round_trips_with_its_family_intact() {
        // `sin_len`, `sin_family`, port 443, 192.0.2.7, eight zero bytes.
        let mut guest = [0u8; 16];
        guest[0] = 16;
        guest[1] = family::INET as u8;
        guest[2..4].copy_from_slice(&443u16.to_be_bytes());
        guest[4..8].copy_from_slice(&[192, 0, 2, 7]);

        // SAFETY: sixteen readable bytes, and the length says sixteen.
        let host = unsafe { to_host(guest.as_ptr().cast(), 16) }.expect("a known family");
        assert_eq!(host.bytes[0], libc::AF_INET as u8);
        assert_eq!(host.bytes[1], 0);
        assert_eq!(&host.bytes[2..8], &guest[2..8]);

        let mut back = [0u8; 16];
        let mut len = 16u32;
        // SAFETY: sixteen writable bytes and a live length.
        unsafe { from_host(&host, back.as_mut_ptr().cast(), &mut len) };
        assert_eq!(len, 16);
        assert_eq!(back, guest);
    }

    /// Everything past the family is the same bytes at the same offsets, which
    /// is why only the first two are rewritten.
    #[test]
    fn an_ipv6_address_keeps_its_flow_label_and_scope() {
        let mut guest = [0u8; 28];
        guest[0] = 28;
        guest[1] = family::INET6 as u8;
        for (i, b) in guest[2..28].iter_mut().enumerate() {
            *b = (i as u8).wrapping_add(1);
        }
        // SAFETY: twenty-eight readable bytes, and the length matches.
        let host = unsafe { to_host(guest.as_ptr().cast(), 28) }.expect("a known family");
        assert_eq!(&host.bytes[2..28], &guest[2..28]);

        let mut back = [0u8; 28];
        let mut len = 28u32;
        // SAFETY: twenty-eight writable bytes and a live length.
        unsafe { from_host(&host, back.as_mut_ptr().cast(), &mut len) };
        assert_eq!(back, guest);
    }

    /// Too small a buffer is truncated and the real length reported, which is
    /// how a caller of `getsockname` learns it asked for too little.
    #[test]
    fn a_short_buffer_is_truncated_and_the_real_length_reported() {
        let mut host = HostAddr::empty();
        host.bytes[0] = libc::AF_INET as u8;
        host.len = 16;

        let mut back = [0xAAu8; 16];
        let mut len = 4u32;
        // SAFETY: sixteen writable bytes, of which the call may use four.
        unsafe { from_host(&host, back.as_mut_ptr().cast(), &mut len) };
        assert_eq!(len, 16, "the real length, not the truncated one");
        assert_eq!(back[1], family::INET as u8);
        assert_eq!(back[4..], [0xAA; 12], "wrote past the caller's length");
    }
}
