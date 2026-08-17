//! `open`, `openat`, `sem_open` and `fcntl`: the variadics whose arity a flag
//! word decides.
//!
//! ## Why these can be shimmed and `printf` cannot
//!
//! Same reason as [`ioctl`](super::ioctl), reached a different way. `printf`'s
//! argument list is arbitrarily long and its shape lives in a string, so
//! forwarding one means rendering it. These take a **fixed, tiny** number of
//! variadic arguments, and a fixed argument says whether they are there:
//!
//! - `int open(const char *path, int flags, ... /* mode_t */)` — the `mode`
//!   argument is read only when `flags` contains `O_CREAT`. POSIX says so, and
//!   passing a mode when `O_CREAT` is absent is harmless anyway, which is why
//!   the check is about not reading a register the guest never wrote rather than
//!   about the callee.
//! - `sem_t *sem_open(const char *name, int oflag, ... /* mode_t, unsigned */)`
//!   — the same rule with the same flag, and *two* arguments rather than one.
//! - `int fcntl(int fd, int cmd, ...)` — the third argument is an `int`, a
//!   pointer, or absent, entirely according to `cmd`. That one *is* a list of
//!   facts, and it is written out below.
//!
//! ## Why `open`, `openat` and `sem_open` are no longer exits at all
//!
//! Everything above says their argument list is *known*, and a known argument
//! list is exactly what [`crate::abi::table`] describes and the lifter inlines.
//! What kept them at a VM exit was not the arity — it was that "the arity
//! depends on a flag" cannot be written in a signature, so both were marked
//! variadic and every `open` in the guest paid a register-file spill and fill, a
//! bridge, a symbol lookup and a generic marshaller.
//!
//! The flag rule does not have to live at the seam. It can live in the
//! **callee**, and then the seam has nothing left to decide: the table declares
//! `int open(const char *, int, unsigned)` with the mode always present, the
//! lifter bakes in an absolute call, and [`open_with_mode`] below applies the
//! `O_CREAT` rule with the same three lines the shim used to.
//!
//! The callee has to be one of ours. The host's `open` is genuinely variadic,
//! and Apple's arm64 ABI passes a variadic argument **on the stack** while a
//! fixed one goes in a register — so inlining a fixed 3-argument call to the
//! host's `open` would put the mode in `x2`, where the callee's `va_arg` never
//! looks, and a created file would get whatever the stack happened to hold.
//! That is why [`provide`] claims these names ahead of the native provider: the
//! address the guest binds to, promoted or bridged or trampolined, is this
//! module's wrapper, and the wrapper's mode is an ordinary parameter.
//!
//! `sem_open` is the one where a *signature alone* was already wrong. It was
//! spelled with its optional arguments present and still bound to the host's
//! variadic copy, so a guest creating a named semaphore handed the mode and the
//! initial value in `x2`/`x3` and the host read two `va_arg`s off the stack:
//! a semaphore with arbitrary permissions and an arbitrary count, or `EINVAL`
//! from a value above `SEM_VALUE_MAX`. The wrapper is what makes that signature
//! true, and it is the reason a wrapper is the *mechanism* here rather than an
//! optimisation.
//!
//! `fcntl` stays a shim. Its third argument is sometimes an `int`, sometimes a
//! `struct flock *` and sometimes absent, and the list below is what decides
//! which — a rule about a *register that may not have been written*, which is
//! not something a callee can recover once the call has been placed.
//!
//! ## What is no longer read through [`GuestMemory`]
//!
//! The path. A shim copied it out of guest memory to hold a `CString`; the
//! wrappers take the guest's pointer as a pointer, which is what every other
//! `P` argument in the table already does — the guest is identity-mapped, so the
//! host's own `open` reads the guest's bytes in place. A pointer the guest got
//! wrong now returns `EFAULT` from the kernel instead of an `AbiError`, which is
//! what the guest's own libc would have done for it.
//!
//! ## Why the third argument is passed as a word either way
//!
//! For `fcntl` the argument is sometimes `int` and sometimes `struct flock *`,
//! and the two disagree about the high half of the register. Passing the whole
//! word is right for the pointer commands and right for the integer ones too,
//! because the callee's own prototype narrows it — the guest's SysV convention
//! and the host's AAPCS64 both leave a narrow argument's high bits undefined,
//! and neither callee reads them.
//!
//! ## What is not translated
//!
//! Nothing needs to be. `mode_t`, `off_t`, `int` and every pointed-at structure
//! these commands take (`struct flock`, `struct radvisory`, `struct log2phys`)
//! have identical layouts on x86-64 and arm64 macOS, so the guest's buffer is
//! passed through by address. A command whose structure ever differed would need
//! translating *here*, by number, which is why this is a list rather than a
//! blanket forward.

