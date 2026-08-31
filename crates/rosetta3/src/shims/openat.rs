//! `open`, `openat`, `sem_open` and `fcntl`: the variadics whose arity a flag word
//! decides.

use machish::resolve::SyntheticDylib;

use microlift::arch::x86_64::context::RegisterContext;

use crate::abi::{GuestMemory, Result};

use super::set_int_return;

/// `int open(const char *path, int flags, unsigned mode)`, as the guest's own `open`
/// with the ellipsis spelled out.
///
/// The mode is a parameter here and was a register at the seam; nothing else about the
/// call changed.
///
/// # Safety
///
/// Called only through an import slot [`provide`] defines, so `path` is the guest's own
/// `const char *` - addressable, since the guest is identity-mapped - and the other two
/// arguments are scalars.
pub unsafe extern "C" fn open_with_mode(
    path: *const libc::c_char,
    flags: libc::c_int,
    mode: libc::c_uint,
) -> libc::c_int {
    let permissions = mode_for(flags, mode as u64);
    let Some(flags) = host_flags(flags) else {
        return -1;
    };
    // SAFETY: the caller's guarantee, above. Whether the path exists is the
    // kernel's judgement, reported through the return value and `errno`.
    unsafe { libc::open(path, flags, permissions) }
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
    let permissions = mode_for(flags, mode as u64);
    let Some(flags) = host_flags(flags) else {
        return -1;
    };
    // SAFETY: as `open_with_mode`.
    unsafe { libc::openat(dirfd, path, flags, permissions) }
}

/// The guest's `open` flags as this host's, or `None` having set `errno`.
fn host_flags(flags: libc::c_int) -> Option<libc::c_int> {
    match darwin::fcntl::flags_to_host(flags) {
        Ok(host) => Some(host),
        Err(unknown) => {
            say_unknown_open_flags(unknown);
            darwin::errno::set(libc::EINVAL);
            None
        }
    }
}

/// Name an `open` flag with no equivalent here, once for each distinct set.
///
/// `EINVAL` alone says the guest passed something wrong, which is not what happened.
fn say_unknown_open_flags(unknown: libc::c_int) {
    use std::sync::Mutex;
    static SAID: Mutex<Vec<libc::c_int>> = Mutex::new(Vec::new());
    let mut said = SAID.lock().unwrap_or_else(|e| e.into_inner());
    if said.contains(&unknown) {
        return;
    }
    said.push(unknown);
    eprintln!("rosetta3: open: no equivalent on this host for Darwin O_ flags {unknown:#x}");
}

/// `sem_t *sem_open(const char *name, int oflag, unsigned mode, unsigned value)`, with
/// the ellipsis spelled out.
///
/// Two optional arguments rather than one, and the same `O_CREAT` deciding them - but
/// unlike `open`'s mode, these cannot simply be passed on regardless.
///
/// # Safety
///
/// Called only through an import slot [`provide`] defines, so `name` is the guest's own
/// `const char *` and the rest are scalars.
pub unsafe extern "C" fn sem_open_with_mode(
    name: *const libc::c_char,
    oflag: libc::c_int,
    mode: libc::c_uint,
    value: libc::c_uint,
) -> *mut libc::sem_t {
    let Some(host_oflag) = host_flags(oflag) else {
        return libc::SEM_FAILED;
    };
    // Decided by the guest's flags; made with the host's.
    match darwin::fcntl::creates(oflag) {
        // SAFETY: the caller's guarantee, above. The mode is narrowed to the
        // `mode_t` the callee will read and re-promoted, exactly as `mode_for`
        // does - a guest register's spare bits are not permissions.
        true => unsafe {
            libc::sem_open(
                name,
                host_oflag,
                mode as libc::mode_t as libc::c_uint,
                value as libc::c_uint,
            )
        },
        // SAFETY: as above, and the two arguments the guest did not pass are
        // not passed on. See the note on the value's range.
        false => unsafe { libc::sem_open(name, host_oflag) },
    }
}

/// Claim `_open`, `_openat` and `_sem_open` for this module's wrappers.
pub(crate) fn provide(lib: &mut SyntheticDylib) {
    lib.define_fn("_open", open_with_mode as *const ());
    lib.define_fn("_openat", openat_with_mode as *const ());
    lib.define_fn("_sem_open", sem_open_with_mode as *const ());
}

