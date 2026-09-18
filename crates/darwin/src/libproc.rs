//! `<libproc.h>`, which is how a Mac program asks about a process without Mach.
//!
//! Only the two flavours anything has needed: the list of open descriptors,
//! which a program pairs with `getrlimit(RLIMIT_NOFILE)` to see how close to
//! the limit it is running, and the task's memory and CPU totals.
//!
//! There is no host symbol to fall back on - libproc is Darwin's - so an
//! unbound `proc_pidinfo` is a call to address zero. Every other flavour is
//! refused the way libproc refuses one it cannot answer: zero bytes returned
//! and `errno` set, which callers already handle.

#![cfg(not(target_os = "macos"))]

use std::ffi::{c_int, c_void};

const PROC_PIDLISTFDS: c_int = 1;
const PROC_PIDTASKINFO: c_int = 4;

/// `struct proc_fdinfo`: a descriptor number and what kind of object it is.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct ProcFdInfo {
    fd: i32,
    kind: u32,
}

const FDTYPE_VNODE: u32 = 1;
const FDTYPE_SOCKET: u32 = 2;
const FDTYPE_KQUEUE: u32 = 5;
const FDTYPE_PIPE: u32 = 6;

/// `struct proc_taskinfo`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct ProcTaskInfo {
    virtual_size: u64,
    resident_size: u64,
    total_user: u64,
    total_system: u64,
    threads_user: u64,
    threads_system: u64,
    policy: i32,
    faults: i32,
    pageins: i32,
    cow_faults: i32,
    messages_sent: i32,
    messages_received: i32,
    syscalls_mach: i32,
    syscalls_unix: i32,
    context_switches: i32,
    threadnum: i32,
    numrunning: i32,
    priority: i32,
}

const _: () = {
    assert!(size_of::<ProcFdInfo>() == 8);
    assert!(size_of::<ProcTaskInfo>() == 96);
};

/// This process's open descriptors, and what each one is.
///
/// The kind is read from where `/proc/self/fd/N` points: this host names a
/// socket and a pipe in the link text, and everything else is a file as far as
/// a caller counting descriptors is concerned.
fn open_descriptors() -> Vec<ProcFdInfo> {
    let Ok(entries) = std::fs::read_dir("/proc/self/fd") else {
        return Vec::new();
    };
    let mut fds: Vec<ProcFdInfo> = entries
        .flatten()
        .filter_map(|entry| {
            let fd = entry.file_name().to_str()?.parse::<i32>().ok()?;
            let target = std::fs::read_link(entry.path()).unwrap_or_default();
            let target = target.to_string_lossy();
            let kind = match () {
                _ if target.starts_with("socket:") => FDTYPE_SOCKET,
                _ if target.starts_with("pipe:") => FDTYPE_PIPE,
                _ if target.contains("[eventpoll]") => FDTYPE_KQUEUE,
                _ => FDTYPE_VNODE,
            };
            Some(ProcFdInfo { fd, kind })
        })
        .collect();
    fds.sort_by_key(|entry| entry.fd);
    fds
}

/// The page counts `/proc/self/statm` reports, in bytes.
fn footprint() -> (u64, u64) {
    // SAFETY: `sysconf` reads a static system parameter and takes no pointer.
    let page = (unsafe { libc::sysconf(libc::_SC_PAGESIZE) }).max(4096) as u64;
    let Ok(text) = std::fs::read_to_string("/proc/self/statm") else {
        return (0, 0);
    };
    let mut fields = text
        .split_whitespace()
        .map(|field| field.parse::<u64>().unwrap_or(0));
    (
        fields.next().unwrap_or(0).saturating_mul(page),
        fields.next().unwrap_or(0).saturating_mul(page),
    )
}

