//! Extended attributes, which differ in arity *and* in namespace.
//!
//! Darwin's calls take two arguments this host's do not:
//!
//! ```text
//! Darwin   getxattr(path, name, value, size, u_int32_t position, int options)
//! Linux    getxattr(path, name, value, size)
//! ```
//!
//! Passing one through leaves `position` and `options` in registers nobody
//! reads, which is harmless, and it works by accident. What does not work by
//! accident is the *name*: Darwin's attribute namespace is flat, and Linux
//! partitions it - an attribute on a regular file must be called `user.`
//! something, and one that is not is refused with `EOPNOTSUPP` no matter what
//! the filesystem supports.
//!
//! That is how this surfaced. Crashpad stores
//! `org.chromium.crashpad.database.initialized` on its database directory,
//! got `EOPNOTSUPP` for every read and every write, and reported "failed to
//! initialize crash reporter" - a filesystem answer wearing the shape of a
//! missing feature.
//!
//! So every guest name is prefixed with `user.` on the way down and the prefix
//! is taken off on the way back, which keeps `listxattr` reporting the names the
//! guest wrote. An attribute already in one of Linux's own namespaces
//! (`security.`, `system.`, `trusted.`) is not re-prefixed: the guest cannot
//! have meant one, but a name that happens to start that way must still round
//! trip to itself.

#![cfg(not(target_os = "macos"))]

use std::ffi::{CStr, CString, c_char, c_void};

/// Linux's own attribute namespaces, which are not the guest's to use.
const HOST_NAMESPACES: &[&str] = &["user.", "security.", "system.", "trusted."];

/// `XATTR_NOFOLLOW`, the one Darwin option with a host counterpart - and it has
/// one only as a different function (`lgetxattr` rather than `getxattr`).
const NOFOLLOW: i32 = 0x0001;

/// A guest's attribute name in this host's namespace.
///
/// `None` for a name that is not a valid C string, or one long enough that the
/// prefix would push it past the host's limit.
fn to_host(name: &CStr) -> Option<CString> {
    let text = name.to_str().ok()?;
    if HOST_NAMESPACES.iter().any(|ns| text.starts_with(ns)) {
        return CString::new(text).ok();
    }
    CString::new(format!("user.{text}")).ok()
}

/// `int getxattr(const char *, const char *, void *, size_t, u_int32_t, int)`
///
/// `position` is Darwin's offset into a resource fork and is refused when it is
/// not zero: this host has no way to read an attribute from part-way through,
/// and answering from the start would be a short read reported as a whole one.
///
/// # Safety
///
/// `path` and `name` are NUL-terminated; `value` is `size` writable bytes, or
/// `size` is zero.
unsafe extern "C" fn getxattr(
    path: *const c_char,
    name: *const c_char,
    value: *mut c_void,
    size: usize,
    position: u32,
    options: i32,
) -> isize {
    if position != 0 {
        crate::errno::set(libc::EINVAL);
        return -1;
    }
    // SAFETY: the caller's guarantee.
    let Some(host) = (unsafe { host_name(name) }) else {
        crate::errno::set(libc::EINVAL);
        return -1;
    };
    // SAFETY: the caller's path and buffer, with a name this function owns.
    unsafe {
        match options & NOFOLLOW {
            0 => libc::getxattr(path, host.as_ptr(), value, size),
            _ => libc::lgetxattr(path, host.as_ptr(), value, size),
        }
    }
}

/// `int setxattr(const char *, const char *, const void *, size_t, u_int32_t, int)`
///
/// # Safety
///
/// As [`getxattr`], with `value` readable rather than writable.
unsafe extern "C" fn setxattr(
    path: *const c_char,
    name: *const c_char,
    value: *const c_void,
    size: usize,
    position: u32,
    options: i32,
) -> i32 {
    if position != 0 {
        crate::errno::set(libc::EINVAL);
        return -1;
    }
    // SAFETY: the caller's guarantee.
    let Some(host) = (unsafe { host_name(name) }) else {
        crate::errno::set(libc::EINVAL);
        return -1;
    };
    // Darwin's `XATTR_CREATE`/`XATTR_REPLACE` are `0x0002`/`0x0004` and this
    // host's are `1`/`2`, so the flag word is rebuilt rather than forwarded.
    let mut flags = 0;
    if options & 0x0002 != 0 {
        flags |= libc::XATTR_CREATE;
    }
    if options & 0x0004 != 0 {
        flags |= libc::XATTR_REPLACE;
    }
    // SAFETY: the caller's path and buffer, with a name this function owns.
    unsafe {
        match options & NOFOLLOW {
            0 => libc::setxattr(path, host.as_ptr(), value, size, flags),
            _ => libc::lsetxattr(path, host.as_ptr(), value, size, flags),
        }
    }
}

/// `int removexattr(const char *, const char *, int)`
///
/// # Safety
///
/// `path` and `name` are NUL-terminated.
unsafe extern "C" fn removexattr(path: *const c_char, name: *const c_char, options: i32) -> i32 {
    // SAFETY: the caller's guarantee.
    let Some(host) = (unsafe { host_name(name) }) else {
        crate::errno::set(libc::EINVAL);
        return -1;
    };
    // SAFETY: the caller's path, with a name this function owns.
    unsafe {
        match options & NOFOLLOW {
            0 => libc::removexattr(path, host.as_ptr()),
            _ => libc::lremovexattr(path, host.as_ptr()),
        }
    }
}

