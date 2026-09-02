//! Names whose Darwin and host ABIs differ, and which this crate has not yet bridged.

// The reasons, shared by the entries that give them. Only the non-macOS lists have
// entries, so on macOS these name nothing - which is the same statement the empty
// lists below make.
#[cfg(not(target_os = "macos"))]
mod why {
    pub const STAT: &str = "struct stat is laid out differently";
    pub const DIRENT: &str = "struct dirent is laid out differently";
    pub const SIGNAL: &str = "the signal numbers and sigaction layout differ";
    pub const TERMIOS: &str = "struct termios is laid out differently";
    pub const MACH: &str = "Mach is a Darwin kernel interface";
    pub const SYSCTL: &str = "reimplemented; see darwin::sysctl";
    pub const XATTR: &str =
        "the calls take two more arguments and the names live in a namespace here";
    pub const ERRNO: &str = "the errno numbers diverge above 34; see darwin::errno";
    pub const SOCKET: &str = "struct sockaddr and the AF_/MSG_ constants differ; see darwin::net";
    pub const OBJC: &str = "the Objective-C runtime is macOS-only";
    pub const PTHREAD: &str = "no Darwin-compatible implementation; see darwin::pthread for the family this \
         crate does implement";
}

#[cfg(not(target_os = "macos"))]
use why::*;

/// Exact names that must not reach the host. See [`PREFIXES`] for the families.
#[cfg(not(target_os = "macos"))]
pub const NAMES: &[(&str, &str)] = &[
    // `struct stat` differs in field order, in the width of `st_ino`, and in whether
    // the timespecs are `st_atimespec` or `st_atim`. The `$INODE64` spellings are the
    // same function; `rosetta3::abi::table` already folds the suffix away.
    ("stat", STAT),
    ("fstat", STAT),
    ("lstat", STAT),
    ("fstatat", STAT),
    ("statfs", STAT),
    ("fstatfs", STAT),
    // `struct dirent`: `d_ino` width and the position of `d_type`.
    ("readdir", DIRENT),
    ("readdir_r", DIRENT),
    ("scandir", DIRENT),
    ("alphasort", DIRENT),
    ("getdirentries", DIRENT),
    // The comparator's arguments are `(thunk, a, b)` on Darwin and `(a, b, thunk)`
    // under glibc. Passing this through calls the guest's comparator with its
    // arguments rotated, which sorts and does not crash.
    (
        "qsort_r",
        "the comparator takes its context argument in a different position",
    ),
    // Darwin's is the POSIX `int` form; glibc's returns `char *` and may not touch
    // the caller's buffer at all. Kept listed although `crate::string` now answers
    // it: the entry is what stops the host's own from being bound instead, and
    // `reason` carves an implemented name back out.
    (
        "strerror_r",
        "glibc returns a pointer rather than filling the caller's buffer",
    ),
    // `struct sigaction` differs in layout and the `SA_*` differ in value; a guest
    // handler installed through the host's would be entered with a `ucontext_t` that
    // is not the one Darwin describes. `rosetta3::signal` shims these.
    ("sigaction", SIGNAL),
    ("sigaltstack", SIGNAL),
    ("sigprocmask", SIGNAL),
    ("sigsuspend", SIGNAL),
    // Darwin's `SIGINFO`, `SIGEMT` and `SIGBUS` are not the host's numbers.
    ("signal", SIGNAL),
    ("raise", SIGNAL),
    ("kill", SIGNAL),
    ("sigemptyset", SIGNAL),
    ("sigfillset", SIGNAL),
    ("sigaddset", SIGNAL),
    ("sigdelset", SIGNAL),
    ("sigismember", SIGNAL),
    ("sigpending", SIGNAL),
    ("killpg", SIGNAL),
    // The number names a different signal on each host, so the *name* it prints
    // is wrong rather than missing - `strerror`'s trap, one table over.
    ("strsignal", SIGNAL),
    ("psignal", SIGNAL),
    // `RLIMIT_*` are not the same numbers, and `rlim_t` differs in width.
    ("getrlimit", "the RLIMIT_ resource numbers differ"),
    ("setrlimit", "the RLIMIT_ resource numbers differ"),
    // `FD_SETSIZE` is 1024 on both, but `fd_set`'s word width is not, and Darwin's
    // `POLL*` are not the host's bits.
    ("select", "fd_set is laid out differently"),
    ("pselect", "fd_set is laid out differently"),
    // Listed although `crate::poll` now answers it, for the reason `strerror_r`
    // is: the entry is what stops the host's own from being bound instead.
    ("poll", "the POLL* event bits differ"),
    // `crate::fcntl` answers it. The commands agree only as far as `F_SETFL`, and
    // the flag word that command takes is an `open` flag word, which agrees on
    // nothing past the access mode.
    ("fcntl", "the fcntl commands and the O_ status flags differ"),
    // `struct termios` field order, and the `c_cc` indices.
    ("tcgetattr", TERMIOS),
    ("tcsetattr", TERMIOS),
    // The socket API, every layer of which disagrees; `crate::net` answers all of
    // these, and the entries are what stop the host's own being bound instead.
    // `listen`, `shutdown` and the `inet_addr` family are absent on purpose: they
    // carry no platform constant.
    ("socket", SOCKET),
    ("socketpair", SOCKET),
    ("bind", SOCKET),
    ("connect", SOCKET),
    ("accept", SOCKET),
    ("getsockname", SOCKET),
    ("getpeername", SOCKET),
    ("send", SOCKET),
    ("recv", SOCKET),
    ("sendto", SOCKET),
    ("recvfrom", SOCKET),
    ("sendmsg", SOCKET),
    ("recvmsg", SOCKET),
    ("inet_pton", SOCKET),
    ("inet_ntop", SOCKET),
    (
        "setsockopt",
        "SOL_SOCKET and every SO_ name are different numbers",
    ),
    (
        "getsockopt",
        "SOL_SOCKET and every SO_ name are different numbers",
    ),
    ("getifaddrs", SOCKET),
    ("freeifaddrs", SOCKET),
    // Darwin's `struct addrinfo` orders `ai_addr` and `ai_canonname` the other way,
    // and its `AI_*`/`NI_*` are different bits.
    ("getaddrinfo", "struct addrinfo is laid out differently"),
    ("freeaddrinfo", "struct addrinfo is laid out differently"),
    (
        "gai_strerror",
        "the EAI_ codes differ, in value and in sign",
    ),
    ("getnameinfo", "the NI_ flags differ"),
    ("dladdr", "Dl_info is laid out differently"),
    // The reading half of the clock family is reimplemented; see darwin::clock for
    // why the `clockid_t` cannot be passed through. Setting the clock is refused
    // rather than translated: it would be the *host's* clock a guest moved.
    (
        "clock_settime",
        "the clockid_t values differ; see darwin::clock",
    ),
    ("sysctl", SYSCTL),
    ("sysctlbyname", SYSCTL),
    ("sysctlnametomib", SYSCTL),
    // `struct utsname`'s fields are 256 bytes on Darwin and 65 under glibc, so the
    // buffer the guest passes is not the size the host would fill.
    ("uname", "struct utsname has different field widths"),
    // Two extra arguments on Darwin, and a flat namespace where this host has a
    // partitioned one. `crate::xattr` answers all four.
    ("getxattr", XATTR),
    ("setxattr", XATTR),
    ("removexattr", XATTR),
    ("listxattr", XATTR),
    // The message a number names is not the same message: the `errno` tables
    // diverge above 34. `crate::string` answers both.
    ("strerror", ERRNO),
    ("perror", ERRNO),
];