/// `int proc_pidinfo(int pid, int flavor, uint64_t arg, void *buffer,
/// int buffersize)`
///
/// Returns the number of **bytes** written, which is what a caller divides by
/// the entry size to get a count. Zero is the failure, with `errno` saying why.
///
/// # Safety
///
/// `buffer` is null or writable for `size` bytes.
pub unsafe extern "C" fn proc_pidinfo(
    pid: c_int,
    flavor: c_int,
    _arg: u64,
    buffer: *mut c_void,
    size: c_int,
) -> c_int {
    // SAFETY: takes no argument and cannot fail.
    if pid != 0 && pid != unsafe { libc::getpid() } {
        // Everything below reads `/proc/self`. Answering about another process
        // with this one's figures is the failure this crate refuses elsewhere.
        crate::errno::set(libc::ESRCH);
        return 0;
    }
    if size < 0 {
        crate::errno::set(libc::EINVAL);
        return 0;
    }
    match flavor {
        PROC_PIDLISTFDS => {
            let fds = open_descriptors();
            let wanted = (fds.len() * size_of::<ProcFdInfo>()) as c_int;
            // A null buffer is the size enquiry libproc documents, and it is
            // how a caller sizes the allocation it is about to make.
            if buffer.is_null() {
                return wanted;
            }
            let room = (size as usize / size_of::<ProcFdInfo>()).min(fds.len());
            // SAFETY: `room` entries fit in what the caller said it has, and
            // the two do not overlap.
            unsafe {
                std::ptr::copy_nonoverlapping(fds.as_ptr(), buffer.cast::<ProcFdInfo>(), room)
            };
            (room * size_of::<ProcFdInfo>()) as c_int
        }
        PROC_PIDTASKINFO => {
            let wanted = size_of::<ProcTaskInfo>() as c_int;
            if buffer.is_null() {
                return wanted;
            }
            if size < wanted {
                crate::errno::set(libc::ENOMEM);
                return 0;
            }
            let (virtual_size, resident_size) = footprint();
            let info = ProcTaskInfo {
                virtual_size,
                resident_size,
                threadnum: threads(),
                ..ProcTaskInfo::default()
            };
            // SAFETY: the caller's buffer has room for the structure.
            unsafe { buffer.cast::<ProcTaskInfo>().write_unaligned(info) };
            wanted
        }
        // As libproc does for a flavour it has no answer for: nothing written,
        // and a caller that reads the return value sees it.
        _ => {
            crate::errno::set(libc::EINVAL);
            0
        }
    }
}

/// `int proc_name(int pid, void *buffer, uint32_t buffersize)`
///
/// The process's name, NUL-terminated; the length is returned, zero on a
/// failure. Qt's `QLockFile` asks it whether a lock file's owner is still the
/// program that wrote it, and removes the lock when it is not.
///
/// A process this runtime runs is named for its guest rather than for
/// `rosetta3`, which is what the guest would see on a Mac.
///
/// # Safety
///
/// `buffer` is null or writable for `size` bytes.
pub unsafe extern "C" fn proc_name(pid: c_int, buffer: *mut c_void, size: u32) -> c_int {
    let Ok(cmdline) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
        crate::errno::set(libc::ESRCH);
        return 0;
    };
    let Some(name) = process_name(&cmdline) else {
        crate::errno::set(libc::ESRCH);
        return 0;
    };
    // `p_name` is 2 * MAXCOMLEN bytes.
    let name = &name.as_bytes()[..name.len().min(32)];
    if buffer.is_null() || (size as usize) <= name.len() {
        crate::errno::set(libc::EINVAL);
        return 0;
    }
    // SAFETY: the caller's buffer holds `size` bytes, more than the name.
    unsafe {
        std::ptr::copy_nonoverlapping(name.as_ptr(), buffer.cast::<u8>(), name.len());
        *buffer.cast::<u8>().add(name.len()) = 0;
    }
    name.len() as c_int
}