/// `ssize_t listxattr(const char *, char *, size_t, int)`
///
/// The host's answer is a run of NUL-terminated names; each is copied back with
/// its `user.` prefix removed and anything in one of this host's other
/// namespaces dropped, so a guest sees the names it wrote and nothing it did
/// not.
///
/// # Safety
///
/// `path` is NUL-terminated and `list` is `size` writable bytes, or `size` is
/// zero.
unsafe extern "C" fn listxattr(
    path: *const c_char,
    list: *mut c_char,
    size: usize,
    options: i32,
) -> isize {
    let mut buffer = vec![0u8; 4096];
    loop {
        // SAFETY: the caller's path, and a buffer this function owns.
        let rc = unsafe {
            let (p, n) = (buffer.as_mut_ptr().cast::<c_char>(), buffer.len());
            match options & NOFOLLOW {
                0 => libc::listxattr(path, p, n),
                _ => libc::llistxattr(path, p, n),
            }
        };
        if rc >= 0 {
            buffer.truncate(rc as usize);
            break;
        }
        if crate::errno::get() != libc::ERANGE || buffer.len() >= 1 << 20 {
            return -1;
        }
        buffer.resize(buffer.len() * 2, 0);
    }

    let mut out: Vec<u8> = Vec::new();
    for name in buffer.split(|b| *b == 0).filter(|n| !n.is_empty()) {
        let Some(bare) = name.strip_prefix(b"user.") else {
            // One of this host's own namespaces; not something the guest wrote.
            continue;
        };
        out.extend_from_slice(bare);
        out.push(0);
    }
    // A zero size is the guest asking how much room it needs.
    if size == 0 {
        return out.len() as isize;
    }
    if out.len() > size {
        crate::errno::set(libc::ERANGE);
        return -1;
    }
    // SAFETY: the caller's buffer, which was just checked to be big enough.
    unsafe { core::ptr::copy_nonoverlapping(out.as_ptr(), list.cast::<u8>(), out.len()) };
    out.len() as isize
}

/// `name` in this host's namespace, or `None` for a name it cannot be put in.
///
/// # Safety
///
/// `name` is NUL-terminated or null.
unsafe fn host_name(name: *const c_char) -> Option<CString> {
    if name.is_null() {
        return None;
    }
    // SAFETY: the caller's guarantee.
    to_host(unsafe { CStr::from_ptr(name) })
}

exports::exports! {
    functions {
        "getxattr" => getxattr,
        "setxattr" => setxattr,
        "removexattr" => removexattr,
        "listxattr" => listxattr,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The rule this module exists for: a flat Darwin name is not a name this
    /// host will store on a regular file.
    #[test]
    fn a_guest_name_is_put_in_this_hosts_namespace() {
        let host = to_host(c"org.chromium.crashpad.database.initialized").expect("a valid name");
        assert_eq!(
            host.to_str().unwrap(),
            "user.org.chromium.crashpad.database.initialized"
        );
    }

    /// A name that already reads as one of this host's namespaces is left
    /// alone, so that it still round trips to itself.
    #[test]
    fn a_name_already_in_a_host_namespace_is_not_prefixed_twice() {
        assert_eq!(
            to_host(c"user.thing").unwrap().to_str().unwrap(),
            "user.thing"
        );
        assert_eq!(
            to_host(c"security.thing").unwrap().to_str().unwrap(),
            "security.thing"
        );
    }

    /// Through the real filesystem: set, read back, list, remove - all in the
    /// guest's flat spelling.
    ///
    /// Skipped rather than failed where the filesystem has no extended
    /// attributes, which is a property of the host and not of this code.
    #[test]
    fn an_attribute_round_trips_in_the_guests_spelling() {
        let path = std::env::temp_dir().join(format!("r3-xattr-{}", std::process::id()));
        std::fs::write(&path, b"x").expect("a scratch file");
        let c = CString::new(path.to_str().unwrap()).unwrap();

        // SAFETY: a path and name this test owns, with a buffer of its own.
        let rc = unsafe {
            setxattr(
                c.as_ptr(),
                c"com.example.tag".as_ptr(),
                c"7".as_ptr().cast(),
                1,
                0,
                0,
            )
        };
        if rc != 0 {
            let _ = std::fs::remove_file(&path);
            eprintln!("skipping: this filesystem has no extended attributes");
            return;
        }

        let mut value = [0u8; 8];
        // SAFETY: as above, with eight writable bytes.
        let got = unsafe {
            getxattr(
                c.as_ptr(),
                c"com.example.tag".as_ptr(),
                value.as_mut_ptr().cast(),
                value.len(),
                0,
                0,
            )
        };
        assert_eq!(got, 1);
        assert_eq!(value[0], b'7');

        let mut names = [0u8; 64];
        // SAFETY: as above.
        let listed = unsafe { listxattr(c.as_ptr(), names.as_mut_ptr().cast(), names.len(), 0) };
        assert!(listed > 0);
        assert!(
            names[..listed as usize]
                .split(|b| *b == 0)
                .any(|n| n == b"com.example.tag"),
            "the guest's own spelling is what comes back"
        );

        // SAFETY: as above.
        assert_eq!(
            unsafe { removexattr(c.as_ptr(), c"com.example.tag".as_ptr(), 0) },
            0
        );
        let _ = std::fs::remove_file(&path);
    }

    /// Darwin's resource-fork offset has no host equivalent and is refused
    /// rather than ignored: ignoring it answers from the start of the attribute
    /// and reports the count as if it came from the offset asked for.
    #[test]
    fn a_non_zero_position_is_refused_rather_than_ignored() {
        // SAFETY: nothing is dereferenced on the refusing path.
        let rc = unsafe { getxattr(c"/".as_ptr(), c"x".as_ptr(), core::ptr::null_mut(), 0, 4, 0) };
        assert_eq!(rc, -1);
        assert_eq!(crate::errno::get(), libc::EINVAL);
        crate::errno::set(0);
    }
}
