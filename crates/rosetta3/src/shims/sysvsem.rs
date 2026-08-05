//! `semctl`: a variadic whose fourth argument is a union one word wide.
//!
//! Steam's `libtier0` names a lock across processes with System V semaphores.
//! `semget` and `semop` are ordinary signatures in [`crate::abi::table`] —
//! scalars and a `struct sembuf *` whose layout was compiled both ways and
//! compared. `semctl` cannot be one:
//!
//! ```c
//! int semctl(int semid, int semnum, int cmd, ... /* union semun arg */);
//! ```
//!
//! It is variadic, so the guest passed the fourth argument in `rcx` while the
//! host's own `semctl` will look for it *on the stack* — Apple's arm64 ABI
//! passes every variadic argument there. A signature cannot express that; a
//! shim can, because Rust calls a variadic `extern "C"` function by the
//! platform's rules. This is the same shape as [`fcntl`](super::openat), and
//! for the same reason it is a **list of commands** rather than a blanket
//! forward: `cmd` decides not just whether the argument exists but what it is.
//!
//! ## What the fourth argument may be, and why one word carries all of it
//!
//! `union semun` is `{ int val; struct semid_ds *buf; unsigned short *array; }`
//! — an `int`, or one of two pointers. Every member fits in a register, and the
//! callee's own use of it narrows the word, so the whole word is passed for
//! every command that takes one and a zero for every command that does not.
//! Withholding an argument a command needs is fatal; passing one it ignores is
//! not.
//!
//! Both pointer forms address guest memory, which is identity-mapped, so no
//! translation is needed — provided the structure behind the pointer is the same
//! on both architectures. It is: `struct semid_ds` is 72 bytes with `sem_base`
//! at 24, `sem_nsems` at 28, `sem_otime` at 32 and `sem_ctime` at 44, and
//! `struct ipc_perm` inside it is 24 with `_key` at 20 and `_seq` at 18, on
//! x86-64 and arm64 alike — compiled both ways and compared, which is the rule
//! every pointer argument is now held to. `unsigned short *` is an array of
//! fixed-width values and cannot differ.

use microlift::arch::x86_64::context::RegisterContext;

use crate::abi::{GuestMemory, Result};

use super::set_int_return;

/// The shim for `name`, if this module has one.
pub fn shim_for(
    name: &str,
) -> Option<fn(&str, &mut RegisterContext, &mut dyn GuestMemory) -> Result<()>> {
    match name {
        "semctl" => Some(semctl),
        _ => None,
    }
}

/// `int semctl(int semid, int semnum, int cmd, ...)`
fn semctl(_symbol: &str, ctx: &mut RegisterContext, _mem: &mut dyn GuestMemory) -> Result<()> {
    let semid = ctx.rdi as i32;
    let semnum = ctx.rsi as i32;
    let cmd = ctx.rdx as i32;
    let arg = if takes_fourth_argument(cmd) {
        ctx.rcx
    } else {
        0
    };

    // SAFETY: the fourth parameter is an `int` or a pointer into the guest,
    // which is identity-mapped and so is a pointer here; the commands that read
    // it are exactly the ones listed below. Whether the semaphore set exists is
    // the kernel's judgement, reported through the return value.
    let rc = unsafe { libc::semctl(semid, semnum, cmd, arg as usize) };
    set_int_return(ctx, rc);
    Ok(())
}

/// Whether `cmd` reads a fourth argument.
///
/// Written the opposite way round from [`fcntl`](super::openat)'s list: the
/// `semctl` commands are a short closed set, all ten of them named, so the ones
/// that take an argument can be listed directly and an unrecognised command
/// passes none. There is no command outside this set for a program to reach.
fn takes_fourth_argument(cmd: i32) -> bool {
    matches!(
        cmd,
        libc::IPC_STAT | libc::IPC_SET | libc::GETALL | libc::SETALL | libc::SETVAL
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// This shim reads no guest memory: the union argument is passed by value or
    /// by address, untouched either way.
    struct NoMemory;

    impl GuestMemory for NoMemory {
        fn read(&self, va: u64, out: &mut [u8]) -> core::result::Result<(), machish::Error> {
            Err(machish::Error::OutOfBounds {
                addr: va,
                len: out.len(),
            })
        }
        fn write(&mut self, va: u64, data: &[u8]) -> core::result::Result<(), machish::Error> {
            Err(machish::Error::OutOfBounds {
                addr: va,
                len: data.len(),
            })
        }
    }

    #[test]
    fn only_the_commands_with_a_union_argument_pass_a_register_on() {
        for cmd in [
            libc::IPC_STAT,
            libc::IPC_SET,
            libc::GETALL,
            libc::SETALL,
            libc::SETVAL,
        ] {
            assert!(takes_fourth_argument(cmd), "{cmd}");
        }
        for cmd in [
            libc::IPC_RMID,
            libc::GETNCNT,
            libc::GETPID,
            libc::GETVAL,
            libc::GETZCNT,
        ] {
            assert!(!takes_fourth_argument(cmd), "{cmd}");
        }
    }

    #[test]
    fn a_semaphore_set_can_be_made_read_and_removed_through_the_shim() {
        // The whole path, against the real kernel: create a set, `SETVAL` a
        // value into it, `GETVAL` it back, and `IPC_RMID` it — one command with
        // a fourth argument, one without, and one that destroys.
        let key = libc::IPC_PRIVATE;
        // SAFETY: `IPC_PRIVATE` always makes a fresh set, owned by this process.
        let id = unsafe { libc::semget(key, 1, 0o600 | libc::IPC_CREAT) };
        if id < 0 {
            // System V semaphores can be absent from a sandbox; the shim's own
            // decision table is still checked by the test above.
            return;
        }

        let mut ctx = RegisterContext::new();
        let mut mem = NoMemory;

        ctx.rdi = id as u64;
        ctx.rsi = 0;
        ctx.rdx = libc::SETVAL as u64;
        ctx.rcx = 7;
        semctl("semctl", &mut ctx, &mut mem).unwrap();
        assert_eq!(ctx.rax as i32, 0);

        ctx.rdx = libc::GETVAL as u64;
        ctx.rcx = 0;
        semctl("semctl", &mut ctx, &mut mem).unwrap();
        assert_eq!(ctx.rax as i32, 7);

        ctx.rdx = libc::IPC_RMID as u64;
        semctl("semctl", &mut ctx, &mut mem).unwrap();
        assert_eq!(ctx.rax as i32, 0);
    }
}
