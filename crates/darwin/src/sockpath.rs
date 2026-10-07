//! `AF_UNIX` socket paths, which name files and so live in the prefix.
//!
//! A socket bound at `/private/var/run/x.sock` by one guest is the file
//! another guest connects to by the same name, so both must reach the same
//! host file - the prefix's, not the host's own `/private/var/run`.
//!
//! `net` converts every address, on every host, and calls [`to_host`] for this
//! part and [`bound`] once the file exists.

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
