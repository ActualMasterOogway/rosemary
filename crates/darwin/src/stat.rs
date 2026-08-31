//! `struct stat`, which is not the host's, and the calls that fill one.
//!
//! Darwin's is 144 bytes with a 64-bit `st_ino`, a 32-bit `st_dev` and four
//! `timespec`s - the fourth being `st_birthtimespec`, which glibc's has no
//! counterpart for. glibc's is a different size with a different field order.
//! Handing a guest's buffer to the host's `stat` therefore fills the right
//! number of bytes with the wrong meanings: a size read out of what the host put
//! there is a plausible number, which is the worst kind of wrong.
//!
//! It was reachable until this module existed. `stat` is on
//! [`crate::divergent`]'s list, but the *`$INODE64` spelling is a different
//! name* and the loader's legacy-suffix fallback resolved it straight to the
//! host's.
//!
//! The path arguments also go through [`crate::path`], because a Mac filesystem
//! is case-insensitive and Mac software leans on that without meaning to.

#![cfg(not(target_os = "macos"))]

use std::ffi::{CStr, c_char, c_int};

use machish::resolve::SyntheticDylib;

/// Darwin's `struct stat` as an x86-64 guest sees it - the `_DARWIN_FEATURE_64_BIT_INODE`
/// layout, which is the only one arm64 and modern x86-64 code has.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct Stat {
    pub dev: i32,
    pub mode: u16,
    pub nlink: u16,
    pub ino: u64,
    pub uid: u32,
    pub gid: u32,
    pub rdev: i32,
    _pad: i32,
    pub atime: Timespec,
    pub mtime: Timespec,
    pub ctime: Timespec,
    pub birthtime: Timespec,
    pub size: i64,
    pub blocks: i64,
    pub blksize: i32,
    pub flags: u32,
    /// `st_gen`, spelled out because `gen` is a reserved word now.
    pub generation: u32,
    pub lspare: i32,
    pub qspare: [i64; 2],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct Timespec {
    pub sec: i64,
    pub nsec: i64,
}

const _: () = {
    assert!(size_of::<Stat>() == 144);
    assert!(std::mem::offset_of!(Stat, ino) == 8);
    assert!(std::mem::offset_of!(Stat, rdev) == 24);
    assert!(std::mem::offset_of!(Stat, atime) == 32);
    assert!(std::mem::offset_of!(Stat, size) == 96);
    assert!(std::mem::offset_of!(Stat, blksize) == 112);
};

/// Convert what the host filled in.
///
/// `st_birthtimespec` is the creation time, which glibc's `stat` does not carry:
/// it is left zero rather than filled from `st_ctime`, because a guest comparing
/// the two to decide whether a file was replaced would get "yes" every time from
/// a copy of the change time.
fn from_host(host: &libc::stat) -> Stat {
    Stat {
        dev: host.st_dev as i32,
        mode: host.st_mode as u16,
        nlink: host.st_nlink as u16,
        ino: host.st_ino as u64,
        uid: host.st_uid,
        gid: host.st_gid,
        rdev: host.st_rdev as i32,
        _pad: 0,
        atime: Timespec {
            sec: host.st_atime as i64,
            nsec: host.st_atime_nsec as i64,
        },
        mtime: Timespec {
            sec: host.st_mtime as i64,
            nsec: host.st_mtime_nsec as i64,
        },
        ctime: Timespec {
            sec: host.st_ctime as i64,
            nsec: host.st_ctime_nsec as i64,
        },
        birthtime: Timespec::default(),
        size: host.st_size as i64,
        blocks: host.st_blocks as i64,
        blksize: host.st_blksize as i32,
        flags: 0,
        generation: 0,
        lspare: 0,
        qspare: [0; 2],
    }
}

/// Run `ask` on `path`, and again on the case-insensitive reading of it if the
/// first attempt says the file is not there.
///
/// Only on `ENOENT`: a path that exists and cannot be read is a permission
/// problem, and looking for a differently-cased neighbour would answer about a
/// different file.
fn with_fallback<T>(path: *const c_char, mut ask: impl FnMut(*const c_char) -> (T, bool)) -> T {
    let (result, ok) = ask(path);
    if ok || path.is_null() || crate::errno::get() != libc::ENOENT {
        return result;
    }
    // SAFETY: non-null, and a `const char *` is NUL-terminated by contract.
    let Some(found) = crate::path::insensitive(unsafe { CStr::from_ptr(path) }) else {
        return result;
    };
    ask(found.as_ptr()).0
}

/// `int stat(const char *, struct stat *)`
///
/// # Safety
///
/// `path` is a NUL-terminated string and `out` is null or a writable Darwin
/// `struct stat`.
pub unsafe extern "C" fn stat(path: *const c_char, out: *mut Stat) -> c_int {
    // SAFETY: the caller's obligation.
    unsafe { at_path(path, out, false) }
}

/// `int lstat(const char *, struct stat *)`
///
/// # Safety
///
/// As [`stat`].
pub unsafe extern "C" fn lstat(path: *const c_char, out: *mut Stat) -> c_int {
    // SAFETY: the caller's obligation.
    unsafe { at_path(path, out, true) }
}

/// # Safety
///
/// As [`stat`].
unsafe fn at_path(path: *const c_char, out: *mut Stat, symlink: bool) -> c_int {
    with_fallback(path, |path| {
        // SAFETY: a zeroed `struct stat` is what the host is asked to fill.
        let mut host: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: the caller's obligation for `path`, and `host` is a live local.
        let rc = unsafe {
            match symlink {
                true => libc::lstat(path, &mut host),
                false => libc::stat(path, &mut host),
            }
        };
        if rc == 0 && !out.is_null() {
            // SAFETY: the caller states `out` is writable for a Darwin `stat`.
            unsafe { *out = from_host(&host) };
        }
        (rc, rc == 0)
    })
}

