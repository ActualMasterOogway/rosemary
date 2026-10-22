//! `AF_UNIX` socket paths, which name files and so live in the prefix.
//!
//! A socket bound at `/private/var/run/x.sock` by one guest is the file
//! another guest connects to by the same name, so both must reach the same
//! host file - the prefix's, not the host's own `/private/var/run`.
//!
//! `net` converts every address, on every host, and calls [`to_host`] for this
//! part and [`bound`] once the file exists.

/// A guest's `sun_path` as the host should be given it.
///
/// `dir` is set when the path is spelled through a descriptor for its
/// directory, and must stay open until the call that uses `bytes` returns.
/// `file` is then the path that names the file itself, which is what anything
/// that records facts about the file by path - see [`bound`] - has to be given.
pub struct HostPath {
    pub bytes: Vec<u8>,
    pub file: Vec<u8>,
    pub dir: Option<std::os::fd::OwnedFd>,
}

/// The host spelling of a guest's `sun_path` bytes, NUL-terminated, or the
/// errno to fail with.
///
/// An abstract or unnamed address (empty, or not absolute) is not a file in
/// any prefix and is returned unchanged; a relative one is already resolved
/// against a host working directory.
pub fn to_host(path: &[u8]) -> Result<HostPath, i32> {
    let end = path.iter().position(|&b| b == 0).unwrap_or(path.len());
    let path = &path[..end];
    if path.first() != Some(&b'/') {
        let bytes = [path, &[0]].concat();
        return Ok(HostPath {
            file: bytes.clone(),
            bytes,
            dir: None,
        });
    }
    let Ok(guest) = std::ffi::CString::new(path) else {
        return Err(libc::EINVAL);
    };
    let host = prefix::ns::to_host(&guest);
    let bytes = host.to_bytes_with_nul();
    if bytes.len() <= host_capacity() {
        return Ok(HostPath {
            bytes: bytes.to_vec(),
            file: bytes.to_vec(),
            dir: None,
        });
    }
    // `sun_path` is a fixed array on the host too, and a prefix deep in the
    // host's tree (a Flatpak's `~/.var/app/...`) can push a short guest path
    // past it. A guest that fit Darwin's limit should not fail for that.
    through_directory(host.to_bytes())
}

/// `path` spelled as `/proc/self/fd/N/name`, `N` an `O_PATH` descriptor for
/// its directory: the kernel resolves the magic link, so `bind` and `connect`
/// reach the same file by a name short enough for `sun_path`.
#[cfg(target_os = "linux")]
fn through_directory(path: &[u8]) -> Result<HostPath, i32> {
    use std::os::fd::{FromRawFd, OwnedFd};
    let split = path.iter().rposition(|&b| b == b'/').unwrap_or(0);
    let (dir, name) = (&path[..split.max(1)], &path[split + 1..]);
    let Ok(dir) = std::ffi::CString::new(dir) else {
        return Err(libc::EINVAL);
    };
    // SAFETY: a NUL-terminated path; the descriptor is owned below.
    let fd = unsafe {
        libc::open(
            dir.as_ptr(),
            libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(crate::errno::get());
    }
    // SAFETY: a descriptor this call just opened and nothing else holds.
    let dir = unsafe { OwnedFd::from_raw_fd(fd) };
    let mut bytes = format!("/proc/self/fd/{fd}/").into_bytes();
    bytes.extend_from_slice(name);
    bytes.push(0);
    if bytes.len() > host_capacity() {
        return Err(libc::ENAMETOOLONG);
    }
    Ok(HostPath {
        bytes,
        file: [path, &[0]].concat(),
        dir: Some(dir),
    })
}

#[cfg(not(target_os = "linux"))]
fn through_directory(_path: &[u8]) -> Result<HostPath, i32> {
    Err(libc::ENAMETOOLONG)
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
        assert_eq!(to_host(b"").unwrap().bytes, b"\0");
        assert_eq!(to_host(b"rel.sock\0junk").unwrap().bytes, b"rel.sock\0");
    }

    #[test]
    fn an_absolute_socket_path_comes_back_terminated() {
        let host = to_host(b"/tmp/x.sock").unwrap().bytes;
        assert_eq!(host.last(), Some(&0));
        assert!(host.ends_with(b"x.sock\0"));
    }

    #[cfg(target_os = "linux")]
    fn unix_address(path: &[u8]) -> libc::sockaddr_un {
        // SAFETY: all-zero is a valid `sockaddr_un`.
        let mut un: libc::sockaddr_un = unsafe { std::mem::zeroed() };
        un.sun_family = libc::AF_UNIX as _;
        for (slot, byte) in un.sun_path.iter_mut().zip(path) {
            *slot = *byte as _;
        }
        un
    }

    /// A prefix deep enough to push the path past `sun_path` still binds and
    /// connects, both ends reaching the one file.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_socket_path_longer_than_sun_path_binds_and_connects() {
        use std::os::unix::ffi::OsStrExt;
        let root = std::env::temp_dir().join(format!("sockpath-{}", std::process::id()));
        let dir = root.join("d".repeat(60)).join("e".repeat(60));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("x.sock");
        let file = file.as_os_str().as_bytes();
        assert!(file.len() > host_capacity());

        let size = std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t;
        let bound = through_directory(file).unwrap();
        assert_eq!(bound.file, [file, &[0]].concat());
        let address = unix_address(&bound.bytes);
        // SAFETY: a fresh socket and an address this test owns.
        let server = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
        assert_eq!(
            unsafe { libc::bind(server, (&raw const address).cast(), size) },
            0
        );
        assert_eq!(unsafe { libc::listen(server, 1) }, 0);
        drop(bound);
        assert!(dir.join("x.sock").exists());

        let reached = through_directory(file).unwrap();
        let address = unix_address(&reached.bytes);
        // SAFETY: as above.
        let client = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
        assert_eq!(
            unsafe { libc::connect(client, (&raw const address).cast(), size) },
            0
        );

        // SAFETY: descriptors this test owns and no longer uses.
        unsafe {
            libc::close(client);
            libc::close(server);
        }
        std::fs::remove_dir_all(root).unwrap();
    }
}