use machish::resolve::SyntheticDylib;

use microlift::arch::x86_64::context::RegisterContext;

use crate::abi::{GuestMemory, Result};

use super::set_int_return;

/// `int open(const char *path, int flags, unsigned mode)`, as the guest's own
/// `open` with the ellipsis spelled out.
///
/// The mode is a parameter here and was a register at the seam; nothing else
/// about the call changed. `flags` still decides whether it is read, because
/// that is a fact about `open` and not about how the call arrived.
///
/// # Safety
///
/// Called only through an import slot [`provide`] defines, so `path` is the
/// guest's own `const char *` — addressable in this process, since the guest is
/// identity-mapped — and the other two arguments are scalars.
pub unsafe extern "C" fn open_with_mode(
    path: *const libc::c_char,
    flags: libc::c_int,
    mode: libc::c_uint,
) -> libc::c_int {
    // SAFETY: the caller's guarantee, above. Whether the path exists is the
    // kernel's judgement, reported through the return value and `errno`.
    unsafe { libc::open(path, flags, mode_for(flags, mode as u64)) }
}

/// `int openat(int fd, const char *path, int flags, unsigned mode)`.
///
/// # Safety
///
/// As [`open_with_mode`].
pub unsafe extern "C" fn openat_with_mode(
    dirfd: libc::c_int,
    path: *const libc::c_char,
    flags: libc::c_int,
    mode: libc::c_uint,
) -> libc::c_int {
    // SAFETY: as `open_with_mode`.
    unsafe { libc::openat(dirfd, path, flags, mode_for(flags, mode as u64)) }
}

/// `sem_t *sem_open(const char *name, int oflag, unsigned mode, unsigned value)`,
/// with the ellipsis spelled out.
///
/// Two optional arguments rather than one, and the same `O_CREAT` deciding
/// them — but unlike `open`'s mode, these cannot simply be passed on regardless.
/// A `value` above `SEM_VALUE_MAX` is `EINVAL` even where POSIX says the
/// argument is not there, so the flagless call has to be a genuinely
/// two-argument one. Which is what this is: the host's `sem_open` is variadic,
/// so calling it with two arguments and calling it with four are different
/// calls, and Rust makes both by the platform's own rules.
///
/// # Safety
///
/// Called only through an import slot [`provide`] defines, so `name` is the
/// guest's own `const char *` and the rest are scalars.
pub unsafe extern "C" fn sem_open_with_mode(
    name: *const libc::c_char,
    oflag: libc::c_int,
    mode: libc::c_uint,
    value: libc::c_uint,
) -> *mut libc::sem_t {
    match oflag & libc::O_CREAT != 0 {
        // SAFETY: the caller's guarantee, above. The mode is narrowed to the
        // `mode_t` the callee will read and re-promoted, exactly as `mode_for`
        // does — a guest register's spare bits are not permissions.
        true => unsafe {
            libc::sem_open(
                name,
                oflag,
                mode as libc::mode_t as libc::c_uint,
                value as libc::c_uint,
            )
        },
        // SAFETY: as above, and the two arguments the guest did not pass are
        // not passed on. See the note on the value's range.
        false => unsafe { libc::sem_open(name, oflag) },
    }
}

