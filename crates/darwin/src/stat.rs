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

/// Darwin's `PATH_MAX`, which is **not** the host's.
///
/// A guest sizes its `realpath` buffer with its own headers, so 1024 bytes is all
/// the room there is - and Linux's `PATH_MAX` is 4096. Resolving into the caller's
/// buffer directly would let a path longer than a guest expects overrun it, which
/// is why [`realpath`] resolves into its own and copies.
const DARWIN_PATH_MAX: usize = 1024;

/// `char *realpath(const char *, char *resolved)`, with the same fallback.
///
/// Two reasons this is not a passthrough.
///
/// The first is the fallback, and it is what makes a Qt application start: every
/// `QDir::canonicalPath` is a `realpath`, and the host's fails outright on a path
/// whose case does not match the disk. Qt treats that failure as an *empty* path
/// rather than as an error, so a plugin directory spelled `Contents/plugins` in a
/// bundle that has `Contents/Plugins` does not merely fail to load - it puts an
/// empty string in the library search path, and Qt then reports that it looked
/// for its platform plugin in `""`.
///
/// The second is the buffer. Darwin's `PATH_MAX` is a quarter of this host's, so
/// the result is measured against [`DARWIN_PATH_MAX`] and refused with
/// `ENAMETOOLONG` if it does not fit - which is what Darwin itself answers, and
/// the alternative is writing past the end of the caller's array.
///
/// # Safety
///
/// `path` is a NUL-terminated string, and `resolved` is null or writable for
/// `PATH_MAX` bytes as the *guest* counts them.
pub unsafe extern "C" fn realpath(path: *const c_char, resolved: *mut c_char) -> *mut c_char {
    let found = with_fallback(path, |path| {
        // Resolved into this crate's own buffer rather than the caller's: the
        // host may write up to its own `PATH_MAX`, which is larger than the
        // caller's array.
        let mut buffer = [0 as c_char; libc::PATH_MAX as usize];
        // SAFETY: the caller's obligation for `path`; `buffer` is a live local
        // of the host's `PATH_MAX`.
        let got = unsafe { libc::realpath(path, buffer.as_mut_ptr()) };
        match got.is_null() {
            true => (None, false),
            // SAFETY: `realpath` wrote a NUL-terminated path into `buffer`.
            false => (
                Some(unsafe { CStr::from_ptr(buffer.as_ptr()) }.to_owned()),
                true,
            ),
        }
    });
    let Some(found) = found else {
        return std::ptr::null_mut();
    };
    let bytes = found.as_bytes_with_nul();
    if bytes.len() > DARWIN_PATH_MAX {
        crate::errno::set(libc::ENAMETOOLONG);
        return std::ptr::null_mut();
    }
    // A null `resolved` means "allocate one", and the caller frees it - so it has
    // to come from the allocator the guest's `free` uses.
    let out = match resolved.is_null() {
        // SAFETY: a plain size; the block is the caller's to free.
        true => unsafe { crate::heap::malloc(DARWIN_PATH_MAX) }.cast::<c_char>(),
        false => resolved,
    };
    if out.is_null() {
        crate::errno::set(libc::ENOMEM);
        return std::ptr::null_mut();
    }
    // SAFETY: `out` is the caller's `PATH_MAX` array or a fresh block of that
    // size, and `bytes` has just been checked to fit in it.
    unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr().cast::<c_char>(), out, bytes.len()) };
    out
}