#[cfg(target_os = "macos")]
pub const NAMES: &[(&str, &str)] = &[];

/// Families that must not reach the host.
#[cfg(not(target_os = "macos"))]
pub const PREFIXES: &[(&str, &str)] = &[
    // Mach is not an interface this host has. `rosetta3::shims::machthread` answers
    // the handful a guest actually needs.
    ("mach_", MACH),
    ("vm_", MACH),
    ("task_", MACH),
    ("thread_policy_", MACH),
    ("host_", MACH),
    ("semaphore_", MACH),
    ("bootstrap_", MACH),
    // dyld introspection, which `rosetta3::dyld` answers about the *guest's* images.
    ("dyld_", "the host has no dyld"),
    ("NSGet", "a Darwin process-introspection interface"),
    // The Objective-C runtime, libdispatch, and the frameworks above them. Not
    // stubbed: a guest that imports one is refused at the import rather than half an
    // hour into its run.
    ("objc_", OBJC),
    ("class_", OBJC),
    ("sel_", OBJC),
    ("protocol_", OBJC),
    ("dispatch_", "libdispatch is macOS-only"),
    ("CF", "CoreFoundation is macOS-only"),
    ("NS", "Foundation is macOS-only"),
    ("CG", "CoreGraphics is macOS-only"),
    ("CM", "CoreMedia is macOS-only"),
    ("IO", "IOKit is macOS-only"),
    ("Audio", "CoreAudio is macOS-only"),
    // The whole threading family, minus what `crate::pthread` implements or vouches
    // for - see [`reason`], which lets those through. Threading is the part of the C
    // library where a wrong constant does not fail but deadlocks, so a name nobody
    // has thought about is refused rather than passed through on the chance that it
    // agrees.
    ("pthread_", PTHREAD),
    ("os_unfair_lock", "a Darwin locking primitive"),
    ("os_log", "a Darwin logging interface"),
];

