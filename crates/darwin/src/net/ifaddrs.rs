//! `getifaddrs`, whose structure matches and whose contents do not.
//!
//! `struct ifaddrs` has the same fields at the same offsets on both platforms,
//! which is exactly why this is not a passthrough: the list is full of
//! `struct sockaddr`s, and those are the ones whose family byte moves. A guest
//! walking the host's list reads every interface's family as `AF_UNSPEC` and
//! decides it has no addresses.
//!
//! So the list is copied rather than handed over, and [`freeifaddrs`] frees this
//! copy. An interface whose address is of a family Darwin has no number for -
//! `AF_PACKET`, which is where the link-layer addresses are here - keeps its
//! entry and reports `AF_UNSPEC`, because dropping it would renumber the list
//! the guest is walking.

use std::ffi::{CStr, c_char, c_void};

use super::addr;

/// `struct ifaddrs`, identical in shape on both platforms.
#[repr(C)]
struct IfAddrs {
    next: *mut IfAddrs,
    name: *mut c_char,
    flags: u32,
    addr: *mut c_void,
    netmask: *mut c_void,
    dstaddr: *mut c_void,
    data: *mut c_void,
}

/// One entry, as a single allocation: the node, its name, then up to three
/// addresses, so [`freeifaddrs`] is one `free` per entry.
///
/// # Safety
///
/// `host` is a live host `struct ifaddrs`.
unsafe fn node_from_host(host: &libc::ifaddrs) -> Option<*mut IfAddrs> {
    // SAFETY: the host's own name, which is NUL-terminated.
    let name: &[u8] = match host.ifa_name.is_null() {
        true => b"\0",
        false => unsafe { CStr::from_ptr(host.ifa_name) }.to_bytes_with_nul(),
    };
    let mut converted = [None, None, None];
    for (slot, from) in converted
        .iter_mut()
        .zip([host.ifa_addr, host.ifa_netmask, host.ifa_ifu])
    {
        if from.is_null() {
            continue;
        }
        // A `sockaddr` in a list carries no length of its own here, so the
        // family says how long it is. An unknown family is copied at the size of
        // the generic `sockaddr`, which is all a guest can read of it anyway.
        // SAFETY: the host's own address, live for as long as its list is.
        let family = unsafe { (*from).sa_family } as i32;
        let len = match family {
            libc::AF_INET => 16u32,
            libc::AF_INET6 => 28,
            _ => 16,
        };
        // SAFETY: `len` bytes of an address the host reported.
        *slot = unsafe { addr::darwin_from_host(from.cast(), len) };
    }

    let addr_bytes: usize = converted.iter().flatten().map(|(_, len)| *len).sum();
    let total = core::mem::size_of::<IfAddrs>() + name.len() + addr_bytes;
    // SAFETY: a non-zero size; null is checked.
    let block = unsafe { libc::malloc(total) }.cast::<u8>();
    if block.is_null() {
        return None;
    }
    let node = block.cast::<IfAddrs>();
    // SAFETY: `block` is `total` bytes, and the name and the three addresses are
    // written end to end inside it, each within the size computed above.
    unsafe {
        let name_at = block.add(core::mem::size_of::<IfAddrs>());
        core::ptr::copy_nonoverlapping(name.as_ptr(), name_at, name.len());
        let mut at = name_at.add(name.len());
        let mut written = [core::ptr::null_mut::<c_void>(); 3];
        for (slot, address) in written.iter_mut().zip(converted.iter()) {
            let Some((bytes, len)) = address else {
                continue;
            };
            core::ptr::copy_nonoverlapping(bytes.as_ptr(), at, *len);
            *slot = at.cast();
            at = at.add(*len);
        }
        node.write(IfAddrs {
            next: core::ptr::null_mut(),
            name: name_at.cast(),
            flags: host.ifa_flags,
            addr: written[0],
            netmask: written[1],
            dstaddr: written[2],
            // Darwin's `ifa_data` is a `struct if_data` this host has no
            // counterpart for; null rather than a pointer to the wrong shape.
            data: core::ptr::null_mut(),
        });
    }
    Some(node)
}

