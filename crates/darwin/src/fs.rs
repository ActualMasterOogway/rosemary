//! The path-taking calls that were passthroughs, and cannot be any more.
//!
//! Every one of these has an identical signature on both platforms, which is
//! why [`crate::passthrough`] claimed them. What changed is not the call but
//! the filesystem underneath it: a guest names `/usr/lib` or
//! `/Users/user/Library`, and this host has neither, so the name has to be
//! translated into the prefix before the host is asked. See [`prefix::ns`].
//!
//! Three shapes appear here.
//!
//! * One path in: translate, call, and fold on `ENOENT` - the same
//!   [`crate::stat::with_fallback`] the `stat` family uses.
//! * Two paths in: translate both. The fold applies to the source only, since
//!   the destination is a name that is not supposed to exist yet.
//! * A path *out*: `getcwd` answers with a host path, and a guest that is given
//!   one will store it, compare it and be wrong about it later - so it is
//!   spelled the guest's way before it is copied out, and a directory no mount
//!   reaches is reported rather than leaked.

#![cfg(not(target_os = "macos"))]

use std::ffi::{CStr, CString, c_char, c_int};

use crate::stat::{DARWIN_PATH_MAX, guest_path, with_fallback};

/// The host path `path` names, or `None` for a null pointer.
fn host(path: *const c_char) -> Option<CString> {
    if path.is_null() {
        return None;
    }
    // SAFETY: non-null, and a `const char *` is NUL-terminated by contract.
    Some(prefix::ns::to_host(unsafe { CStr::from_ptr(path) }).into_owned())
}

/// Run `ask` on the host spelling of both paths, folding `from` on `ENOENT`.
///
/// Only `from` is folded: `to` is a name the call is about to create, and a
/// case-insensitive reading of a name that does not exist yet would either find
/// nothing or find the wrong neighbour.
fn with_pair(
    from: *const c_char,
    to: *const c_char,
    mut ask: impl FnMut(*const c_char, *const c_char) -> c_int,
) -> c_int {
    let (Some(from_host), Some(to_host)) = (host(from), host(to)) else {
        crate::errno::set(libc::EFAULT);
        return -1;
    };
    let rc = ask(from_host.as_ptr(), to_host.as_ptr());
    if rc == 0 || crate::errno::get() != libc::ENOENT {
        return rc;
    }
    let Some(found) = prefix::ns::fold(&from_host) else {
        return rc;
    };
    ask(found.as_ptr(), to_host.as_ptr())
}

/// `int chdir(const char *)`
///
/// # Safety
///
/// `path` is a NUL-terminated string.
pub unsafe extern "C" fn chdir(path: *const c_char) -> c_int {
    with_fallback(path, |path| {
        // SAFETY: the caller's obligation.
        let rc = unsafe { libc::chdir(path) };
        (rc, rc == 0)
    })
}

/// `char *getcwd(char *, size_t)`
///
/// The working directory is a host directory - `chdir` put it there - and the
/// guest has to be told the name it used. A directory outside every mount has
/// no guest spelling; `ENOENT` is what a Darwin kernel answers about a path
/// that is not in its filesystem, and is better than handing over a host path
/// the guest will keep.
///
/// # Safety
///
/// `buffer` is null or writable for `size` bytes.
pub unsafe extern "C" fn getcwd(buffer: *mut c_char, size: libc::size_t) -> *mut c_char {
    // Into this crate's own buffer first: the host's name and the guest's are
    // not the same length, so the caller's `size` cannot be measured yet.
    let mut here = [0 as c_char; libc::PATH_MAX as usize];
    // SAFETY: `here` is a live local of the host's `PATH_MAX`.
    if unsafe { libc::getcwd(here.as_mut_ptr(), here.len()) }.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: `getcwd` wrote a NUL-terminated path into `here`.
    let Some(spelled) = guest_path(unsafe { CStr::from_ptr(here.as_ptr()) }) else {
        crate::errno::set(libc::ENOENT);
        return std::ptr::null_mut();
    };
    let bytes = spelled.as_bytes_with_nul();
    // A null buffer means "allocate one", and the caller frees it - so it has
    // to come from the allocator the guest's `free` uses.
    let (out, room) = match buffer.is_null() {
        // SAFETY: a plain size; the block is the caller's to free.
        true => (
            unsafe { crate::heap::malloc(DARWIN_PATH_MAX) }.cast::<c_char>(),
            DARWIN_PATH_MAX,
        ),
        false => (buffer, size),
    };
    if out.is_null() {
        crate::errno::set(libc::ENOMEM);
        return std::ptr::null_mut();
    }
    if bytes.len() > room {
        if buffer.is_null() {
            // SAFETY: a block this function just allocated and nothing else
            // holds.
            unsafe { crate::heap::free(out.cast()) };
        }
        crate::errno::set(libc::ERANGE);
        return std::ptr::null_mut();
    }
    // SAFETY: `out` is writable for `room` bytes and `bytes` has just been
    // checked to fit; the two do not overlap.
    unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr().cast::<c_char>(), out, bytes.len()) };
    out
}

