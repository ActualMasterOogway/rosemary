//! `wait`, `waitpid`, `wait3` and `wait4`, whose options and status words
//! are numbered Darwin's way.
//!
//! Two differences, both silent when passed through:
//!
//! - `WCONTINUED` is `0x10` on Darwin and `8` here, where `0x10` is not a
//!   defined bit - so a job-control shell reaping with `WUNTRACED|WCONTINUED`
//!   is refused with `EINVAL` on every call, and loops on it forever.
//! - The status word carries a signal number wherever a child was killed or
//!   stopped, and those are this host's numbers: a child stopped by `SIGTSTP`
//!   reports 20 here, which Darwin calls `SIGCHLD`. A continued child is
//!   `0xffff` here and `0x137f` - "stopped by `SIGCONT`" - on Darwin.
//!
//! `struct rusage` passes through: its members are `long`s but for the
//! `timeval`s' `tv_usec`, an `int` on Darwin, whose low half is at the same
//! offset on a little-endian host and never exceeds a million.

#![cfg(not(target_os = "macos"))]

use std::ffi::{c_int, c_void};

const DARWIN_WNOHANG: c_int = 0x01;
const DARWIN_WUNTRACED: c_int = 0x02;
const DARWIN_WCONTINUED: c_int = 0x10;

/// Darwin's status for a continued child: `_WSTOPPED` with `SIGCONT`, 19.
const DARWIN_CONTINUED: c_int = 0x137f;

/// Darwin's options as this host's; `None` for a bit with no meaning here.
fn options_to_host(options: c_int) -> Option<c_int> {
    let known = DARWIN_WNOHANG | DARWIN_WUNTRACED | DARWIN_WCONTINUED;
    if options & !known != 0 {
        return None;
    }
    let mut host = 0;
    if options & DARWIN_WNOHANG != 0 {
        host |= libc::WNOHANG;
    }
    if options & DARWIN_WUNTRACED != 0 {
        host |= libc::WUNTRACED;
    }
    if options & DARWIN_WCONTINUED != 0 {
        host |= libc::WCONTINUED;
    }
    Some(host)
}

/// This host's status word as Darwin's.
fn status_to_darwin(status: c_int) -> c_int {
    let signal = |host: c_int| crate::signal::signal_from_host(host).unwrap_or(host);
    if libc::WIFCONTINUED(status) {
        DARWIN_CONTINUED
    } else if libc::WIFSTOPPED(status) {
        0x7f | (signal(libc::WSTOPSIG(status)) << 8)
    } else if libc::WIFSIGNALED(status) {
        signal(libc::WTERMSIG(status)) | (status & 0x80)
    } else {
        // Exited: the code in the second byte, the same on both.
        status
    }
}

/// The shared body: ask the host, and translate the answer.
///
/// # Safety
///
/// `status` is null or writable, and `rusage` is null or a writable Darwin
/// `struct rusage`.
unsafe fn reap(
    pid: libc::pid_t,
    status: *mut c_int,
    options: c_int,
    rusage: *mut c_void,
) -> libc::pid_t {
    let Some(host_options) = options_to_host(options) else {
        crate::errno::set(libc::EINVAL);
        return -1;
    };
    let mut host_status = 0;
    // SAFETY: a local for the status, and the caller's `rusage`, whose layout
    // the module note explains.
    let reaped = unsafe { libc::wait4(pid, &mut host_status, host_options, rusage.cast()) };
    if reaped > 0 && !status.is_null() {
        // SAFETY: the caller's obligation.
        unsafe { *status = status_to_darwin(host_status) };
    }
    reaped
}

/// `pid_t wait(int *status)`
///
/// # Safety
///
/// As [`reap`].
pub unsafe extern "C" fn wait(status: *mut c_int) -> libc::pid_t {
    // SAFETY: the caller's obligation.
    unsafe { reap(-1, status, 0, std::ptr::null_mut()) }
}

/// `pid_t waitpid(pid_t pid, int *status, int options)`
///
/// # Safety
///
/// As [`reap`].
pub unsafe extern "C" fn waitpid(
    pid: libc::pid_t,
    status: *mut c_int,
    options: c_int,
) -> libc::pid_t {
    // SAFETY: the caller's obligation.
    unsafe { reap(pid, status, options, std::ptr::null_mut()) }
}

/// `pid_t wait3(int *status, int options, struct rusage *rusage)`
///
/// # Safety
///
/// As [`reap`].
pub unsafe extern "C" fn wait3(
    status: *mut c_int,
    options: c_int,
    rusage: *mut c_void,
) -> libc::pid_t {
    // SAFETY: the caller's obligation.
    unsafe { reap(-1, status, options, rusage) }
}

/// `pid_t wait4(pid_t pid, int *status, int options, struct rusage *rusage)`
///
/// # Safety
///
/// As [`reap`].
pub unsafe extern "C" fn wait4(
    pid: libc::pid_t,
    status: *mut c_int,
    options: c_int,
    rusage: *mut c_void,
) -> libc::pid_t {
    // SAFETY: the caller's obligation.
    unsafe { reap(pid, status, options, rusage) }
}

exports::exports! {
    functions {
        "wait" | "wait$NOCANCEL" => wait,
        "waitpid" | "waitpid$NOCANCEL" => waitpid,
        "wait3" => wait3,
        "wait4" | "wait4$NOCANCEL" => wait4,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn job_control_options_are_accepted() {
        assert_eq!(
            options_to_host(DARWIN_WUNTRACED | DARWIN_WCONTINUED),
            Some(libc::WUNTRACED | libc::WCONTINUED)
        );
        assert_eq!(options_to_host(0x1000), None);
    }

    #[test]
    fn statuses_carry_darwins_signal_numbers() {
        // Exited with 3.
        assert_eq!(status_to_darwin(3 << 8), 3 << 8);
        // Stopped by the host's SIGTSTP, which is Darwin's 18.
        assert_eq!(
            status_to_darwin(0x7f | (libc::SIGTSTP << 8)),
            0x7f | (18 << 8)
        );
        // Killed by the host's SIGUSR1, Darwin's 30, with a core.
        assert_eq!(status_to_darwin(libc::SIGUSR1 | 0x80), 30 | 0x80);
        // Continued.
        assert_eq!(status_to_darwin(0xffff), DARWIN_CONTINUED);
    }

    #[test]
    fn a_child_is_reaped_with_its_exit_status() {
        // SAFETY: the child only exits.
        let pid = unsafe { libc::fork() };
        if pid == 0 {
            // SAFETY: leaves the child without running the test harness's
            // exit handlers.
            unsafe { libc::_exit(7) };
        }
        let mut status = 0;
        // SAFETY: a local for the status.
        let reaped = unsafe { waitpid(pid, &mut status, DARWIN_WUNTRACED | DARWIN_WCONTINUED) };
        assert_eq!(reaped, pid);
        assert_eq!(status, 7 << 8);
    }
}
