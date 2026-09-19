//! `<pwd.h>`, where the structure is the same idea with the fields in different
//! places.
//!
//! Darwin's `struct passwd` carries two members glibc's does not - `pw_change`
//! and `pw_class`, both BSD's - and they sit in the middle, so everything after
//! them moves:
//!
//! | member | Darwin | this host |
//! |---|---|---|
//! | `pw_gecos` | 40 | 24 |
//! | `pw_dir` | **48** | **32** |
//! | `pw_shell` | 56 | 40 |

#![cfg(not(target_os = "macos"))]

use std::cell::RefCell;
use std::ffi::{CStr, CString, c_char};

/// Darwin's `struct passwd`, from `<pwd.h>`.
///
/// `#[repr(C)]` because the guest reads it by offset - see the module note on
/// `#[repr(C)]` in `darwin::dispatch`, which is the same rule for the same
/// reason.
#[repr(C)]
pub struct Passwd {
    name: *mut c_char,
    passwd: *mut c_char,
    uid: u32,
    gid: u32,
    /// BSD's password-change time. Not a thing this host records.
    change: i64,
    /// BSD's login class. Empty rather than null: a guest is entitled to print it.
    class: *mut c_char,
    gecos: *mut c_char,
    dir: *mut c_char,
    shell: *mut c_char,
    /// BSD's account-expiry time. As `change`.
    expire: i64,
}

/// One answer, with the strings it points at.
///
/// Per thread and overwritten by the next call, which is the lifetime both
/// platforms' `getpwuid` documents. The strings are *copied* rather than
/// borrowed from the host's own static storage, so a guest that calls something
/// else in libc between the lookup and the read still finds them intact.
#[derive(Default)]
struct Answer {
    entry: Option<Box<Entry>>,
}

struct Entry {
    guest: Passwd,
    _strings: Vec<CString>,
}

thread_local! {
    static ANSWER: RefCell<Answer> = RefCell::new(Answer::default());
}

/// A host `struct passwd` as Darwin's, in this thread's storage.
///
/// # Safety
///
/// `host` is a live `struct passwd` this process's libc returned.
unsafe fn publish(host: *const libc::passwd) -> *mut Passwd {
    // SAFETY: the caller's guarantee.
    let host = unsafe { &*host };
    // SAFETY: each is a NUL-terminated string libc owns, or null.
    let text = |p: *const c_char| unsafe {
        match p.is_null() {
            true => CString::default(),
            false => CString::from(CStr::from_ptr(p)),
        }
    };

    let strings = vec![
        text(host.pw_name),
        text(host.pw_passwd),
        // Darwin has a login class and this host has none. Empty is the honest
        // answer and a null is not: `pw_class` is a `char *` a guest may print.
        CString::default(),
        text(host.pw_gecos),
        home_of(host),
        text(host.pw_shell),
    ];
    let at = |i: usize| strings[i].as_ptr().cast_mut();
    let guest = Passwd {
        name: at(0),
        passwd: at(1),
        uid: host.pw_uid,
        gid: host.pw_gid,
        change: 0,
        class: at(2),
        gecos: at(3),
        dir: at(4),
        shell: at(5),
        expire: 0,
    };
    ANSWER.with(|slot| {
        let mut slot = slot.borrow_mut();
        slot.entry = Some(Box::new(Entry {
            guest,
            _strings: strings,
        }));
        slot.entry
            .as_mut()
            .map_or(std::ptr::null_mut(), |e| &raw mut e.guest)
    })
}

/// The home directory to report for `host`.
///
/// `$HOME` wins for the calling user, and the reason is not politeness: this
/// runtime creates the `~/Library` subdirectories a Mac program assumes exist
/// (`foundation::paths`), and it uses `$HOME` to find them. A guest told a
/// *different* home by `getpwuid` would create its own `Library` somewhere the
/// runtime never prepared, and every path either of them built would disagree.
fn home_of(host: &libc::passwd) -> CString {
    // SAFETY: takes no argument and cannot fail.
    if host.pw_uid == unsafe { libc::getuid() }
        && let Some(home) = std::env::var_os("HOME")
    {
        use std::os::unix::ffi::OsStrExt;
        if let Ok(home) = CString::new(home.as_bytes()) {
            return home;
        }
    }
    // SAFETY: libc's own NUL-terminated string, or null.
    unsafe {
        match host.pw_dir.is_null() {
            true => CString::default(),
            false => CString::from(CStr::from_ptr(host.pw_dir)),
        }
    }
}

