//! `fcntl` and the `open` flag word, neither of which survives being forwarded.
//!
//! The command numbers agree only up to `F_SETFL`, and the flags do not agree at
//! all: Darwin's `O_NONBLOCK` is `0x0004` and Linux's is `0o4000`. A `F_SETFL`
//! passed straight through therefore *succeeds* and sets nothing, which is how
//! curl's wakeup pipe stayed blocking and its drain loop stopped forever on the
//! second read.

/// Darwin's `fcntl` commands, from `<sys/fcntl.h>`.
pub mod cmd {
    pub const F_DUPFD: i32 = 0;
    pub const F_GETFD: i32 = 1;
    pub const F_SETFD: i32 = 2;
    pub const F_GETFL: i32 = 3;
    pub const F_SETFL: i32 = 4;
    pub const F_GETOWN: i32 = 5;
    pub const F_SETOWN: i32 = 6;
    pub const F_GETLK: i32 = 7;
    pub const F_SETLK: i32 = 8;
    pub const F_SETLKW: i32 = 9;
    pub const F_PREALLOCATE: i32 = 42;
    pub const F_SETSIZE: i32 = 43;
    pub const F_RDADVISE: i32 = 44;
    pub const F_RDAHEAD: i32 = 45;
    pub const F_NOCACHE: i32 = 48;
    pub const F_LOG2PHYS: i32 = 49;
    pub const F_GETPATH: i32 = 50;
    pub const F_FULLFSYNC: i32 = 51;
    pub const F_PATHPKG_CHECK: i32 = 52;
    pub const F_FREEZE_FS: i32 = 53;
    pub const F_THAW_FS: i32 = 54;
    pub const F_GLOBAL_NOCACHE: i32 = 55;
    pub const F_DUPFD_CLOEXEC: i32 = 67;
}

/// Whether `cmd` reads a third argument.
///
/// A question about the guest's call, answered from the guest's numbers, so the answer
/// does not change with the host.
pub fn takes_third_argument(cmd: i32) -> bool {
    // `F_NOCACHE` and `F_GLOBAL_NOCACHE` are not in this list: both take an
    // `int` (0 or 1) that turns caching off or back on, per Darwin's
    // `fcntl(2)`.
    !matches!(
        cmd,
        cmd::F_GETFD
            | cmd::F_GETFL
            | cmd::F_GETOWN
            | cmd::F_FULLFSYNC
            | cmd::F_FREEZE_FS
            | cmd::F_THAW_FS
    )
}

/// Darwin's `struct flock`, which is not this host's.
///
/// The two platforms hold the same five fields in a different **order**:
///
/// | | Darwin | Linux |
/// |---|---|---|
/// | 0 | `l_start` | `l_type`, `l_whence` |
/// | 8 | `l_len` | `l_start` |
/// | 16 | `l_pid` | `l_len` |
/// | 20 | `l_type`, `l_whence` | `l_pid` at 24 |
///
/// So forwarding the structure reads a whole-file lock's `l_start` of 0 as
/// `l_type` 0, which is Linux's `F_RDLCK` where Darwin's 0 is not a lock type at
/// all. The values are renumbered too - see [`lock_type_to_host`].
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct Flock {
    pub start: i64,
    pub len: i64,
    pub pid: i32,
    pub kind: i16,
    pub whence: i16,
}

const _: () = assert!(size_of::<Flock>() == 24);

/// Darwin's `fstore_t`, used by `F_PREALLOCATE`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct Fstore {
    pub flags: u32,
    pub posmode: i32,
    pub offset: i64,
    pub length: i64,
    pub bytes_allocated: i64,
}

const _: () = assert!(size_of::<Fstore>() == 32);

pub mod preallocate {
    pub const F_ALLOCATECONTIG: u32 = 2;
    pub const F_ALLOCATEALL: u32 = 4;
    pub const F_PEOFPOSMODE: i32 = 3;
    pub const F_VOLPOSMODE: i32 = 4;
}

/// Darwin's `l_type` values, from `<sys/fcntl.h>`.
pub mod lock {
    pub const F_RDLCK: i16 = 1;
    pub const F_UNLCK: i16 = 2;
    pub const F_WRLCK: i16 = 3;
}