/// Claim `_open`, `_openat` and `_sem_open` for this module's wrappers.
///
/// Called from [`super::runtime::provider`], which sits ahead of the native
/// provider in [`crate::program::HostProviders`] — so a guest binding these
/// names gets a fixed-arity function whose `mode` is a parameter, which is the
/// whole reason they can be inlined at all. A guest image that defines its own
/// `open` still wins: loaded images are resolved before any provider.
pub(crate) fn provide(lib: &mut SyntheticDylib) {
    lib.define_fn("_open", open_with_mode as *const ());
    lib.define_fn("_openat", openat_with_mode as *const ());
    lib.define_fn("_sem_open", sem_open_with_mode as *const ());
}

/// The wrapper for a host symbol name, for a `dlsym` that would otherwise hand
/// the guest the host's variadic copy.
///
/// [`crate::dyld`]'s global fallback records a `dlsym` result so it can be
/// called later, and calls it with the signature the table gives its *name* —
/// which for these two is now a fixed one. Handing back the host's `open` there
/// would reintroduce exactly the register-versus-stack mismatch [`provide`]
/// exists to avoid, in the one path that does not go through a binding.
pub(crate) fn host_substitute(name: &str) -> Option<u64> {
    match name.strip_prefix('_').unwrap_or(name) {
        "open" => Some(open_with_mode as *const () as u64),
        "openat" => Some(openat_with_mode as *const () as u64),
        "sem_open" => Some(sem_open_with_mode as *const () as u64),
        _ => None,
    }
}

/// `int fcntl(int fd, int cmd, ...)`
pub fn fcntl(_symbol: &str, ctx: &mut RegisterContext, _mem: &mut dyn GuestMemory) -> Result<()> {
    let fd = ctx.rdi as i32;
    let cmd = ctx.rsi as i32;
    let arg = if takes_third_argument(cmd) {
        ctx.rdx
    } else {
        0
    };

    // SAFETY: the third parameter is a word-sized scalar or a pointer into the
    // guest, which is identity-mapped and so is a pointer here. Commands that
    // take no third argument are passed a zero rather than whatever the guest
    // last left in `rdx`, which is the same care `ioctl` takes for `IOC_VOID`.
    let rc = unsafe { libc::fcntl(fd, cmd, arg as usize) };
    set_int_return(ctx, rc);
    Ok(())
}

/// The `mode` argument, or zero when the guest passed none.
///
/// Without `O_CREAT` the argument holds whatever the guest last left in the
/// register the signature reads it from. A stale mode cannot make `open` fault,
/// but it can make a *created* file appear with permissions nobody asked for the
/// moment the flag word is misread, so the zero is the honest value rather than
/// a defensive one.
fn mode_for(flags: i32, register: u64) -> libc::c_int {
    if flags & libc::O_CREAT != 0 {
        register as libc::mode_t as libc::c_int
    } else {
        0
    }
}