/// `int unlink(const char *)`
///
/// # Safety
///
/// `path` is a NUL-terminated string.
pub unsafe extern "C" fn unlink(path: *const c_char) -> c_int {
    with_fallback(path, |path| {
        // SAFETY: the caller's obligation.
        let rc = unsafe { libc::unlink(path) };
        (rc, rc == 0)
    })
}

/// `int rmdir(const char *)`
///
/// # Safety
///
/// `path` is a NUL-terminated string.
pub unsafe extern "C" fn rmdir(path: *const c_char) -> c_int {
    with_fallback(path, |path| {
        // SAFETY: the caller's obligation.
        let rc = unsafe { libc::rmdir(path) };
        (rc, rc == 0)
    })
}

/// `int remove(const char *)`
///
/// # Safety
///
/// `path` is a NUL-terminated string.
pub unsafe extern "C" fn remove(path: *const c_char) -> c_int {
    with_fallback(path, |path| {
        // SAFETY: the caller's obligation.
        let rc = unsafe { libc::remove(path) };
        (rc, rc == 0)
    })
}

/// `int mkdir(const char *, mode_t)`
///
/// # Safety
///
/// `path` is a NUL-terminated string.
pub unsafe extern "C" fn mkdir(path: *const c_char, mode: libc::mode_t) -> c_int {
    let Some(path) = host(path) else {
        crate::errno::set(libc::EFAULT);
        return -1;
    };
    // No fold: the directory is one this call is about to create, and a
    // case-insensitive reading of a name that does not exist yet would answer
    // about a different directory.
    // SAFETY: a NUL-terminated path.
    unsafe { libc::mkdir(path.as_ptr(), mode) }
}

/// `int chmod(const char *, mode_t)`
///
/// # Safety
///
/// `path` is a NUL-terminated string.
pub unsafe extern "C" fn chmod(path: *const c_char, mode: libc::mode_t) -> c_int {
    with_fallback(path, |path| {
        // SAFETY: the caller's obligation.
        let rc = unsafe { libc::chmod(path, mode) };
        (rc, rc == 0)
    })
}

/// `int chown(const char *, uid_t, gid_t)`
///
/// # Safety
///
/// `path` is a NUL-terminated string.
pub unsafe extern "C" fn chown(path: *const c_char, uid: libc::uid_t, gid: libc::gid_t) -> c_int {
    with_fallback(path, |path| {
        // SAFETY: the caller's obligation.
        let rc = unsafe { libc::chown(path, uid, gid) };
        (rc, rc == 0)
    })
}

/// `int lchown(const char *, uid_t, gid_t)`
///
/// # Safety
///
/// `path` is a NUL-terminated string.
pub unsafe extern "C" fn lchown(path: *const c_char, uid: libc::uid_t, gid: libc::gid_t) -> c_int {
    with_fallback(path, |path| {
        // SAFETY: the caller's obligation.
        let rc = unsafe { libc::lchown(path, uid, gid) };
        (rc, rc == 0)
    })
}

/// `int truncate(const char *, off_t)`
///
/// # Safety
///
/// `path` is a NUL-terminated string.
pub unsafe extern "C" fn truncate(path: *const c_char, length: libc::off_t) -> c_int {
    with_fallback(path, |path| {
        // SAFETY: the caller's obligation.
        let rc = unsafe { libc::truncate(path, length) };
        (rc, rc == 0)
    })
}

/// `int link(const char *, const char *)`
///
/// # Safety
///
/// Both arguments are NUL-terminated strings.
pub unsafe extern "C" fn link(from: *const c_char, to: *const c_char) -> c_int {
    // SAFETY: the caller's obligation.
    with_pair(from, to, |from, to| unsafe { libc::link(from, to) })
}

/// `int rename(const char *, const char *)`
///
/// # Safety
///
/// Both arguments are NUL-terminated strings.
pub unsafe extern "C" fn rename(from: *const c_char, to: *const c_char) -> c_int {
    // SAFETY: the caller's obligation.
    with_pair(from, to, |from, to| unsafe { libc::rename(from, to) })
}

/// `int symlink(const char *target, const char *link)`
///
/// The *target* is translated too, and deliberately: it is stored in the link
/// verbatim, and an absolute one is read back by [`crate::stat::readlink`],
/// which spells a host target the guest's way. Storing the host spelling is
/// what makes those two agree, and it is also what lets a host tool follow a
/// link a guest made.
///
/// # Safety
///
/// Both arguments are NUL-terminated strings.
pub unsafe extern "C" fn symlink(target: *const c_char, link: *const c_char) -> c_int {
    let (Some(target), Some(link)) = (host(target), host(link)) else {
        crate::errno::set(libc::EFAULT);
        return -1;
    };
    // SAFETY: two NUL-terminated paths.
    unsafe { libc::symlink(target.as_ptr(), link.as_ptr()) }
}