/// The name a process's NUL-separated command line gives it: the file name
/// of its executable, or of its guest when that executable is `rosetta3 run`.
fn process_name(cmdline: &[u8]) -> Option<String> {
    let arguments: Vec<&[u8]> = cmdline.split(|b| *b == 0).collect();
    let base = |argument: &[u8]| {
        let text = String::from_utf8_lossy(argument).into_owned();
        text.rsplit('/').next().map(str::to_owned)
    };
    let program = base(arguments.first()?)?;
    if program == "rosetta3"
        && arguments.get(1).is_some_and(|a| *a == b"run")
        && let Some(guest) = arguments[2..]
            .iter()
            .find(|a| !a.is_empty() && !a.starts_with(b"-"))
    {
        return base(guest);
    }
    (!program.is_empty()).then_some(program)
}

/// How many threads this process is running.
fn threads() -> i32 {
    std::fs::read_dir("/proc/self/task").map_or(1, |entries| entries.count().max(1) as i32)
}

exports::exports! {
    functions {
        "proc_pidinfo" => proc_pidinfo,
        "proc_name" => proc_name,
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_guest_is_named_for_its_executable_not_the_runtime() {
        assert_eq!(
            process_name(b"/x/rosetta3\0run\0--emulated\0/A.app/Contents/MacOS/Studio\0-p\0"),
            Some("Studio".to_owned())
        );
        assert_eq!(
            process_name(b"/usr/bin/bash\0-c\0"),
            Some("bash".to_owned())
        );
    }

    use super::*;

    /// The call this exists for: how many descriptors are open, which is the
    /// return value divided by the entry size.
    #[test]
    fn the_descriptor_list_counts_what_this_process_has_open() {
        let mut fds = [ProcFdInfo::default(); 256];
        let size = std::mem::size_of_val(&fds) as c_int;
        // SAFETY: a buffer this test sized.
        let bytes = unsafe { proc_pidinfo(0, PROC_PIDLISTFDS, 0, fds.as_mut_ptr().cast(), size) };
        assert!(bytes > 0 && bytes % 8 == 0);
        let open = bytes as usize / size_of::<ProcFdInfo>();
        assert!(open >= 3, "at least the three standard descriptors");
        assert!(fds[..open].iter().all(|entry| entry.kind != 0));

        // And a null buffer is the size enquiry rather than a failure.
        // SAFETY: a null buffer is what this checks.
        let wanted = unsafe { proc_pidinfo(0, PROC_PIDLISTFDS, 0, std::ptr::null_mut(), 0) };
        assert!(wanted >= bytes);
    }

    /// A buffer that holds fewer entries than are open is filled to its own
    /// size, not past it.
    #[test]
    fn a_short_buffer_is_filled_to_its_own_length() {
        let mut fds = [ProcFdInfo::default(); 2];
        // SAFETY: a buffer this test sized, described honestly.
        let bytes = unsafe { proc_pidinfo(0, PROC_PIDLISTFDS, 0, fds.as_mut_ptr().cast(), 16) };
        assert_eq!(bytes, 16);
    }

    #[test]
    fn the_task_flavour_reports_this_processs_memory() {
        let mut info = ProcTaskInfo::default();
        // SAFETY: a live structure, described honestly.
        let bytes = unsafe {
            proc_pidinfo(
                0,
                PROC_PIDTASKINFO,
                0,
                (&raw mut info).cast(),
                size_of::<ProcTaskInfo>() as c_int,
            )
        };
        assert_eq!(bytes, 96);
        assert!(info.resident_size > 0);
        assert!(info.virtual_size >= info.resident_size);
        assert!(info.threadnum >= 1);
    }

    #[test]
    fn another_process_and_an_unknown_flavour_are_refused() {
        let mut info = ProcTaskInfo::default();
        let mut ask = |pid, flavor| {
            // SAFETY: a live structure, described honestly.
            unsafe {
                proc_pidinfo(
                    pid,
                    flavor,
                    0,
                    (&raw mut info).cast(),
                    size_of::<ProcTaskInfo>() as c_int,
                )
            }
        };
        assert_eq!(ask(0, 999), 0);
        assert_eq!(ask(1, PROC_PIDTASKINFO), 0, "not this process");
    }
}