/// `int fstat(int, struct stat *)`
///
/// # Safety
///
/// `out` is null or a writable Darwin `struct stat`.
pub unsafe extern "C" fn fstat(fd: c_int, out: *mut Stat) -> c_int {
    // SAFETY: a zeroed `struct stat` is what the host is asked to fill.
    let mut host: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: `host` is a live local; `fd` is the caller's.
    let rc = unsafe { libc::fstat(fd, &mut host) };
    if rc == 0 && !out.is_null() {
        // SAFETY: the caller states `out` is writable.
        unsafe { *out = from_host(&host) };
    }
    rc
}

/// `int access(const char *, int)`, with the same fallback.
///
/// Not divergent - the flags agree - but a program that lowercases a path asks
/// this about it before opening it.
///
/// # Safety
///
/// `path` is a NUL-terminated string.
pub unsafe extern "C" fn access(path: *const c_char, mode: c_int) -> c_int {
    with_fallback(path, |path| {
        // SAFETY: the caller's obligation.
        let rc = unsafe { libc::access(path, mode) };
        (rc, rc == 0)
    })
}

/// `FILE *fopen(const char *, const char *)`, with the same fallback.
///
/// # Safety
///
/// Both arguments are NUL-terminated strings.
pub unsafe extern "C" fn fopen(path: *const c_char, mode: *const c_char) -> *mut libc::FILE {
    with_fallback(path, |path| {
        // SAFETY: the caller's obligation.
        let file = unsafe { libc::fopen(path, mode) };
        (file, !file.is_null())
    })
}

/// `int open(const char *, int, ...)`, with the same fallback.
///
/// The third argument is read unconditionally, which is what the guest's own
/// variadic call put in the register whether or not `O_CREAT` was asked for.
///
/// # Safety
///
/// `path` is a NUL-terminated string.
pub unsafe extern "C" fn open(path: *const c_char, flags: c_int, mode: u32) -> c_int {
    with_fallback(path, |path| {
        // SAFETY: the caller's obligation.
        let fd = unsafe { libc::open(path, flags, mode) };
        (fd, fd >= 0)
    })
}

/// Every name this module answers, in both spellings a Mach-O may import them
/// by - see the module note on why `$INODE64` is not the same name.
pub const NAMES: &[&str] = &[
    "stat",
    "lstat",
    "fstat",
    "stat$INODE64",
    "lstat$INODE64",
    "fstat$INODE64",
    "access",
    "fopen",
    "fopen$UNIX2003",
    "open",
];

pub fn symbol(name: &str) -> Option<u64> {
    let f: *const () = match name {
        "stat" | "stat$INODE64" => stat as *const (),
        "lstat" | "lstat$INODE64" => lstat as *const (),
        "fstat" | "fstat$INODE64" => fstat as *const (),
        "access" => access as *const (),
        "fopen" | "fopen$UNIX2003" => fopen as *const (),
        "open" => open as *const (),
        _ => return None,
    };
    Some(f as usize as u64)
}

pub fn provide(lib: &mut SyntheticDylib) {
    for name in NAMES {
        crate::define_fn(
            lib,
            &format!("_{name}"),
            symbol(name).unwrap_or(0) as *const (),
        );
    }
}

/// The name a `$`-suffixed spelling is a variant of.
///
/// Unused outside the tests, and here so the list above cannot drift from the
/// match below.
#[cfg(test)]
fn base(name: &str) -> &str {
    name.split('$').next().unwrap_or(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;

    #[test]
    fn every_listed_name_resolves_and_a_variant_is_its_base() {
        for name in NAMES {
            assert!(symbol(name).is_some(), "{name}");
            assert_eq!(symbol(name), symbol(base(name)), "{name}");
        }
    }

    /// The layout is the guest's, and the size a guest reads is the file's.
    #[test]
    fn a_size_survives_the_conversion() {
        let at = std::env::temp_dir().join(format!("r3-stat-{}", std::process::id()));
        std::fs::write(&at, vec![0u8; 1234]).expect("a file");
        let path = CString::new(at.to_str().expect("utf-8")).expect("no NUL");
        let mut out = Stat::default();
        // SAFETY: a NUL-terminated path and a writable Darwin `stat`.
        let rc = unsafe { stat(path.as_ptr(), &mut out) };
        let _ = std::fs::remove_file(&at);
        assert_eq!(rc, 0);
        assert_eq!(out.size, 1234);
        assert_ne!(out.mode & 0o170000, 0, "a file type");
    }

    /// And the case-insensitive fallback reaches through it.
    #[test]
    fn a_lowercased_path_still_stats() {
        let dir = std::env::temp_dir().join(format!("r3-stat-ci-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("Contents")).expect("a directory");
        std::fs::write(dir.join("Contents").join("Game.ios"), b"xyz").expect("a file");
        let lowered = CString::new(
            dir.join("contents")
                .join("game.ios")
                .to_str()
                .expect("utf-8"),
        )
        .expect("no NUL");
        let mut out = Stat::default();
        // SAFETY: as above.
        let rc = unsafe { stat(lowered.as_ptr(), &mut out) };
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(rc, 0, "the lowercased path was not found");
        assert_eq!(out.size, 3);
    }
}
