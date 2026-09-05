//! Names both sides implement identically, so the host's own symbol is the answer.
//!
//! Nothing is defined here and nothing is wrapped: a claimed name is resolved by the
//! host resolver at the end of the chain, and the guest's import slot points straight
//! at musl's `strlen`. What this list *is* is the claim - a reviewable statement that
//! for this name the two platforms agree on the arguments, the return value, the
//! errno values and the memory the call touches.
//!
//! The claim is worth writing down even though it changes no behaviour, because the
//! alternative is that "which symbols are safe to pass through" exists only as the
//! absence of an entry in [`crate::divergent`], where nobody can read it.
//!
//! Not exhaustive, and not meant to be: a name that is on neither list is still passed
//! through. Adding one here is how a reviewer records that the question was asked.

/// Symbols this crate asserts are ABI-identical on Darwin and on the host.
///
/// C names, without the Mach-O underscore.
///
/// `access` and `fopen` were on this list and are not any more. The *call* is
/// identical; what is not is the filesystem underneath it, which on a Mac is
/// case-insensitive and here is not - so [`crate::stat`] wraps both with the
/// fallback that difference needs, and a claim of "the host's own symbol is the
/// answer" would be false off Darwin.
///
/// `qsort` and `bsearch` came off because of what they *call*: the comparator is
/// guest code, and entering it needs the guest's thread pointer on a host that is
/// not Darwin. See [`crate::callback`], which is where they are now.
///
/// `write` came off for a subtler reason: the call is identical and what it does
/// to a *socket* is not. `SO_NOSIGPIPE` is a property of the descriptor on
/// Darwin and of the call here, so a write to a socket the guest asked not to be
/// signalled about had nowhere to say so and killed the process. See
/// [`crate::net::socket`], which answers it and `writev` both.
pub const CLAIMED: &[&str] = &[
    // <string.h>. Pure functions over caller memory; no struct crosses the seam.
    "memchr",
    "memcmp",
    "memcpy",
    "memmove",
    "memset",
    "strcasecmp",
    "strcat",
    "strchr",
    "strcmp",
    "strcoll",
    "strcpy",
    "strcspn",
    "strdup",
    "strlen",
    "strncasecmp",
    "strncat",
    "strncmp",
    "strncpy",
    "strndup",
    "strnlen",
    "strpbrk",
    "strrchr",
    "strspn",
    "strstr",
    "strtok",
    "strtok_r",
    "bcmp",
    "bcopy",
    "bzero",
    "index",
    "rindex",
    // <stdlib.h>. `malloc` and friends operate on an address space both sides share.
    "abort",
    "abs",
    "atof",
    "atoi",
    "atol",
    "atoll",
    "calloc",
    "div",
    "free",
    "labs",
    "llabs",
    "malloc",
    "rand",
    "realloc",
    "srand",
    "strtod",
    "strtof",
    "strtol",
    "strtoll",
    "strtoul",
    "strtoull",
    // <ctype.h>, in the byte-at-a-time forms. The locale-aware `_l` variants take a
    // `locale_t`, which is not the same object on both sides, and are deliberately
    // absent.
    "isalnum",
    "isalpha",
    "isascii",
    "isblank",
    "iscntrl",
    "isdigit",
    "isgraph",
    "islower",
    "isprint",
    "ispunct",
    "isspace",
    "isupper",
    "isxdigit",
    "tolower",
    "toupper",
    // <unistd.h>. File descriptors are integers on both sides and the flags these
    // take are POSIX. `open`, `openat` and `fcntl` are not here: their flag values
    // differ and `rosetta3::shims::openat` bridges them.
    "_exit",
    "alarm",
    "chdir",
    "chown",
    "close",
    "dup",
    "dup2",
    // The exec family is *not* here: the program being launched is usually another
    // Mach-O, which this host's kernel cannot execute. See `crate::spawn`.
    "fsync",
    "ftruncate",
    "getcwd",
    "getegid",
    "geteuid",
    "getgid",
    "getpid",
    "getppid",
    "getuid",
    "isatty",
    "link",
    "lseek",
    "pipe",
    "pread",
    "pwrite",
    "read",
    "readlink",
    "rmdir",
    "sleep",
    "symlink",
    "sync",
    "truncate",
    "unlink",
    "usleep",
    // <stdio.h>, minus the `FILE *` globals, which are named differently and live in
    // [`crate::stdio`]. `FILE` itself never crosses: the guest receives a pointer and
    // hands it back.
    "clearerr",
    "fclose",
    "fdopen",
    "feof",
    "ferror",
    "fflush",
    "fgetc",
    "fgets",
    "fileno",
    "fputc",
    "fputs",
    "fread",
    "freopen",
    "fseek",
    "fseeko",
    "ftell",
    "ftello",
    "fwrite",
    "getchar",
    "gets",
    "putchar",
    "puts",
    "remove",
    "rename",
    "rewind",
    "setbuf",
    "setvbuf",
    "tmpfile",
    "ungetc",
    // <math.h>. IEEE 754 on both, and `rosetta3::shims::math` already intercepts the
    // handful where Darwin's rounding of a corner case differs.
    "acos",
    "asin",
    "atan",
    "atan2",
    "cbrt",
    "ceil",
    "cos",
    "cosh",
    "exp",
    "exp2",
    "expm1",
    "fabs",
    "floor",
    "fmax",
    "fmin",
    "fmod",
    "hypot",
    "log",
    "log10",
    "log1p",
    "log2",
    "pow",
    "round",
    "sin",
    "sinh",
    "sqrt",
    "tan",
    "tanh",
    "trunc",
    "clock",
    "difftime",
    "mktime",
    "time",
    // Environment.
    // Not `atexit`: glibc keeps it in `libc_nonshared.a` and exports no dynamic
    // symbol for it, so there is nothing to pass through to. `rosetta3`'s
    // `shims::atexit` registers the handler through `__cxa_atexit` instead,
    // which it would have to do anyway - the handler is a guest address.
    "getenv",
    "putenv",
    "setenv",
    "unsetenv",
    "exit",
    // <sys/mman.h>, the half of it that takes no flag word. `PROT_READ`,
    // `PROT_WRITE` and `PROT_EXEC` are the same three bits on both sides, an
    // address and a length mean the same thing, and each of these returns `0` or
    // `-1` with the same errno values.
    //
    // The other half - `mmap`, `madvise`, `posix_madvise`, `msync` - is *not*
    // here and must not be: their `MAP_`, `MADV_` and `MS_` constants have
    // different values on the two platforms, so `crate::vmmap` rewrites them.
    "munmap",
    "mprotect",
    "mlock",
    "munlock",
    "mincore",
];

/// Whether this crate claims `name` is safe to pass through unchanged.
pub fn claimed(name: &str) -> bool {
    let name = crate::c_name(name);
    CLAIMED.contains(&name) || crate::pthread::PASSED_THROUGH.contains(&name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_claim_is_made_about_c_names_and_mach_o_ones_alike() {
        assert!(claimed("strlen"));
        assert!(claimed("_strlen"));
        assert!(
            !claimed("sysctl"),
            "a reimplemented name is not a passthrough"
        );
    }

    /// A claimed name that the host does not export is a claim about nothing, and
    /// almost always a typo.
    #[test]
    fn every_claimed_name_is_one_this_host_actually_exports() {
        let missing: Vec<&str> = CLAIMED
            .iter()
            .copied()
            .filter(|n| crate::host_symbol(n).is_none())
            .collect();
        assert!(
            missing.is_empty(),
            "claimed but not exported here: {missing:?}"
        );
    }

    #[test]
    fn the_list_has_no_duplicates() {
        let mut sorted = CLAIMED.to_vec();
        sorted.sort_unstable();
        let before = sorted.len();
        sorted.dedup();
        assert_eq!(before, sorted.len(), "a name is claimed twice");
    }
}