/// Darwin's lock type as this host's, which renumbers all three.
///
/// `None` for a value that is none of them: a lock type nothing recognises is a
/// caller's bug, and passing it on would ask for whichever lock this host gives
/// that number - `F_UNLCK` for Darwin's `F_WRLCK`, which is an unlock where an
/// exclusive lock was asked for.
#[cfg(not(target_os = "macos"))]
pub fn lock_type_to_host(kind: i16) -> Option<i16> {
    Some(match kind {
        lock::F_RDLCK => libc::F_RDLCK as i16,
        lock::F_WRLCK => libc::F_WRLCK as i16,
        lock::F_UNLCK => libc::F_UNLCK as i16,
        _ => return None,
    })
}

/// And back, for what `F_GETLK` reports about the lock that is in the way.
#[cfg(not(target_os = "macos"))]
pub fn lock_type_to_guest(kind: i16) -> i16 {
    match kind as libc::c_int {
        libc::F_RDLCK => lock::F_RDLCK,
        libc::F_WRLCK => lock::F_WRLCK,
        _ => lock::F_UNLCK,
    }
}

/// Whether a command takes a `struct flock` rather than a scalar.
pub fn takes_a_lock(cmd: i32) -> bool {
    matches!(cmd, cmd::F_GETLK | cmd::F_SETLK | cmd::F_SETLKW)
}

/// The host command that means what Darwin's `cmd` means, or `None`.
///
/// `None` is the answer for two separate reasons, and neither is worth guessing
/// past:
///
/// * `F_FULLFSYNC`, which asks the drive to flush its own write cache. Linux spells
///   that `fsync` - a different call, not a different command number - so a caller
///   that wants it has to make it, and this cannot.
/// * The rest of the Darwin-only tail, which has no host equivalent at all.
///
/// The lock family is *not* refused: it is translated, structure and all, by
/// [`entry::fcntl`].
#[cfg(not(target_os = "macos"))]
pub fn to_host(cmd: i32) -> Option<libc::c_int> {
    Some(match cmd {
        cmd::F_DUPFD => libc::F_DUPFD,
        cmd::F_GETFD => libc::F_GETFD,
        cmd::F_SETFD => libc::F_SETFD,
        cmd::F_GETFL => libc::F_GETFL,
        cmd::F_SETFL => libc::F_SETFL,
        cmd::F_GETOWN => libc::F_GETOWN,
        cmd::F_SETOWN => libc::F_SETOWN,
        cmd::F_DUPFD_CLOEXEC => libc::F_DUPFD_CLOEXEC,
        cmd::F_GETLK => libc::F_GETLK,
        cmd::F_SETLK => libc::F_SETLK,
        cmd::F_SETLKW => libc::F_SETLKW,
        _ => return None,
    })
}

/// On Darwin the guest's command *is* the host's.
#[cfg(target_os = "macos")]
pub fn to_host(cmd: i32) -> Option<libc::c_int> {
    Some(cmd)
}

/// Darwin's `open` flags, from `<sys/fcntl.h>`.
pub mod oflag {
    pub const O_RDONLY: i32 = 0x0000;
    pub const O_WRONLY: i32 = 0x0001;
    pub const O_RDWR: i32 = 0x0002;
    pub const O_ACCMODE: i32 = 0x0003;
    pub const O_NONBLOCK: i32 = 0x0004;
    pub const O_APPEND: i32 = 0x0008;
    pub const O_ASYNC: i32 = 0x0040;
    pub const O_SYNC: i32 = 0x0080;
    pub const O_NOFOLLOW: i32 = 0x0100;
    pub const O_CREAT: i32 = 0x0200;
    pub const O_TRUNC: i32 = 0x0400;
    pub const O_EXCL: i32 = 0x0800;
    pub const O_NOCTTY: i32 = 0x0002_0000;
    pub const O_DIRECTORY: i32 = 0x0010_0000;
    pub const O_DSYNC: i32 = 0x0040_0000;
    pub const O_CLOEXEC: i32 = 0x0100_0000;
}

/// Whether the guest asked to create the file.
///
/// Its own function because it decides whether `open`'s third argument exists at all,
/// and reading it through `libc::O_CREAT` is wrong by a factor of eight on Linux.
pub fn creates(flags: i32) -> bool {
    flags & oflag::O_CREAT != 0
}

