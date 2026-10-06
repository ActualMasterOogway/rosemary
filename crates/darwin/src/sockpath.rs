//! `AF_UNIX` socket paths, which name files and so live in the prefix.
//!
//! A socket bound at `/private/var/run/x.sock` by one guest is the file
//! another guest connects to by the same name, so both must reach the same
//! host file - the prefix's, not the host's own `/private/var/run`.
//!
//! On Linux `net` converts every address and calls [`to_host`] for this part.
//! A macOS host shares Darwin's `sockaddr` layout, so only `bind` and
//! `connect` are answered here, and only to rewrite the path.

use std::ffi::c_void;

/// `offsetof(struct sockaddr_un, sun_path)` on Darwin: `sun_len`, `sun_family`.
#[cfg(target_os = "macos")]
const PATH_OFFSET: usize = 2;
/// `sizeof(sun_path)` on Darwin.
#[cfg(target_os = "macos")]
const DARWIN_PATH: usize = 104;

#[cfg(target_os = "macos")]
const AF_UNIX: u8 = 1;

/// The host spelling of a guest's `sun_path` bytes, NUL-terminated, or the
/// errno to fail with.
///
/// An abstract or unnamed address (empty, or not absolute) is not a file in
/// any prefix and is returned unchanged; a relative one is already resolved
/// against a host working directory.
pub fn to_host(path: &[u8]) -> Result<Vec<u8>, i32> {
    let end = path.iter().position(|&b| b == 0).unwrap_or(path.len());
    let path = &path[..end];
    if path.first() != Some(&b'/') {
        return Ok([path, &[0]].concat());
    }
    let Ok(guest) = std::ffi::CString::new(path) else {
        return Err(libc::EINVAL);
    };
    let host = prefix::ns::to_host(&guest);
    let bytes = host.to_bytes_with_nul();
    // `sun_path` is a fixed array on the host too, and a prefix deep in the
    // host's tree can push a short guest path past it.
    if bytes.len() > host_capacity() {
        return Err(libc::ENAMETOOLONG);
    }
    Ok(bytes.to_vec())
}

/// Record the socket file a successful `bind` just made at host path `host`
/// as the guest's, the way every other file a guest creates is: a daemon
/// checks its old socket is its own before replacing it.
pub fn bound(host: &[u8]) {
    if !crate::credentials::creates_as_another() {
        return;
    }
    let Ok(path) = std::ffi::CStr::from_bytes_until_nul(host) else {
        return;
    };
    if !path.is_empty() {
        crate::credentials::created(libc::AT_FDCWD, path);
    }
}

fn host_capacity() -> usize {
    // SAFETY: only the array's size is taken.
    let un: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    un.sun_path.len()
}

/// A guest's address with its path made the host's, as Darwin-shaped bytes.
/// `None` when it is not an `AF_UNIX` address and needs nothing.
///
/// # Safety
///
/// `address` is `len` readable bytes.
#[cfg(target_os = "macos")]
unsafe fn translated(address: *const c_void, len: u32) -> Option<Result<Vec<u8>, i32>> {
    if address.is_null() || (len as usize) <= PATH_OFFSET {
        return None;
    }
    // SAFETY: the caller's `len` readable bytes.
    let bytes = unsafe { std::slice::from_raw_parts(address.cast::<u8>(), len as usize) };
    if bytes[1] != AF_UNIX {
        return None;
    }
    let path = &bytes[PATH_OFFSET..bytes.len().min(PATH_OFFSET + DARWIN_PATH)];
    Some(to_host(path).map(|host| {
        let mut out = Vec::with_capacity(PATH_OFFSET + host.len());
        out.extend_from_slice(&[0, AF_UNIX]);
        out.extend_from_slice(&host);
        out[0] = out.len().min(u8::MAX as usize) as u8;
        out
    }))
}

/// `call` with the address's path made the host's; `creates` says that a
/// success made the socket file.
///
/// # Safety
///
/// `address` is `len` readable bytes.
#[cfg(target_os = "macos")]
unsafe fn with_path(
    fd: i32,
    address: *const c_void,
    len: u32,
    call: unsafe extern "C" fn(i32, *const libc::sockaddr, libc::socklen_t) -> i32,
    creates: bool,
) -> i32 {
    // SAFETY: the caller's guarantee, passed along.
    match unsafe { translated(address, len) } {
        // SAFETY: as above.
        None => unsafe { call(fd, address.cast(), len) },
        Some(Ok(host)) => {
            // SAFETY: as above.
            let rc = unsafe { call(fd, host.as_ptr().cast(), host.len() as u32) };
            if rc == 0 && creates {
                bound(&host[PATH_OFFSET..]);
            }
            rc
        }
        Some(Err(errno)) => {
            crate::errno::set(errno);
            -1
        }
    }
}

/// `int bind(int, const struct sockaddr *, socklen_t)`
///
/// # Safety
///
/// `address` is `len` readable bytes.
#[cfg(target_os = "macos")]
unsafe extern "C" fn bind(fd: i32, address: *const c_void, len: u32) -> i32 {
    // SAFETY: the caller's guarantee, passed along.
    unsafe { with_path(fd, address, len, libc::bind, true) }
}

/// `int connect(int, const struct sockaddr *, socklen_t)`
///
/// # Safety
///
/// As [`bind`].
#[cfg(target_os = "macos")]
unsafe extern "C" fn connect(fd: i32, address: *const c_void, len: u32) -> i32 {
    // SAFETY: the caller's guarantee, passed along.
    unsafe { with_path(fd, address, len, libc::connect, false) }
}

#[cfg(target_os = "macos")]
exports::exports! {
    functions {
        "bind" => bind,
        "connect" => connect,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unnamed_or_relative_socket_path_is_left_alone() {
        assert_eq!(to_host(b"").unwrap(), b"\0");
        assert_eq!(to_host(b"rel.sock\0junk").unwrap(), b"rel.sock\0");
    }

    #[test]
    fn an_absolute_socket_path_comes_back_terminated() {
        let host = to_host(b"/tmp/x.sock").unwrap();
        assert_eq!(host.last(), Some(&0));
        assert!(host.ends_with(b"x.sock\0"));
    }
}