/// Whether `cmd` reads a third argument.
///
/// The commands with no argument are the ones listed here; anything else is
/// assumed to take one, because passing a word to a command that ignores it is
/// harmless while withholding one it needs is not.
fn takes_third_argument(cmd: i32) -> bool {
    !matches!(
        cmd,
        libc::F_GETFD
            | libc::F_GETFL
            | libc::F_GETOWN
            | libc::F_FULLFSYNC
            | libc::F_FREEZE_FS
            | libc::F_THAW_FS
            | libc::F_NOCACHE
            | libc::F_GLOBAL_NOCACHE
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_mode_is_only_read_when_o_creat_asks_for_one() {
        assert_eq!(mode_for(libc::O_RDONLY, 0o777), 0);
        assert_eq!(mode_for(libc::O_WRONLY | libc::O_CREAT, 0o644), 0o644);
        // The register may hold a whole pointer; only the mode bits are a mode.
        assert_eq!(mode_for(libc::O_CREAT, 0xdead_beef_0000_01a4), 0o644);
    }

    /// The wrapper is the callee a fast import calls, so it has to *be* `open`:
    /// the file appears, and it appears with the mode the caller asked for.
    #[test]
    fn the_wrapper_creates_a_file_with_the_mode_it_was_given() {
        let dir = std::env::temp_dir().join("rosetta3-openat-shim");
        std::fs::create_dir_all(&dir).expect("a temp directory");
        let path = dir.join("created");
        let _ = std::fs::remove_file(&path);
        let c = std::ffi::CString::new(path.to_str().unwrap()).unwrap();

        // SAFETY: a NUL-terminated path this test owns, and two scalars — the
        // arguments the import slot supplies.
        let fd = unsafe {
            open_with_mode(
                c.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
                0o604,
            )
        };
        assert!(fd >= 0, "open failed: {}", std::io::Error::last_os_error());
        // SAFETY: a descriptor this test just opened.
        unsafe { libc::close(fd) };

        // `0o604` is a mode with a hole in it — nothing for the group, a read
        // for everyone else — so neither a zero nor the usual `0o666` can be
        // mistaken for it. `umask` can only clear bits, so what survives it is
        // what the test can insist on.
        assert_eq!(
            permissions(&path) & 0o604,
            0o604 & !umask(),
            "mode was not the one passed"
        );

        let _ = std::fs::remove_file(&path);
    }

    /// And without `O_CREAT` there is no mode, whatever the register held. A
    /// wrapper that passed it on regardless would be indistinguishable here —
    /// which is why the file is created first, with a mode of its own.
    #[test]
    fn a_mode_passed_without_o_creat_changes_nothing() {
        let dir = std::env::temp_dir().join("rosetta3-openat-shim");
        std::fs::create_dir_all(&dir).expect("a temp directory");
        let path = dir.join("existing");
        std::fs::write(&path, b"x").expect("a file to open");
        std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o600))
            .expect("a mode of its own");
        let c = std::ffi::CString::new(path.to_str().unwrap()).unwrap();

        // SAFETY: as above; the third argument is the stale register this shim
        // exists to not believe.
        let fd = unsafe { open_with_mode(c.as_ptr(), libc::O_RDONLY, 0o777) };
        assert!(fd >= 0, "open failed: {}", std::io::Error::last_os_error());
        // SAFETY: a descriptor this test just opened.
        unsafe { libc::close(fd) };

        assert_eq!(permissions(&path), 0o600);
        let _ = std::fs::remove_file(&path);
    }

    /// `openat` is the same function with a directory descriptor, and the
    /// relative path is what says the descriptor was used rather than ignored.
    #[test]
    fn the_openat_wrapper_resolves_against_the_directory_it_was_given() {
        let dir = std::env::temp_dir().join("rosetta3-openat-shim-dir");
        std::fs::create_dir_all(&dir).expect("a temp directory");
        let path = dir.join("relative");
        let _ = std::fs::remove_file(&path);
        let c_dir = std::ffi::CString::new(dir.to_str().unwrap()).unwrap();
        // SAFETY: a NUL-terminated path this test owns.
        let dirfd = unsafe { open_with_mode(c_dir.as_ptr(), libc::O_RDONLY, 0) };
        assert!(dirfd >= 0, "opening the directory failed");

        let name = std::ffi::CString::new("relative").unwrap();
        // SAFETY: a descriptor and a NUL-terminated relative path.
        let fd = unsafe {
            openat_with_mode(
                dirfd,
                name.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
                0o604,
            )
        };
        assert!(
            fd >= 0,
            "openat failed: {}",
            std::io::Error::last_os_error()
        );
        // SAFETY: two descriptors this test opened.
        unsafe {
            libc::close(fd);
            libc::close(dirfd);
        }

        assert!(path.exists(), "openat did not resolve against dirfd");
        assert_eq!(permissions(&path) & 0o604, 0o604 & !umask());
        let _ = std::fs::remove_file(&path);
    }

    /// The named semaphore the guest asked for, with the mode *and* the count
    /// it asked for.
    ///
    /// The count is the falsifiable half and the reason this test is here rather
    /// than in a signature's word: `sem_open` was already described with its
    /// optional arguments present while still bound to the host's variadic copy,
    /// so the two arrived in registers and the host read the stack. Two
    /// successful `sem_trywait`s and a third that fails say the value was 2.
    #[test]
    fn a_named_semaphore_gets_the_mode_and_the_count_it_was_opened_with() {
        // Darwin caps a semaphore name at `PSEMNAMLEN` (31) and wants a leading
        // slash; the pid keeps concurrent runs of this test apart.
        let name = std::ffi::CString::new(format!("/r3sem{}", std::process::id())).unwrap();
        // SAFETY: a NUL-terminated name this test owns, and three scalars.
        unsafe { libc::sem_unlink(name.as_ptr()) };

        // SAFETY: as above. `O_EXCL` so a leftover from a previous run is a
        // failure rather than a semaphore with somebody else's count.
        let sem =
            unsafe { sem_open_with_mode(name.as_ptr(), libc::O_CREAT | libc::O_EXCL, 0o600, 2) };
        assert_ne!(
            sem,
            libc::SEM_FAILED,
            "sem_open failed: {}",
            std::io::Error::last_os_error()
        );

        // SAFETY: a semaphore this test just created, decremented at most as
        // many times as it was created with plus one.
        unsafe {
            assert_eq!(libc::sem_trywait(sem), 0, "the count was below 1");
            assert_eq!(libc::sem_trywait(sem), 0, "the count was below 2");
            assert_eq!(libc::sem_trywait(sem), -1, "the count was above 2");
        }

        // And the flagless call, which must reach the host as a *two-argument*
        // call: the same semaphore, opened without creating one.
        // SAFETY: as above; the trailing pair is what the flag word says is not
        // there.
        let again = unsafe { sem_open_with_mode(name.as_ptr(), 0, 0o777, 0xffff_ffff) };
        assert_ne!(
            again,
            libc::SEM_FAILED,
            "reopening failed: {}",
            std::io::Error::last_os_error()
        );

        // SAFETY: two open semaphores this test owns, and the name it created.
        unsafe {
            libc::sem_close(again);
            libc::sem_close(sem);
            libc::sem_unlink(name.as_ptr());
        }
    }

    /// The point of the whole rewrite, asked of the classifier the lifter reads:
    /// each is `Fast`, which is what makes the call an inlined one. A `vsig!`
    /// entry or a re-added shim would answer `Slow` here.
    #[test]
    fn both_names_classify_as_fast_imports() {
        for name in ["_open", "_openat", "_sem_open"] {
            assert_eq!(
                crate::abi::classify(0, name),
                crate::abi::Classification::Fast,
                "{name} must be a fast import"
            );
            assert!(
                !super::super::is_shimmed(name),
                "{name} is no longer a shim"
            );
        }
    }

    /// And the address it is inlined against must be *ours*, because the host's
    /// copy of these two is variadic and would read the mode off the stack.
    #[test]
    fn the_substituted_address_is_never_the_hosts() {
        for name in ["open", "openat", "sem_open"] {
            let wrapper = host_substitute(name).expect("a wrapper for the name");
            assert_ne!(Some(wrapper), crate::dyld::host_symbol(name));
        }
        assert_eq!(host_substitute("openlog"), None);
    }

    /// The process's `umask`, read the only way it can be read: by setting it
    /// and putting it back.
    fn umask() -> u32 {
        // SAFETY: `umask` cannot fail; the second call restores what the first
        // took away. Racy against a concurrent `open` in another test thread,
        // which is why the window is two adjacent calls and nothing else.
        unsafe {
            let previous = libc::umask(0o777);
            libc::umask(previous);
            previous as u32
        }
    }

    /// A file's permission bits.
    fn permissions(path: &std::path::Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path)
            .expect("the file exists")
            .permissions()
            .mode()
            & 0o777
    }

    #[test]
    fn the_argumentless_fcntl_commands_do_not_pass_a_register_on() {
        assert!(!takes_third_argument(libc::F_GETFD));
        assert!(!takes_third_argument(libc::F_GETFL));
        assert!(takes_third_argument(libc::F_SETFD));
        assert!(takes_third_argument(libc::F_SETLK));
    }
}
