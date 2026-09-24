#![cfg(not(target_os = "macos"))]

use std::ffi::{c_int, c_void};

const PROC_PIDLISTFDS: c_int = 1;
const PROC_PIDTASKINFO: c_int = 4;

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

#[cfg(not(target_os = "freebsd"))]
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

#[cfg(target_os = "freebsd")]
fn open_descriptors() -> Result<Vec<ProcFdInfo>, i32> {
    let bytes = freebsd_proc_sysctl(libc::KERN_PROC_FILEDESC, unsafe { libc::getpid() })?;
    parse_freebsd_descriptors(&bytes)
}

#[cfg(any(test, target_os = "freebsd"))]
fn parse_freebsd_descriptors(bytes: &[u8]) -> Result<Vec<ProcFdInfo>, i32> {
    let mut fds = Vec::new();
    let mut offset = 0;
    while offset < bytes.len() {
        let header = bytes.get(offset..offset + 12).ok_or(libc::EINVAL)?;
        let record_size = i32::from_ne_bytes(header[0..4].try_into().unwrap());
        let kind = i32::from_ne_bytes(header[4..8].try_into().unwrap());
        let fd = i32::from_ne_bytes(header[8..12].try_into().unwrap());
        if record_size < 12 || record_size as usize > bytes.len() - offset {
            return Err(libc::EINVAL);
        }
        // The sysctl also includes pseudo descriptors for cwd, root, etc.
        if fd >= 0 {
            // KF_TYPE_* values from FreeBSD's <sys/user.h>.
            let kind = match kind {
                2 => FDTYPE_SOCKET,
                3 | 4 => FDTYPE_PIPE,
                5 => FDTYPE_KQUEUE,
                _ => FDTYPE_VNODE,
            };
            fds.push(ProcFdInfo { fd, kind });
        }
        offset += record_size as usize;
    }
    fds.sort_by_key(|entry| entry.fd);
    Ok(fds)
}

#[cfg(target_os = "freebsd")]
fn freebsd_proc_sysctl(flavor: c_int, pid: c_int) -> Result<Vec<u8>, i32> {
    let mut mib = [libc::CTL_KERN, libc::KERN_PROC, flavor, pid];
    let mut needed = 0usize;
    // SAFETY: the MIB and output length are valid; a null output asks for size.
    if unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as u32,
            std::ptr::null_mut(),
            &raw mut needed,
            std::ptr::null_mut(),
            0,
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error()
            .raw_os_error()
            .unwrap_or(libc::EIO));
    }
    // The descriptor table and command line may grow between the two calls.
    for _ in 0..4 {
        let mut bytes = vec![0u8; needed.saturating_add(4096)];
        let mut length = bytes.len();
        // SAFETY: `bytes` is writable for `length` bytes, and MIB is valid.
        if unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                mib.len() as u32,
                bytes.as_mut_ptr().cast(),
                &raw mut length,
                std::ptr::null_mut(),
                0,
            )
        } == 0
        {
            bytes.truncate(length);
            return Ok(bytes);
        }
        let error = std::io::Error::last_os_error()
            .raw_os_error()
            .unwrap_or(libc::EIO);
        if error != libc::ENOMEM {
            return Err(error);
        }
        needed = length.max(bytes.len().saturating_mul(2));
    }
    Err(libc::ENOMEM)
}

#[cfg(target_os = "linux")]
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

#[cfg(target_os = "freebsd")]
fn task_info() -> Result<ProcTaskInfo, i32> {
    let bytes = freebsd_proc_sysctl(libc::KERN_PROC_PID, unsafe { libc::getpid() })?;
    if bytes.len() < size_of::<libc::kinfo_proc>() {
        return Err(libc::EINVAL);
    }
    // SAFETY: the sysctl returned at least one complete `kinfo_proc`.
    let proc = unsafe { bytes.as_ptr().cast::<libc::kinfo_proc>().read_unaligned() };
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
    // SAFETY: `getrusage` writes a complete `rusage` on success.
    if unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error()
            .raw_os_error()
            .unwrap_or(libc::EIO));
    }
    // SAFETY: the call above initialized the structure.
    let usage = unsafe { usage.assume_init() };
    let nanos = |time: libc::timeval| {
        (time.tv_sec as u64).saturating_mul(1_000_000_000)
            + (time.tv_usec as u64).saturating_mul(1_000)
    };
    // SAFETY: `sysconf` reads a static system parameter.
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) }.max(1) as u64;
    Ok(ProcTaskInfo {
        virtual_size: proc.ki_size as u64,
        resident_size: (proc.ki_rssize as u64).saturating_mul(page_size),
        total_user: nanos(usage.ru_utime),
        total_system: nanos(usage.ru_stime),
        faults: usage.ru_minflt as i32,
        pageins: usage.ru_majflt as i32,
        context_switches: (usage.ru_nvcsw + usage.ru_nivcsw) as i32,
        threadnum: proc.ki_numthreads,
        ..ProcTaskInfo::default()
    })
}

