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
    pub const OBJC: &str = "the Objective-C runtime is macOS-only";
    pub const PTHREAD: &str =
        "no Darwin-compatible implementation; see darwin::pthread for the family this \
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
    // the caller's buffer at all.
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
    // `RLIMIT_*` are not the same numbers, and `rlim_t` differs in width.
    ("getrlimit", "the RLIMIT_ resource numbers differ"),
    ("setrlimit", "the RLIMIT_ resource numbers differ"),
    // `FD_SETSIZE` is 1024 on both, but `fd_set`'s word width is not, and Darwin's
    // `POLL*` are not the host's bits.
    ("select", "fd_set is laid out differently"),
    ("pselect", "fd_set is laid out differently"),
    ("poll", "the POLL* event bits differ"),
    // `struct termios` field order, and the `c_cc` indices.
    ("tcgetattr", TERMIOS),
    ("tcsetattr", TERMIOS),
    // Darwin's `struct addrinfo` orders `ai_addr` and `ai_canonname` the other way,
    // and its `AI_*`/`NI_*` are different bits.
    ("getaddrinfo", "struct addrinfo is laid out differently"),
    ("getnameinfo", "the NI_ flags differ"),
    ("dladdr", "Dl_info is laid out differently"),
    ("sysctl", SYSCTL),
    ("sysctlbyname", SYSCTL),
    ("sysctlnametomib", SYSCTL),
    // `struct utsname`'s fields are 256 bytes on Darwin and 65 under glibc, so the
    // buffer the guest passes is not the size the host would fill.
    ("uname", "struct utsname has different field widths"),
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
    // The threading family is denied by prefix below, so the part of it this crate
    // answers for has to be excused here: the entries it implements, the handful it
    // has looked at and vouched for, and the few it leaves to the runtime above.
    if crate::pthread::PASSED_THROUGH.contains(&name)
        || crate::pthread::SHIMMED_ABOVE.contains(&name)
        || crate::pthread::symbol(name).is_some()
    {
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
        assert_eq!(reason("mach_absolute_time"), Some(MACH));
        assert_eq!(reason("strlen"), None, "a genuine passthrough is not denied");
    }
}