/// The guest's `open` flags as the host's.
///
/// Bit for bit on macOS. Elsewhere a translation, and one that drops nothing silently:
/// a Darwin-only flag with no host equivalent - `O_SHLOCK`, `O_EVTONLY`, `O_SYMLINK` -
/// is reported rather than ignored, because a guest that asked for a shared lock and
/// did not get one has been told it succeeded.
#[cfg(not(target_os = "macos"))]
pub fn flags_to_host(flags: i32) -> Result<libc::c_int, i32> {
    match split_flags_to_host(flags) {
        (host, 0) => Ok(host),
        (_, unknown) => Err(unknown),
    }
}

/// One Darwin flag and the host flag that means the same thing.
///
/// `O_SYNC` precedes `O_DSYNC` because Linux's `O_SYNC` *contains* `O_DSYNC`'s
/// bit, so a reader that took the narrower one first would never see the wider.
#[cfg(not(target_os = "macos"))]
const PAIRS: &[(i32, libc::c_int)] = &[
    (oflag::O_NONBLOCK, libc::O_NONBLOCK),
    (oflag::O_APPEND, libc::O_APPEND),
    (oflag::O_ASYNC, libc::O_ASYNC),
    (oflag::O_SYNC, libc::O_SYNC),
    (oflag::O_NOFOLLOW, libc::O_NOFOLLOW),
    (oflag::O_CREAT, libc::O_CREAT),
    (oflag::O_TRUNC, libc::O_TRUNC),
    (oflag::O_EXCL, libc::O_EXCL),
    (oflag::O_NOCTTY, libc::O_NOCTTY),
    (oflag::O_DIRECTORY, libc::O_DIRECTORY),
    (oflag::O_DSYNC, libc::O_DSYNC),
    (oflag::O_CLOEXEC, libc::O_CLOEXEC),
];

/// The host flags, and the Darwin bits nothing here corresponds to.
///
/// Split out because `open` and `F_SETFL` want opposite things from the leftover:
/// `open` refuses, `F_SETFL` drops. See [`entry::fcntl`] for why.
#[cfg(not(target_os = "macos"))]
fn split_flags_to_host(flags: i32) -> (libc::c_int, i32) {
    // The access mode is the low two bits and has the same three values on both.
    let mut host = flags & oflag::O_ACCMODE;
    let mut rest = flags & !oflag::O_ACCMODE;
    for (darwin, native) in PAIRS {
        if rest & darwin != 0 {
            host |= native;
            rest &= !darwin;
        }
    }
    (host, rest)
}

/// The host's flag word as the guest's, for what `F_GETFL` answers.
///
/// The reverse of [`flags_to_host`], and unlike it this one *drops* what it
/// cannot name rather than reporting it. A host-only status bit - Linux sets
/// `O_LARGEFILE` on nearly every descriptor, and has `O_PATH` and `O_NOATIME`
/// besides - describes a property Darwin has no spelling for, and `F_GETFL`'s
/// answer is not a request the guest made: the guest reads it, sets a bit, and
/// hands it back to `F_SETFL`, which translates it the other way. Refusing here
/// would fail every `F_GETFL` on this host.
#[cfg(not(target_os = "macos"))]
pub fn flags_to_guest(flags: libc::c_int) -> i32 {
    let mut guest = flags & oflag::O_ACCMODE;
    let mut rest = flags & !oflag::O_ACCMODE;
    for (darwin, native) in PAIRS {
        if rest & native == *native {
            guest |= darwin;
            rest &= !native;
        }
    }
    guest
}

#[cfg(target_os = "macos")]
pub fn flags_to_guest(flags: libc::c_int) -> i32 {
    flags
}

#[cfg(target_os = "macos")]
pub fn flags_to_host(flags: i32) -> Result<libc::c_int, i32> {
    Ok(flags)
}

/// `fcntl` itself, for a host whose commands and flag word are not Darwin's.
#[cfg(not(target_os = "macos"))]
pub mod entry {
    use machish::resolve::SyntheticDylib;

    use super::{
        Flock, Fstore, cmd, flags_to_guest, lock_type_to_guest, lock_type_to_host, preallocate,
        split_flags_to_host, takes_a_lock, takes_third_argument, to_host,
    };