pub unsafe extern "C" fn proc_pidinfo(
    pid: c_int,
    flavor: c_int,
    _arg: u64,
    buffer: *mut c_void,
    size: c_int,
) -> c_int {
    if pid != 0 && pid != unsafe { libc::getpid() } {
        crate::errno::set(libc::ESRCH);
        return 0;
    }
    if size < 0 {
        crate::errno::set(libc::EINVAL);
        return 0;
    }
    match flavor {
        PROC_PIDLISTFDS => {
            #[cfg(not(target_os = "freebsd"))]
            let fds = open_descriptors();
            #[cfg(target_os = "freebsd")]
            let fds = match open_descriptors() {
                Ok(fds) => fds,
                Err(error) => {
                    crate::errno::set(error);
                    return 0;
                }
            };
            let wanted = (fds.len() * size_of::<ProcFdInfo>()) as c_int;
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
            #[cfg(not(target_os = "freebsd"))]
            let (virtual_size, resident_size) = footprint();
            #[cfg(not(target_os = "freebsd"))]
            let info = ProcTaskInfo {
                virtual_size,
                resident_size,
                threadnum: threads(),
                ..ProcTaskInfo::default()
            };
            #[cfg(target_os = "freebsd")]
            let info = match task_info() {
                Ok(info) => info,
                Err(error) => {
                    crate::errno::set(error);
                    return 0;
                }
            };
            // SAFETY: the caller's buffer has room for the structure.
            unsafe { buffer.cast::<ProcTaskInfo>().write_unaligned(info) };
            wanted
        }
        _ => {
            crate::errno::set(libc::EINVAL);
            0
        }
    }
}

pub unsafe extern "C" fn proc_name(pid: c_int, buffer: *mut c_void, size: u32) -> c_int {
    #[cfg(target_os = "linux")]
    let Ok(cmdline) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
        crate::errno::set(libc::ESRCH);
        return 0;
    };
    #[cfg(target_os = "freebsd")]
    let cmdline = match freebsd_proc_sysctl(libc::KERN_PROC_ARGS, pid) {
        Ok(cmdline) => cmdline,
        Err(error) => {
            crate::errno::set(error);
            return 0;
        }
    };
    let Some(name) = process_name(&cmdline) else {
        crate::errno::set(libc::ESRCH);
        return 0;
    };
    // FIXME (PORTABILITY): may be different on other systems
    const MAXCOMLEN: usize = 16;
    // `p_name` is 2 * MAXCOMLEN bytes.
    let name = &name.as_bytes()[..name.len().min(MAXCOMLEN * 2)];
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
#[cfg(target_os = "linux")]
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
    fn freebsd_descriptor_records_skip_pseudo_fds_and_use_record_lengths() {
        let mut bytes = Vec::new();
        for (length, kind, fd) in [(16i32, 1i32, -1i32), (24, 2, 7), (16, 3, 4)] {
            bytes.extend_from_slice(&length.to_ne_bytes());
            bytes.extend_from_slice(&kind.to_ne_bytes());
            bytes.extend_from_slice(&fd.to_ne_bytes());
            bytes.resize(bytes.len() + length as usize - 12, 0);
        }
        let fds = parse_freebsd_descriptors(&bytes).unwrap();
        assert_eq!(fds.iter().map(|entry| entry.fd).collect::<Vec<_>>(), [4, 7]);
        assert_eq!(fds[0].kind, FDTYPE_PIPE);
        assert_eq!(fds[1].kind, FDTYPE_SOCKET);
        bytes[16..20].copy_from_slice(&100i32.to_ne_bytes());
        assert!(matches!(
            parse_freebsd_descriptors(&bytes),
            Err(libc::EINVAL)
        ));
    }

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

        let wanted = unsafe { proc_pidinfo(0, PROC_PIDLISTFDS, 0, std::ptr::null_mut(), 0) };
        assert!(wanted > 0 && wanted % size_of::<ProcFdInfo>() as c_int == 0);
    }

    #[test]
    fn a_short_buffer_is_filled_to_its_own_length() {
        let mut fds = [ProcFdInfo::default(); 2];
        let bytes = unsafe { proc_pidinfo(0, PROC_PIDLISTFDS, 0, fds.as_mut_ptr().cast(), 16) };
        assert_eq!(bytes, 16);
    }

    #[test]
    fn the_task_flavour_reports_this_processs_memory() {
        let mut info = ProcTaskInfo::default();
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
