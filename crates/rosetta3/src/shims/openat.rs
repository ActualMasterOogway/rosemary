//! `open`, `openat` and `fcntl`: the variadics whose arity a flag word decides.
//!
//! ## Why these can be shimmed and `printf` cannot
//!
//! Same reason as [`ioctl`](super::ioctl), reached a different way. `printf`'s
//! argument list is arbitrarily long and its shape lives in a string, so
//! forwarding one means rendering it. These take **at most one** variadic
//! argument, and a fixed argument says whether it is there:
//!
//! - `int open(const char *path, int flags, ... /* mode_t */)` — the `mode`
//!   argument is read only when `flags` contains `O_CREAT`. POSIX says so, and
//!   passing a mode when `O_CREAT` is absent is harmless anyway, which is why
//!   the check is about not reading a register the guest never wrote rather than
//!   about the callee.
//! - `int fcntl(int fd, int cmd, ...)` — the third argument is an `int`, a
//!   pointer, or absent, entirely according to `cmd`. That one *is* a list of
//!   facts, and it is written out below.
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

use microlift::arch::x86_64::context::RegisterContext;

use crate::abi::{GuestMemory, Result};

use super::{bad_pointer, set_int_return};

/// `int open(const char *path, int flags, ...)`
pub fn open(symbol: &str, ctx: &mut RegisterContext, mem: &mut dyn GuestMemory) -> Result<()> {
    let path = read_path(symbol, ctx.rdi, mem)?;
    let flags = ctx.rsi as i32;
    let mode = mode_for(flags, ctx.rdx);

    // SAFETY: `path` is a NUL-terminated buffer this shim owns, and `open`'s
    // remaining arguments are scalars. Whether the path exists is the kernel's
    // judgement, reported through the return value.
    let rc = unsafe { libc::open(path.as_ptr(), flags, mode) };
    set_int_return(ctx, rc);
    Ok(())
}

/// `int openat(int fd, const char *path, int flags, ...)`
pub fn openat(symbol: &str, ctx: &mut RegisterContext, mem: &mut dyn GuestMemory) -> Result<()> {
    let dirfd = ctx.rdi as i32;
    let path = read_path(symbol, ctx.rsi, mem)?;
    let flags = ctx.rdx as i32;
    let mode = mode_for(flags, ctx.rcx);

    // SAFETY: as `open`.
    let rc = unsafe { libc::openat(dirfd, path.as_ptr(), flags, mode) };
    set_int_return(ctx, rc);
    Ok(())
}

/// `int fcntl(int fd, int cmd, ...)`
pub fn fcntl(_symbol: &str, ctx: &mut RegisterContext, _mem: &mut dyn GuestMemory) -> Result<()> {
    let fd = ctx.rdi as i32;
    let cmd = ctx.rsi as i32;
    let arg = if takes_third_argument(cmd) { ctx.rdx } else { 0 };

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
/// Without `O_CREAT` the register holds whatever the guest last left there. A
/// stale mode cannot make `open` fault, but it can make a *created* file appear
/// with permissions nobody asked for the moment the flag word is misread, so the
/// zero is the honest value rather than a defensive one.
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

/// Read a path argument out of the guest, as a C string this side owns.
fn read_path(
    symbol: &str,
    va: u64,
    mem: &dyn GuestMemory,
) -> Result<std::ffi::CString> {
    let bytes = mem.read_cstr(va, symbol)?;
    std::ffi::CString::new(bytes).map_err(|_| bad_pointer(symbol, va))
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

    #[test]
    fn the_argumentless_fcntl_commands_do_not_pass_a_register_on() {
        assert!(!takes_third_argument(libc::F_GETFD));
        assert!(!takes_third_argument(libc::F_GETFL));
        assert!(takes_third_argument(libc::F_SETFD));
        assert!(takes_third_argument(libc::F_SETLK));
    }
}