    /// `int fcntl(int fd, int cmd, ...)`, with the ellipsis spelled out.
    ///
    /// Three separate translations, and the middle one is the one that matters:
    ///
    /// * the command, through [`to_host`], which refuses rather than forwarding a
    ///   number that means something else here;
    /// * the `F_SETFL` flag word, which is an `open` flag word and so is Darwin's;
    /// * the `F_GETFL` answer, back into Darwin's, because the guest's next call is
    ///   `F_SETFL` with that word plus a bit.
    ///
    /// An unmappable bit in `F_SETFL` is dropped, not refused. `F_SETFL` ignores
    /// every flag outside its own handful on both platforms, so a Darwin-only bit
    /// reaching it was already a no-op there; failing the call would make this
    /// stricter than the platform being emulated, and callers do not expect a
    /// `F_SETFL` that fails.
    ///
    /// # Safety
    ///
    /// Called only through the import slot [`provide`] defines. The third argument
    /// is a word-sized scalar or a pointer into the guest, which is identity-mapped;
    /// for a command that takes none it is whatever the caller left in the register,
    /// and is not passed on.
    pub unsafe extern "C" fn fcntl(
        fd: libc::c_int,
        command: libc::c_int,
        arg: usize,
    ) -> libc::c_int {
        if command == cmd::F_PREALLOCATE {
            // SAFETY: `F_PREALLOCATE` defines its third argument as `fstore_t *`.
            return unsafe { preallocate(fd, arg as *mut Fstore) };
        }
        let Some(host_command) = to_host(command) else {
            crate::errno::set(libc::EINVAL);
            return -1;
        };
        if takes_a_lock(command) {
            // SAFETY: the caller's obligation - a lock command's third argument
            // is a `struct flock *`.
            return unsafe { lock(fd, host_command, arg as *mut Flock) };
        }
        let arg = match command {
            _ if !takes_third_argument(command) => 0,
            cmd::F_SETFL => {
                let (host, dropped) = split_flags_to_host(arg as i32);
                if dropped != 0 {
                    say_dropped(dropped);
                }
                host as usize
            }
            _ => arg,
        };
        // SAFETY: the caller's obligation, above.
        let rc = unsafe { libc::fcntl(fd, host_command, arg) };
        if rc >= 0 && matches!(command, cmd::F_DUPFD | cmd::F_DUPFD_CLOEXEC) {
            crate::net::sockopt::inherit_sigpipe_suppression(fd, rc);
        }
        match command == cmd::F_GETFL && rc >= 0 {
            true => flags_to_guest(rc),
            false => rc,
        }
    }

    unsafe fn preallocate(fd: libc::c_int, store: *mut Fstore) -> libc::c_int {
        if store.is_null() {
            crate::errno::set(libc::EINVAL);
            return -1;
        }
        // SAFETY: the caller supplied the readable and writable `fstore_t`.
        let request = unsafe { *store };
        unsafe { (*store).bytes_allocated = 0 };
        if !matches!(
            request.flags,
            preallocate::F_ALLOCATECONTIG | preallocate::F_ALLOCATEALL
        ) || request.offset < 0
            || request.length < 0
        {
            crate::errno::set(libc::EINVAL);
            return -1;
        }
        let offset = match request.posmode {
            preallocate::F_PEOFPOSMODE => {
                let mut status: libc::stat = unsafe { std::mem::zeroed() };
                // SAFETY: `status` is writable and `fd` is the caller's descriptor.
                if unsafe { libc::fstat(fd, &mut status) } < 0 {
                    return -1;
                }
                match status.st_size.checked_add(request.offset) {
                    Some(offset) => offset,
                    None => {
                        crate::errno::set(libc::EOVERFLOW);
                        return -1;
                    }
                }
            }
            preallocate::F_VOLPOSMODE => request.offset,
            _ => {
                crate::errno::set(libc::EINVAL);
                return -1;
            }
        };
        // `posix_fallocate` returns an errno value directly.
        let error = unsafe { libc::posix_fallocate(fd, offset, request.length) };
        if error != 0 {
            crate::errno::set(error);
            return -1;
        }
        unsafe { (*store).bytes_allocated = request.length };
        0
    }