/// The wrapper for a host symbol name, for a `dlsym` that would otherwise hand the
/// guest the host's variadic copy.
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

    // A command with no host equivalent is refused rather than forwarded under a
    // number that means something else here. See [`darwin::fcntl::to_host`].
    let Some(cmd) = darwin::fcntl::to_host(cmd) else {
        set_int_return(ctx, -1);
        darwin::errno::set(libc::EINVAL);
        return Ok(());
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
/// Without `O_CREAT` the argument holds whatever the guest last left in the register
/// the signature reads it from.
fn mode_for(flags: i32, register: u64) -> libc::c_int {
    if darwin::fcntl::creates(flags) {
        register as libc::mode_t as libc::c_int
    } else {
        0
    }
}

/// Whether `cmd` reads a third argument.
///
/// A question about the guest's call, so it is asked of the guest's own command
/// numbers - which past `F_SETFL` are not this host's. See [`darwin::fcntl`].
fn takes_third_argument(cmd: i32) -> bool {
    darwin::fcntl::takes_third_argument(cmd)
}

#[cfg(test)]
mod tests {
    use super::*;

    // The wrappers take the *guest's* flags, which are Darwin's on either host - so
    // the tests pass Darwin's, exactly as a guest would. Using `O_CREAT` here
    // would test the wrapper against the wrong caller.
    use darwin::fcntl::oflag::{O_CREAT, O_EXCL, O_RDONLY, O_WRONLY};

    #[test]
    fn a_mode_is_only_read_when_o_creat_asks_for_one() {
        assert_eq!(mode_for(O_RDONLY, 0o777), 0);
        assert_eq!(mode_for(O_WRONLY | O_CREAT, 0o644), 0o644);
        // The register may hold a whole pointer; only the mode bits are a mode.
        assert_eq!(mode_for(O_CREAT, 0xdead_beef_0000_01a4), 0o644);
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

        // SAFETY: a NUL-terminated path this test owns, and two scalars - the
        // arguments the import slot supplies.
        let fd = unsafe { open_with_mode(c.as_ptr(), O_WRONLY | O_CREAT | O_EXCL, 0o604) };
        assert!(fd >= 0, "open failed: {}", std::io::Error::last_os_error());
        // SAFETY: a descriptor this test just opened.
        unsafe { libc::close(fd) };

        // `0o604` is a mode with a hole in it - nothing for the group, a read
        // for everyone else - so neither a zero nor the usual `0o666` can be
        // mistaken for it.
        assert_eq!(
            permissions(&path) & 0o604,
            0o604 & !umask(),
            "mode was not the one passed"
        );

        let _ = std::fs::remove_file(&path);
    }

    /// And without `O_CREAT` there is no mode, whatever the register held.
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
        let fd = unsafe { open_with_mode(c.as_ptr(), O_RDONLY, 0o777) };
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
        let dirfd = unsafe { open_with_mode(c_dir.as_ptr(), O_RDONLY, 0) };
        assert!(dirfd >= 0, "opening the directory failed");

        let name = std::ffi::CString::new("relative").unwrap();
        // SAFETY: a descriptor and a NUL-terminated relative path.
        let fd =
            unsafe { openat_with_mode(dirfd, name.as_ptr(), O_WRONLY | O_CREAT | O_EXCL, 0o604) };
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

    /// The named semaphore the guest asked for, with the mode *and* the count it asked
    /// for.
    #[test]
    fn a_named_semaphore_gets_the_mode_and_the_count_it_was_opened_with() {
        // Darwin caps a semaphore name at `PSEMNAMLEN` (31) and wants a leading
        // slash; the pid keeps concurrent runs of this test apart.
        let name = std::ffi::CString::new(format!("/r3sem{}", std::process::id())).unwrap();
        // SAFETY: a NUL-terminated name this test owns, and three scalars.
        unsafe { libc::sem_unlink(name.as_ptr()) };

        // SAFETY: as above. `O_EXCL` so a leftover from a previous run is a
        // failure rather than a semaphore with somebody else's count.
        let sem = unsafe { sem_open_with_mode(name.as_ptr(), O_CREAT | O_EXCL, 0o600, 2) };
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

    /// The point of the whole rewrite, asked of the classifier the lifter reads: each
    /// is `Fast`, which is what makes the call an inlined one.
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
            assert_ne!(Some(wrapper), crate::host::symbol(name));
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