/// `struct passwd *getpwuid(uid_t)`
///
/// # Safety
///
/// Called only through an import slot this crate defines.
pub unsafe extern "C" fn getpwuid(uid: u32) -> *mut Passwd {
    // A null return means "no such user" *or* an error, and the two are told
    // apart by `errno` - which the caller only trusts if it was cleared first.
    crate::errno::set(0);
    // SAFETY: takes a scalar and returns libc's own storage or null.
    let host = unsafe { libc::getpwuid(uid) };
    if host.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: non-null, and libc's own entry.
    unsafe { publish(host) }
}

/// `struct passwd *getpwnam(const char *)`
///
/// # Safety
///
/// `name` is a NUL-terminated string.
pub unsafe extern "C" fn getpwnam(name: *const c_char) -> *mut Passwd {
    if name.is_null() {
        crate::errno::set(libc::EINVAL);
        return std::ptr::null_mut();
    }
    crate::errno::set(0);
    // SAFETY: the caller's string.
    let host = unsafe { libc::getpwnam(name) };
    if host.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: non-null, and libc's own entry.
    unsafe { publish(host) }
}

/// The directory `~` or `~name` stands for, or `None` for a user this host has
/// never heard of.
///
/// Here rather than in [`crate::glob`], which is what asks, because the `$HOME`
/// rule [`home_of`] applies is the one a guest must see from every direction:
/// a tilde that expanded to a different directory than `getpwuid` reports would
/// have the guest building two sets of paths.
pub fn home(user: Option<&CStr>) -> Option<CString> {
    crate::errno::set(0);
    // SAFETY: a scalar, or the caller's NUL-terminated name; both return libc's
    // own storage or null.
    let host = unsafe {
        match user {
            Some(name) => libc::getpwnam(name.as_ptr()),
            None => libc::getpwuid(libc::getuid()),
        }
    };
    if host.is_null() {
        // A container often has no passwd entry for the uid it runs as, and
        // `$HOME` is still the answer for the calling user.
        return match user {
            Some(_) => None,
            None => std::env::var_os("HOME").and_then(|home| {
                use std::os::unix::ffi::OsStrExt;
                CString::new(home.as_bytes()).ok()
            }),
        };
    }
    // SAFETY: non-null, and libc's own entry.
    let home = home_of(unsafe { &*host });
    (!home.as_bytes().is_empty()).then_some(home)
}

/// The reentrant pair are **not** here, deliberately.
///
/// `getpwuid_r` fills a `struct passwd` the *caller* supplies out of a buffer
/// the caller sized, so honouring it means writing Darwin's layout into the
/// guest's object and packing the strings into the guest's buffer - which is a
/// different piece of work from this one, and one nothing has needed yet.
/// Passing them through would write glibc's layout into a Darwin structure,
/// which is the bug this module was written for.
pub const REFUSED: &[&str] = &["getpwuid_r", "getpwnam_r"];

exports::exports! {
    functions {
        "getpwuid" => getpwuid,
        "getpwnam" => getpwnam,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The offsets are the guest's, and `pw_dir` is the one that matters: at
    /// Darwin's 48 it is past the end of this host's whole structure.
    #[test]
    fn the_layout_is_darwins_and_not_this_hosts() {
        let base = std::ptr::null::<Passwd>();
        let offset = |field: *const c_char| field as usize;
        // SAFETY: offsets computed from a null base, never dereferenced.
        unsafe {
            assert_eq!(offset(&raw const (*base).name as *const c_char), 0);
            assert_eq!(offset(&raw const (*base).class as *const c_char), 32);
            assert_eq!(offset(&raw const (*base).gecos as *const c_char), 40);
            assert_eq!(offset(&raw const (*base).dir as *const c_char), 48);
            assert_eq!(offset(&raw const (*base).shell as *const c_char), 56);
        }
        assert_eq!(core::mem::size_of::<Passwd>(), 72);
        assert!(
            core::mem::size_of::<libc::passwd>() <= 48,
            "this host's whole structure ends at or before Darwin's pw_dir offset, \
             which is why reading it there reads past the end"
        );
    }

    /// And a real lookup answers with strings a guest can read.
    #[test]
    fn the_calling_user_has_a_name_and_a_home() {
        // SAFETY: this process's own uid.
        let entry = unsafe { getpwuid(libc::getuid()) };
        assert!(!entry.is_null(), "the calling user has a password entry");
        // SAFETY: just published, and this thread owns it.
        unsafe {
            assert!(!CStr::from_ptr((*entry).name).to_bytes().is_empty());
            assert!((*entry).dir.as_ref().is_some());
            assert!(!(*entry).class.is_null(), "empty, never null");
        }
    }
}
