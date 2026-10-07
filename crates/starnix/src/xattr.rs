//! Extended attributes on files, by descriptor or by path.
//!
//! Names are spelled as Linux does, `user.` and all; a host with a different
//! convention maps them. A host that cannot keep them answers
//! [`Unsupported`](std::io::ErrorKind::Unsupported).

use std::ffi::{CStr, CString};
use std::io;
use std::os::fd::RawFd;

use crate::{fd, sys};

/// The file an attribute is on.
#[derive(Clone, Copy)]
pub enum Target<'a> {
    Fd(RawFd),
    /// A path, and whether a final symlink is followed.
    Path(&'a CStr, bool),
    /// A path relative to an open directory, `AT_FDCWD` for the working one.
    At(RawFd, &'a CStr, bool),
}

enum Resolved<'a> {
    Fd(RawFd),
    Path(std::borrow::Cow<'a, CStr>, bool),
}

fn resolve(target: Target<'_>) -> io::Result<Resolved<'_>> {
    use std::borrow::Cow;
    match target {
        Target::Fd(fd) => Ok(Resolved::Fd(fd)),
        Target::Path(path, follow) => Ok(Resolved::Path(Cow::Borrowed(path), follow)),
        Target::At(dirfd, path, follow) => {
            if path.to_bytes().first() == Some(&b'/') || dirfd == libc::AT_FDCWD {
                return Ok(Resolved::Path(Cow::Borrowed(path), follow));
            }
            let dir = fd::path(dirfd).ok_or(io::ErrorKind::InvalidInput)?;
            let joined = [dir.as_os_str().as_encoded_bytes(), b"/", path.to_bytes()].concat();
            let joined = CString::new(joined).map_err(|_| io::ErrorKind::InvalidInput)?;
            Ok(Resolved::Path(Cow::Owned(joined), follow))
        }
    }
}

/// Reads `name` into `buf`, answering the length of the value.
pub fn get(target: Target<'_>, name: &CStr, buf: &mut [u8]) -> io::Result<usize> {
    match resolve(target)? {
        Resolved::Fd(fd) => sys::xattr_get_fd(fd, name, buf),
        Resolved::Path(path, follow) => sys::xattr_get_path(&path, name, buf, follow),
    }
}

/// Sets `name` to `value`, creating or replacing it.
pub fn set(target: Target<'_>, name: &CStr, value: &[u8]) -> io::Result<()> {
    match resolve(target)? {
        Resolved::Fd(fd) => sys::xattr_set_fd(fd, name, value),
        Resolved::Path(path, follow) => sys::xattr_set_path(&path, name, value, follow),
    }
}

/// Removes `name`; [`is_missing`] tells whether it was never there.
pub fn remove(target: Target<'_>, name: &CStr) -> io::Result<()> {
    match resolve(target)? {
        Resolved::Fd(fd) => sys::xattr_remove_fd(fd, name),
        Resolved::Path(path, follow) => sys::xattr_remove_path(&path, name, follow),
    }
}

/// Whether `error` says the attribute is not on the file.
pub fn is_missing(error: &io::Error) -> bool {
    sys::xattr_is_missing(error)
}