/// `int getifaddrs(struct ifaddrs **)`
///
/// # Safety
///
/// `out` is a writable pointer slot.
unsafe extern "C" fn getifaddrs(out: *mut *mut IfAddrs) -> i32 {
    if out.is_null() {
        crate::errno::set(libc::EFAULT);
        return -1;
    }
    let mut list: *mut libc::ifaddrs = core::ptr::null_mut();
    // SAFETY: a live slot this function owns.
    if unsafe { libc::getifaddrs(&mut list) } != 0 {
        return -1;
    }

    let mut head: *mut IfAddrs = core::ptr::null_mut();
    let mut tail: *mut IfAddrs = core::ptr::null_mut();
    let mut at = list;
    while !at.is_null() {
        // SAFETY: a node of the list the host just returned.
        let Some(node) = (unsafe { node_from_host(&*at) }) else {
            // SAFETY: the host's list, and the partial copy built here.
            unsafe {
                libc::freeifaddrs(list);
                free_list(head);
                *out = core::ptr::null_mut();
            }
            crate::errno::set(libc::ENOMEM);
            return -1;
        };
        match tail.is_null() {
            true => head = node,
            // SAFETY: a node this loop allocated and still owns.
            false => unsafe { (*tail).next = node },
        }
        tail = node;
        // SAFETY: as above.
        at = unsafe { (*at).ifa_next };
    }
    // SAFETY: the host's list, now copied.
    unsafe { libc::freeifaddrs(list) };
    // SAFETY: a writable slot, per the caller's guarantee.
    unsafe { *out = head };
    0
}

/// # Safety
///
/// `head` is null or a list this module allocated.
unsafe fn free_list(head: *mut IfAddrs) {
    let mut at = head;
    while !at.is_null() {
        // SAFETY: a node allocated as one block, whose `next` is read before the
        // block goes away.
        unsafe {
            let next = (*at).next;
            libc::free(at.cast());
            at = next;
        }
    }
}

/// `void freeifaddrs(struct ifaddrs *)`
///
/// # Safety
///
/// `head` is null or a list [`getifaddrs`] returned.
unsafe extern "C" fn freeifaddrs(head: *mut IfAddrs) {
    // SAFETY: the caller's guarantee.
    unsafe { free_list(head) };
}

pub const NAMES: &[&str] = &["getifaddrs", "freeifaddrs"];

pub fn symbol(name: &str) -> Option<u64> {
    let f: *const () = match name {
        "getifaddrs" => getifaddrs as *const (),
        "freeifaddrs" => freeifaddrs as *const (),
        _ => return None,
    };
    Some(f as usize as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every host has a loopback, and its address must come back in Darwin's
    /// shape - which is the whole reason the list is copied.
    #[test]
    fn the_loopback_is_reported_with_a_darwin_family() {
        let mut list: *mut IfAddrs = core::ptr::null_mut();
        // SAFETY: a live slot this test owns.
        assert_eq!(unsafe { getifaddrs(&mut list) }, 0);
        assert!(!list.is_null());

        let mut seen_inet = false;
        let mut at = list;
        while !at.is_null() {
            // SAFETY: a node of the list just returned.
            unsafe {
                if !(*at).addr.is_null() {
                    let bytes = (*at).addr.cast::<u8>();
                    let family = i32::from(bytes.add(1).read());
                    if family == addr::family::INET {
                        seen_inet = true;
                        assert_eq!(bytes.read(), 16, "Darwin's sin_len");
                    }
                }
                at = (*at).next;
            }
        }
        assert!(seen_inet, "no interface reported an IPv4 address");
        // SAFETY: the list this test was given.
        unsafe { freeifaddrs(list) };
    }
}