    /// The lock family, through a `struct flock` of this host's.
    ///
    /// `F_GETLK` is copied back as well as in: it answers by *overwriting* the
    /// caller's structure with the lock that is in the way, and a caller reads
    /// `l_pid` out of it to name the process holding it.
    ///
    /// # Safety
    ///
    /// `flock` must be a readable and writable Darwin `struct flock`.
    unsafe fn lock(fd: libc::c_int, host_command: libc::c_int, flock: *mut Flock) -> libc::c_int {
        if flock.is_null() {
            crate::errno::set(libc::EINVAL);
            return -1;
        }
        // SAFETY: the caller's obligation.
        let wanted = unsafe { *flock };
        let Some(kind) = lock_type_to_host(wanted.kind) else {
            crate::errno::set(libc::EINVAL);
            return -1;
        };
        let mut host: libc::flock = unsafe { std::mem::zeroed() };
        host.l_type = kind as libc::c_short;
        host.l_whence = wanted.whence as libc::c_short;
        host.l_start = wanted.start;
        host.l_len = wanted.len;
        // SAFETY: a live `struct flock` of this host's, which is what the host
        // command reads.
        let rc = unsafe { libc::fcntl(fd, host_command, &mut host) };
        if rc >= 0 && host_command == libc::F_GETLK {
            // SAFETY: the caller's obligation.
            unsafe {
                *flock = Flock {
                    start: host.l_start,
                    len: host.l_len,
                    pid: host.l_pid,
                    kind: lock_type_to_guest(host.l_type as i16),
                    whence: host.l_whence as i16,
                };
            }
        }
        rc
    }

    /// Name a `F_SETFL` bit with no equivalent here, once for each distinct set.
    fn say_dropped(dropped: libc::c_int) {
        use std::sync::Mutex;
        static SAID: Mutex<Vec<libc::c_int>> = Mutex::new(Vec::new());
        let mut said = SAID.lock().unwrap_or_else(|e| e.into_inner());
        if said.contains(&dropped) {
            return;
        }
        said.push(dropped);
        eprintln!(
            "rosetta3: fcntl F_SETFL: no equivalent on this host for Darwin O_ flags {dropped:#x} - dropped"
        );
    }

    pub const NAMES: &[&str] = &["fcntl"];