/// `FILE *freopen(const char *, const char *, FILE *)`
///
/// # Safety
///
/// `path` and `mode` are NUL-terminated strings and `stream` is a `FILE *` the
/// host's stdio owns.
pub unsafe extern "C" fn freopen(
    path: *const c_char,
    mode: *const c_char,
    stream: *mut libc::FILE,
) -> *mut libc::FILE {
    with_fallback(path, |path| {
        // SAFETY: the caller's obligation.
        let file = unsafe { libc::freopen(path, mode, stream) };
        (file, !file.is_null())
    })
}

/// `int mkstemp(char *template)`
///
/// The template is rewritten in place with the name that was created, and the
/// caller sized it. The host writes a *host* path there, so the guest spelling
/// goes back instead - which is the same length by construction, the two names
/// differing only in the six characters `mkstemp` replaced.
///
/// # Safety
///
/// `template` is a NUL-terminated string ending in `XXXXXX`, writable for its
/// own length.
pub unsafe extern "C" fn mkstemp(template: *mut c_char) -> c_int {
    let Some((mut host_template, room)) = (unsafe { host_template(template) }) else {
        return -1;
    };
    // SAFETY: a writable NUL-terminated template.
    let fd = unsafe { libc::mkstemp(host_template.as_mut_ptr().cast::<c_char>()) };
    if fd < 0 {
        return fd;
    }
    // SAFETY: `mkstemp` rewrote the template in place, NUL-terminated.
    if !unsafe { write_back(template, &host_template, room) } {
        // The file exists and the caller will never learn its name, so it is
        // removed rather than left behind in the prefix.
        // SAFETY: a descriptor this call just opened and the path that made it.
        unsafe {
            libc::close(fd);
            libc::unlink(host_template.as_ptr().cast::<c_char>());
        }
        return -1;
    }
    fd
}

/// `char *mkdtemp(char *template)`
///
/// # Safety
///
/// As [`mkstemp`].
pub unsafe extern "C" fn mkdtemp(template: *mut c_char) -> *mut c_char {
    let Some((mut host_template, room)) = (unsafe { host_template(template) }) else {
        return std::ptr::null_mut();
    };
    // SAFETY: a writable NUL-terminated template.
    if unsafe { libc::mkdtemp(host_template.as_mut_ptr().cast::<c_char>()) }.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: `mkdtemp` rewrote the template in place, NUL-terminated.
    if !unsafe { write_back(template, &host_template, room) } {
        // SAFETY: a directory this call just made and nothing has entered.
        unsafe { libc::rmdir(host_template.as_ptr().cast::<c_char>()) };
        return std::ptr::null_mut();
    }
    // Darwin answers with the caller's own buffer.
    template
}

/// The host spelling of a `XXXXXX` template, and how many bytes the caller's
/// buffer holds for the name.
///
/// # Safety
///
/// `template` is null or NUL-terminated.
unsafe fn host_template(template: *mut c_char) -> Option<(Vec<u8>, usize)> {
    if template.is_null() {
        crate::errno::set(libc::EFAULT);
        return None;
    }
    // SAFETY: the caller's obligation.
    let guest = unsafe { CStr::from_ptr(template) };
    let room = guest.to_bytes().len();
    Some((
        prefix::ns::to_host(guest)
            .into_owned()
            .into_bytes_with_nul(),
        room,
    ))
}

/// Write the guest spelling of a rewritten template back into the caller's
/// buffer, answering whether it fitted.
///
/// # Safety
///
/// `template` is writable for `room` bytes plus its NUL, and `made` is a
/// NUL-terminated host path.
unsafe fn write_back(template: *mut c_char, made: &[u8], room: usize) -> bool {
    // SAFETY: the caller's obligation.
    let made = unsafe { CStr::from_ptr(made.as_ptr().cast::<c_char>()) };
    let Some(spelled) = guest_path(made) else {
        crate::errno::set(libc::ENOENT);
        return false;
    };
    let bytes = spelled.to_bytes();
    // The two spellings are the same length by construction - the host call
    // only replaced the six `X`s - so this is a guard, not a path anything is
    // expected to take.
    if bytes.len() != room {
        crate::errno::set(libc::ENAMETOOLONG);
        return false;
    }
    // SAFETY: `template` is writable for `room` bytes and `bytes` is exactly
    // that many; the NUL already there stays where it is.
    unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr().cast::<c_char>(), template, room) };
    true
}

exports::exports! {
    functions {
        "chdir" => chdir,
        "getcwd" => getcwd,
        "unlink" => unlink,
        "rmdir" => rmdir,
        "remove" => remove,
        "mkdir" => mkdir,
        "chmod" => chmod,
        "chown" => chown,
        "lchown" => lchown,
        "truncate" => truncate,
        "link" => link,
        "rename" => rename,
        "symlink" => symlink,
        "freopen" => freopen,
        "mkstemp" => mkstemp,
        "mkdtemp" => mkdtemp,
    }
}