/// `ssize_t readlink(const char *, char *, size_t)`, with the same fallback.
///
/// Here for the same reason as [`access`]: a program that built a path in the
/// wrong case asks about it, and the host answers `ENOENT` for a link that is
/// there under another spelling.
///
/// # Safety
///
/// `path` is a NUL-terminated string and `buffer` is writable for `size` bytes.
pub unsafe extern "C" fn readlink(
    path: *const c_char,
    buffer: *mut c_char,
    size: libc::size_t,
) -> libc::ssize_t {
    with_fallback(path, |path| {
        // SAFETY: the caller's obligation.
        let n = unsafe { libc::readlink(path, buffer, size) };
        (n, n >= 0)
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
/// This entry has a fixed third argument so it can also be called by an ABI
/// bridge. Only `O_CREAT` makes that argument meaningful; without it the guest
/// may have left the corresponding register untouched.
///
/// # Safety
///
/// `path` is a NUL-terminated string.
pub unsafe extern "C" fn open(path: *const c_char, flags: c_int, mode: u32) -> c_int {
    let Some(host_flags) = open_flags(flags) else {
        return -1;
    };
    let mode = open_mode(flags, mode);
    with_fallback(path, |path| {
        // SAFETY: the caller's obligation.
        let fd = unsafe { libc::open(path, host_flags, mode) };
        (fd, fd >= 0)
    })
}

/// `int openat(int, const char *, int, ...)`, with Darwin's flag word.
///
/// The case-insensitive fallback remains relative to `dirfd`, including when it
/// is a real directory descriptor rather than `AT_FDCWD`.
///
/// # Safety
///
/// `path` is a NUL-terminated string and `dirfd` is either a directory
/// descriptor or Darwin's `AT_FDCWD`.
pub unsafe extern "C" fn openat(
    dirfd: c_int,
    path: *const c_char,
    flags: c_int,
    mode: u32,
) -> c_int {
    let Some(host_flags) = open_flags(flags) else {
        return -1;
    };
    let dirfd = match dirfd {
        DARWIN_AT_FDCWD => libc::AT_FDCWD,
        fd => fd,
    };
    let mode = open_mode(flags, mode);
    let ask = |path| {
        // SAFETY: the caller's obligation.
        let fd = unsafe { libc::openat(dirfd, path, host_flags, mode) };
        (fd, fd >= 0)
    };
    let (result, ok) = ask(path);
    if ok || path.is_null() || crate::errno::get() != libc::ENOENT {
        return result;
    }
    // SAFETY: non-null, and a `const char *` is NUL-terminated by contract.
    let Some(found) = crate::path::insensitive_at(dirfd, unsafe { CStr::from_ptr(path) }) else {
        return result;
    };
    ask(found.as_ptr()).0
}

/// Darwin spells `AT_FDCWD` as `-2`; Linux spells it as `-100`.
const DARWIN_AT_FDCWD: c_int = -2;

/// Translate a Darwin `O_` word, refusing bits whose semantics cannot be kept.
fn open_flags(flags: c_int) -> Option<c_int> {
    match crate::fcntl::flags_to_host(flags) {
        Ok(host) => Some(host),
        Err(unknown) => {
            say_unknown_open_flags(unknown);
            crate::errno::set(libc::EINVAL);
            None
        }
    }
}

/// Narrow a real mode and discard the stale register when there is no mode.
fn open_mode(flags: c_int, mode: u32) -> c_int {
    match crate::fcntl::creates(flags) {
        true => mode as libc::mode_t as c_int,
        false => 0,
    }
}

/// Name an unsupported flag set once; `EINVAL` alone blames the guest.
fn say_unknown_open_flags(unknown: c_int) {
    use std::sync::Mutex;

    static SAID: Mutex<Vec<c_int>> = Mutex::new(Vec::new());
    let mut said = SAID.lock().unwrap_or_else(|e| e.into_inner());
    if said.contains(&unknown) {
        return;
    }
    said.push(unknown);
    eprintln!("rosetta3: open: no equivalent on this host for Darwin O_ flags {unknown:#x}");
}

/// Every name this module answers, in both spellings a Mach-O may import them
/// by - see the module note on why `$INODE64` is not the same name.
/// Darwin's `struct statfs`, which is 2168 bytes to this host's 120.
///
/// The two do not merely differ in layout, they differ in *kind*: Darwin
/// carries the mount point and the device inside the structure, as two
/// 1024-byte paths, where Linux answers only numbers and expects a caller who
/// wants the path to have asked `/proc/mounts`. So the size difference is the
/// dangerous part - the host's `statfs` writes 120 bytes into a buffer the
/// guest sized at 2168 and leaves the rest as whatever was on its stack, and a
/// caller reading `f_mntonname` gets that.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Statfs {
    pub bsize: u32,
    pub iosize: i32,
    pub blocks: u64,
    pub bfree: u64,
    pub bavail: u64,
    pub files: u64,
    pub ffree: u64,
    pub fsid: [i32; 2],
    pub owner: u32,
    pub fstype: u32,
    pub flags: u32,
    pub fssubtype: u32,
    pub fstypename: [u8; 16],
    pub mntonname: [u8; 1024],
    pub mntfromname: [u8; 1024],
    pub flags_ext: u32,
    pub reserved: [u32; 7],
}

/// Fill a Darwin `struct statfs` from this host's.
///
/// The counts are the whole answer and they carry across exactly: both
/// platforms report blocks of `f_bsize` bytes. What does **not** carry across
/// is everything textual - `f_fstypename`, `f_mntonname` and `f_mntfromname`
/// are left empty rather than filled with a plausible `"apfs"` and `"/"`,
/// because a caller that reads them is one deciding something about the volume
/// and an invented mount point is a wrong answer where an empty one is a
/// visible gap. Every caller seen so far wants `f_bsize * f_bavail`.
///
/// # Safety
///
/// `out` is null or a writable Darwin `struct statfs`.
unsafe fn fill_statfs(host: &libc::statfs, out: *mut Statfs) {
    if out.is_null() {
        return;
    }
    let value = Statfs {
        bsize: host.f_bsize as u32,
        iosize: host.f_bsize as i32,
        blocks: host.f_blocks,
        bfree: host.f_bfree,
        bavail: host.f_bavail,
        files: host.f_files,
        ffree: host.f_ffree,
        // Two 32-bit halves on both sides, and opaque on both: it is only ever
        // compared against another `f_fsid`.
        fsid: unsafe { std::mem::transmute::<libc::fsid_t, [i32; 2]>(host.f_fsid) },
        owner: 0,
        fstype: 0,
        // This host's `struct statfs` carries no mount-flag word at all - the
        // read-only bit is `statvfs`'s `f_flag`, a different call - so nothing
        // is claimed rather than `MNT_RDONLY` being guessed at.
        flags: 0,
        fssubtype: 0,
        fstypename: [0; 16],
        mntonname: [0; 1024],
        mntfromname: [0; 1024],
        flags_ext: 0,
        reserved: [0; 7],
    };
    // SAFETY: the caller's obligation; `out` is non-null and sized by the guest
    // at exactly this structure.
    unsafe { out.write(value) };
}

/// `int statfs(const char *, struct statfs *)`
///
/// # Safety
///
/// `path` is NUL-terminated and `out` is null or a writable Darwin
/// `struct statfs`.
pub unsafe extern "C" fn statfs(path: *const c_char, out: *mut Statfs) -> c_int {
    with_fallback(path, |path| {
        let mut host: libc::statfs = unsafe { std::mem::zeroed() };
        // SAFETY: the caller's obligation, and `host` is this host's own layout.
        let result = unsafe { libc::statfs(path, &mut host) };
        if result == 0 {
            // SAFETY: as above.
            unsafe { fill_statfs(&host, out) };
        }
        (result, result == 0)
    })
}

/// `int fstatfs(int, struct statfs *)`
///
/// # Safety
///
/// As [`statfs`].
pub unsafe extern "C" fn fstatfs(fd: c_int, out: *mut Statfs) -> c_int {
    let mut host: libc::statfs = unsafe { std::mem::zeroed() };
    // SAFETY: `host` is this host's own layout and `fd` is the caller's.
    let result = unsafe { libc::fstatfs(fd, &mut host) };
    if result == 0 {
        // SAFETY: the caller's obligation.
        unsafe { fill_statfs(&host, out) };
    }
    result
}

exports::exports! {
    functions {
        "stat" | "stat$INODE64" => stat,
        "lstat" | "lstat$INODE64" => lstat,
        "fstat" | "fstat$INODE64" => fstat,
        "statfs" | "statfs$INODE64" => statfs,
        "fstatfs" | "fstatfs$INODE64" => fstatfs,
        "access" => access,
        "realpath" => realpath,
        "readlink" => readlink,
        "fopen" | "fopen$UNIX2003" => fopen,
        "open" => open,
        "openat" => openat,
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
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn every_listed_name_resolves_and_a_variant_is_its_base() {
        for name in names() {
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

    #[test]
    fn open_translates_darwin_status_flags() {
        let at = std::env::temp_dir().join(format!("r3-open-flags-{}", std::process::id()));
        std::fs::write(&at, b"x").expect("a file");
        let path = CString::new(at.to_str().expect("utf-8")).expect("no NUL");

        // SAFETY: a NUL-terminated path and Darwin scalar arguments.
        let fd = unsafe {
            open(
                path.as_ptr(),
                crate::fcntl::oflag::O_RDONLY | crate::fcntl::oflag::O_NONBLOCK,
                0,
            )
        };
        assert!(fd >= 0, "open failed: {}", std::io::Error::last_os_error());
        // SAFETY: a descriptor this test just opened; F_GETFL has no third argument.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        // SAFETY: a descriptor this test just opened.
        unsafe { libc::close(fd) };
        let _ = std::fs::remove_file(&at);

        assert_ne!(flags & libc::O_NONBLOCK, 0);
    }

    #[test]
    fn open_refuses_a_flag_the_host_cannot_honor() {
        // Darwin's O_SHLOCK has no Linux equivalent. Silently dropping it
        // would claim a lock that was never acquired.
        const O_SHLOCK: c_int = 0x0010;
        let path = c"/dev/null";
        crate::errno::set(0);
        // SAFETY: a static NUL-terminated path and Darwin scalar arguments.
        let fd = unsafe { open(path.as_ptr(), crate::fcntl::oflag::O_RDONLY | O_SHLOCK, 0) };
        assert_eq!(fd, -1);
        assert_eq!(crate::errno::get(), libc::EINVAL);
        crate::errno::set(0);
    }

    #[test]
    fn openat_translates_at_fdcwd_flags_and_mode() {
        let at = std::env::temp_dir().join(format!("r3-openat-cwd-{}", std::process::id()));
        let _ = std::fs::remove_file(&at);
        let path = CString::new(at.to_str().expect("utf-8")).expect("no NUL");
        let guest_flags = crate::fcntl::oflag::O_WRONLY
            | crate::fcntl::oflag::O_CREAT
            | crate::fcntl::oflag::O_EXCL;

        // SAFETY: a NUL-terminated path, Darwin's AT_FDCWD and scalar arguments.
        let fd = unsafe { openat(DARWIN_AT_FDCWD, path.as_ptr(), guest_flags, 0o604) };
        assert!(
            fd >= 0,
            "openat(AT_FDCWD) failed: {}",
            std::io::Error::last_os_error()
        );
        // SAFETY: a descriptor this test just opened.
        unsafe { libc::close(fd) };

        let permissions = std::fs::metadata(&at)
            .expect("the created file")
            .permissions()
            .mode()
            & 0o777;
        let _ = std::fs::remove_file(&at);
        assert_eq!(permissions & 0o604, 0o604 & !umask());
    }

    #[test]
    fn openat_resolves_a_relative_path_against_its_descriptor() {
        let dir = std::env::temp_dir().join(format!("r3-openat-dir-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a directory");
        let directory = CString::new(dir.to_str().expect("utf-8")).expect("no NUL");
        // SAFETY: a NUL-terminated directory path and Darwin scalar arguments.
        let dirfd = unsafe { open(directory.as_ptr(), crate::fcntl::oflag::O_RDONLY, 0) };
        assert!(dirfd >= 0);

        let name = c"relative";
        let guest_flags = crate::fcntl::oflag::O_WRONLY
            | crate::fcntl::oflag::O_CREAT
            | crate::fcntl::oflag::O_EXCL;
        // SAFETY: a live directory descriptor, a NUL-terminated relative path
        // and Darwin scalar arguments.
        let fd = unsafe { openat(dirfd, name.as_ptr(), guest_flags, 0o600) };
        assert!(
            fd >= 0,
            "openat failed: {}",
            std::io::Error::last_os_error()
        );
        // SAFETY: two descriptors this test just opened.
        unsafe {
            libc::close(fd);
            libc::close(dirfd);
        }

        assert!(dir.join("relative").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn openat_case_folds_relative_to_its_descriptor() {
        let dir = std::env::temp_dir().join(format!("r3-openat-case-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("Contents")).expect("a directory");
        std::fs::write(dir.join("Contents").join("Game.ios"), b"x").expect("a file");
        let directory = CString::new(dir.to_str().expect("utf-8")).expect("no NUL");
        // SAFETY: a NUL-terminated directory path and Darwin scalar arguments.
        let dirfd = unsafe { open(directory.as_ptr(), crate::fcntl::oflag::O_RDONLY, 0) };
        assert!(dirfd >= 0);

        // SAFETY: a live directory descriptor, a NUL-terminated relative path
        // and Darwin scalar arguments.
        let fd = unsafe {
            openat(
                dirfd,
                c"contents/game.ios".as_ptr(),
                crate::fcntl::oflag::O_RDONLY,
                0,
            )
        };
        assert!(
            fd >= 0,
            "openat failed: {}",
            std::io::Error::last_os_error()
        );
        // SAFETY: two descriptors this test just opened.
        unsafe {
            libc::close(fd);
            libc::close(dirfd);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The process umask, restored immediately after reading it.
    fn umask() -> u32 {
        // SAFETY: `umask` cannot fail and the second call restores the first.
        unsafe {
            let previous = libc::umask(0o777);
            libc::umask(previous);
            previous as u32
        }
    }
}