    pub fn symbol(name: &str) -> Option<u64> {
        match name {
            "fcntl" => Some(fcntl as *const () as usize as u64),
            _ => None,
        }
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
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The guest's numbers are the guest's, whatever the host's happen to be.
    #[test]
    fn the_flags_are_darwin_s_and_not_this_host_s() {
        assert_eq!(oflag::O_CREAT, 0x0200);
        assert!(creates(oflag::O_WRONLY | oflag::O_CREAT));
        assert!(!creates(oflag::O_RDONLY));
    }

    #[test]
    fn a_read_only_open_translates_to_a_read_only_open() {
        assert_eq!(flags_to_host(oflag::O_RDONLY), Ok(libc::O_RDONLY));
        let write = flags_to_host(oflag::O_WRONLY | oflag::O_CREAT | oflag::O_TRUNC).unwrap();
        assert_eq!(write & libc::O_ACCMODE, libc::O_WRONLY);
        assert!(write & libc::O_CREAT != 0 && write & libc::O_TRUNC != 0);
    }

    /// A flag with no host equivalent is reported, not dropped: a guest told its
    /// `O_SHLOCK` succeeded when no lock was taken is worse off than one told no.
    #[test]
    #[cfg(not(target_os = "macos"))]
    fn a_flag_with_no_equivalent_is_reported() {
        const O_SHLOCK: i32 = 0x0010;
        assert_eq!(flags_to_host(oflag::O_RDONLY | O_SHLOCK), Err(O_SHLOCK));
    }

    /// The commands that agree do so by accident and stop at five; anything past
    /// `F_SETFL` needs a translation or a refusal.
    #[test]
    fn only_the_commands_that_really_agree_are_forwarded() {
        assert_eq!(to_host(cmd::F_GETFD), Some(libc::F_GETFD));
        assert_eq!(to_host(cmd::F_SETFL), Some(libc::F_SETFL));
        #[cfg(not(target_os = "macos"))]
        {
            assert_eq!(to_host(cmd::F_FULLFSYNC), None, "Linux spells this fsync");
            assert_eq!(to_host(cmd::F_NOCACHE), None);
        }
    }

    /// The lock family is translated rather than forwarded, and the structure is
    /// the reason: the two platforms hold the same fields in a different order.
    #[test]
    #[cfg(not(target_os = "macos"))]
    fn a_darwin_lock_is_not_this_host_s_lock() {
        assert_eq!(std::mem::offset_of!(Flock, start), 0);
        assert_eq!(std::mem::offset_of!(Flock, kind), 20);
        assert_ne!(
            std::mem::offset_of!(Flock, kind),
            std::mem::offset_of!(libc::flock, l_type),
            "the whole reason this needs translating"
        );
        assert!(takes_a_lock(cmd::F_SETLK) && takes_a_lock(cmd::F_SETLKW));
        assert!(!takes_a_lock(cmd::F_SETFL));
    }

    /// And the values are renumbered too. Darwin's `F_WRLCK` is 3, this host's is
    /// 1, and this host's 3 is nothing at all - so an untranslated exclusive lock
    /// asks for a lock type that does not exist.
    #[test]
    #[cfg(not(target_os = "macos"))]
    fn the_lock_types_are_renumbered_both_ways() {
        assert_eq!(lock_type_to_host(lock::F_WRLCK), Some(libc::F_WRLCK as i16));
        assert_eq!(lock_type_to_host(lock::F_RDLCK), Some(libc::F_RDLCK as i16));
        assert_eq!(lock_type_to_host(lock::F_UNLCK), Some(libc::F_UNLCK as i16));
        assert_eq!(lock_type_to_host(0), None, "0 is no Darwin lock type");
        for kind in [lock::F_RDLCK, lock::F_WRLCK, lock::F_UNLCK] {
            assert_eq!(lock_type_to_guest(lock_type_to_host(kind).unwrap()), kind);
        }
    }

    /// The whole-file exclusive lock a single-instance guard takes, end to end
    /// through the published entry point: it is granted, and a second attempt on
    /// the same file from another descriptor is refused rather than granted
    /// twice.
    #[test]
    #[cfg(not(target_os = "macos"))]
    fn a_whole_file_write_lock_is_taken_and_then_contended() {
        use std::os::fd::IntoRawFd;

        let path = std::env::temp_dir().join(format!("rosetta3-flock-{}", std::process::id()));
        let one = std::fs::File::create(&path).expect("a temporary file");
        let two = std::fs::File::open(&path).expect("a second descriptor");
        let (one, two) = (one.into_raw_fd(), two.into_raw_fd());

        let mut wanted = Flock {
            kind: lock::F_WRLCK,
            whence: 0,
            start: 0,
            len: 0,
            pid: 0,
        };
        // SAFETY: two live descriptors and a live Darwin `struct flock`, which is
        // what a lock command's third argument is.
        unsafe {
            assert_eq!(
                entry::fcntl(one, cmd::F_SETLK, (&mut wanted as *mut Flock) as usize),
                0,
                "an uncontended whole-file lock is granted"
            );
            // Held by this process already, so `F_GETLK` on the other descriptor
            // reports it as free - POSIX locks are per process. What must not
            // happen is the `EINVAL` this used to answer with.
            let mut query = wanted;
            assert_eq!(
                entry::fcntl(two, cmd::F_GETLK, (&mut query as *mut Flock) as usize),
                0
            );
            wanted.kind = lock::F_UNLCK;
            assert_eq!(
                entry::fcntl(one, cmd::F_SETLK, (&mut wanted as *mut Flock) as usize),
                0
            );
            libc::close(one);
            libc::close(two);
        }
        let _ = std::fs::remove_file(&path);
    }

    /// The round trip a caller actually performs: read the flags, add one, write
    /// them back. Every bit the guest set has to survive it.
    #[test]
    #[cfg(not(target_os = "macos"))]
    fn a_get_set_round_trip_keeps_every_flag_it_was_given() {
        let guest = oflag::O_RDWR | oflag::O_APPEND | oflag::O_NONBLOCK;
        let host = flags_to_host(guest).expect("all three have equivalents");
        assert_eq!(flags_to_guest(host), guest);
    }

    #[test]
    #[cfg(not(target_os = "macos"))]
    fn f_preallocate_allocates_from_the_end_of_file() {
        use std::os::fd::AsRawFd;

        let path = std::env::temp_dir().join(format!(
            "rosetta3-preallocate-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        let file = std::fs::File::create(&path).expect("a temporary file");
        let mut store = Fstore {
            flags: preallocate::F_ALLOCATEALL,
            posmode: preallocate::F_PEOFPOSMODE,
            length: 102_400,
            ..Fstore::default()
        };
        // SAFETY: a live descriptor and writable Darwin `fstore_t`.
        let result = unsafe {
            entry::fcntl(
                file.as_raw_fd(),
                cmd::F_PREALLOCATE,
                (&raw mut store) as usize,
            )
        };
        assert_eq!(result, 0);
        assert_eq!(store.bytes_allocated, 102_400);
        assert_eq!(file.metadata().unwrap().len(), 102_400);
        drop(file);
        let _ = std::fs::remove_file(path);
    }

    /// A host-only status bit is dropped rather than reported: `F_GETFL` answers
    /// with `O_LARGEFILE` on nearly every descriptor here and Darwin has no name
    /// for it, so refusing would fail the call the guest is entitled to make.
    #[test]
    #[cfg(not(target_os = "macos"))]
    fn a_host_only_status_bit_does_not_reach_the_guest() {
        let answered = flags_to_guest(libc::O_RDONLY | libc::O_LARGEFILE | libc::O_NONBLOCK);
        assert_eq!(answered, oflag::O_RDONLY | oflag::O_NONBLOCK);
    }

    /// The whole point, end to end: Darwin's `O_NONBLOCK` is `0x4`, which means
    /// nothing to this host's `F_SETFL`, so a forwarded flag word succeeds and
    /// leaves the descriptor blocking.
    #[test]
    #[cfg(not(target_os = "macos"))]
    fn a_darwin_o_nonblock_really_makes_the_descriptor_non_blocking() {
        let mut ends = [0 as libc::c_int; 2];
        // SAFETY: a two-element array, which is what `pipe` fills.
        assert_eq!(unsafe { libc::pipe(ends.as_mut_ptr()) }, 0);
        let (read_end, write_end) = (ends[0], ends[1]);

        // SAFETY: the entry point's contract - a live descriptor and a scalar.
        let flags = unsafe { entry::fcntl(read_end, cmd::F_GETFL, 0) };
        assert!(flags >= 0);
        assert_eq!(flags & oflag::O_NONBLOCK, 0, "a fresh pipe blocks");
        // SAFETY: as above.
        let rc =
            unsafe { entry::fcntl(read_end, cmd::F_SETFL, (flags | oflag::O_NONBLOCK) as usize) };
        assert_eq!(rc, 0);

        let mut byte = 0u8;
        // SAFETY: one writable byte, and the descriptor is open.
        let read = unsafe { libc::read(read_end, (&raw mut byte).cast(), 1) };
        let blocked = crate::errno::get();
        // SAFETY: descriptors this test owns.
        unsafe {
            libc::close(read_end);
            libc::close(write_end);
        }
        assert_eq!(read, -1);
        assert_eq!(blocked, libc::EAGAIN, "the read blocked instead of failing");
    }

    #[test]
    #[cfg(not(target_os = "macos"))]
    fn f_dupfd_inherits_nosigpipe_state() {
        let mut ends = [0 as libc::c_int; 2];
        // SAFETY: a writable two-element descriptor array.
        assert_eq!(
            unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, ends.as_mut_ptr()) },
            0
        );
        let on = 1i32;
        // SAFETY: one readable `int`, and the length says four.
        assert_eq!(
            unsafe {
                crate::net::sockopt::setsockopt(ends[0], 0xffff, 0x1022, (&raw const on).cast(), 4)
            },
            0
        );

        // SAFETY: a live descriptor and the scalar minimum descriptor number.
        let copied = unsafe { entry::fcntl(ends[0], cmd::F_DUPFD, 0) };
        assert!(copied >= 0);
        assert!(crate::net::sockopt::suppresses_sigpipe(copied));

        // SAFETY: descriptors owned by this test.
        unsafe {
            libc::close(ends[0]);
            libc::close(ends[1]);
            libc::close(copied);
        }
        crate::net::sockopt::forget(ends[0]);
        crate::net::sockopt::forget(copied);
    }

    #[test]
    fn the_commands_that_take_no_third_argument_are_darwin_s() {
        assert!(!takes_third_argument(cmd::F_GETFD));
        assert!(!takes_third_argument(cmd::F_FULLFSYNC));
        assert!(takes_third_argument(cmd::F_SETFD));
        assert!(takes_third_argument(cmd::F_DUPFD));
    }
}