#[cfg(target_os = "macos")]
pub const PREFIXES: &[(&str, &str)] = &[];

pub fn reason(name: &str) -> Option<&'static str> {
    // A name this crate implements is not divergent, whatever family it is in.
    // Several of the prefixes below are denied wholesale precisely so that the
    // handful within them this crate does answer for can be carved back out
    // here: `mach_absolute_time` out of Mach, `dispatch_once_f` out of
    // libdispatch, and most of `pthread_`.
    if implemented_here(name) {
        return None;
    }
    if let Some((_, why)) = NAMES.iter().find(|(n, _)| *n == name) {
        return Some(why);
    }
    PREFIXES
        .iter()
        .find(|(p, _)| name.starts_with(p))
        .map(|(_, why)| *why)
}

/// Whether `crate` answers for `name` itself.
///
/// The threading family needs two more lists than the others: `PASSED_THROUGH`
/// is what this crate has looked at and vouched for, and `SHIMMED_ABOVE` what it
/// leaves to the runtime above. Neither has a `symbol` entry.
fn implemented_here(name: &str) -> bool {
    crate::pthread::PASSED_THROUGH.contains(&name)
        || crate::pthread::SHIMMED_ABOVE.contains(&name)
        || crate::is_implemented(name)
}

pub fn contains(name: &str) -> bool {
    reason(name).is_some()
}

/// Apply this list to a host resolver, so a denied name never becomes an address.
pub fn deny_in(provider: &mut machish::resolve::NativeDylibProvider) {
    for (name, _) in NAMES {
        // Both spellings: the resolver is asked with the Mach-O underscore.
        provider.deny(name);
        provider.deny(&format!("_{name}"));
    }
    for (prefix, _) in PREFIXES {
        provider.deny_prefix(prefix);
        provider.deny_prefix(&format!("_{prefix}"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(target_os = "macos")]
    fn a_mac_host_denies_nothing() {
        assert!(NAMES.is_empty() && PREFIXES.is_empty());
        assert!(!contains("readdir"));
    }

    #[test]
    #[cfg(not(target_os = "macos"))]
    fn a_divergence_is_named_rather_than_passed_through() {
        assert_eq!(reason("readdir"), Some(DIRENT));
        // A Mach call that reaches the kernel about *memory*, which is the half
        // of the family `crate::mach` does not answer.
        assert_eq!(reason("mach_vm_allocate"), Some(MACH));
        assert_eq!(
            reason("strlen"),
            None,
            "a genuine passthrough is not denied"
        );
    }

    /// The families denied by prefix have holes in them, and the holes are the
    /// point: a prefix is denied wholesale so that the few names this crate does
    /// answer for can be excused one at a time.
    #[test]
    #[cfg(not(target_os = "macos"))]
    fn a_name_this_crate_implements_is_not_denied_by_its_family() {
        for name in [
            "mach_absolute_time",
            "mach_timebase_info",
            // The synthetic port namespace; see [`crate::mach`]. The `mach_`,
            // `task_`, `host_`, `semaphore_` and `bootstrap_` prefixes are all
            // denied wholesale so that exactly this list can be carved back out.
            "mach_task_self",
            "mach_port_allocate",
            "mach_msg",
            "task_set_exception_ports",
            "host_get_exception_ports",
            "semaphore_wait",
            "bootstrap_check_in",
            "dispatch_once_f",
            // The libdispatch this crate implements, which the `dispatch_` prefix
            // would otherwise deny along with the rest of the family.
            "dispatch_async",
            "dispatch_semaphore_wait",
            "dispatch_queue_create",
        ] {
            assert_eq!(reason(name), None, "{name} is implemented and still denied");
        }
        // And the families around them are still shut: a member of one that this
        // crate has *not* implemented is refused rather than passed through.
        assert!(contains("dispatch_source_create"));
        // The half of Mach that is still refused: anything asking the kernel
        // about a task's memory, or about another process at all.
        assert!(contains("mach_vm_read"));
        assert!(contains("task_for_pid"));
        assert!(contains("task_threads"));
    }
}
