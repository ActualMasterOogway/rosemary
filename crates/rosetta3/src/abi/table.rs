//! What the runtime knows about the functions a guest imports.
//!
//! ## Why a hand-written table
//!
//! A Mach-O symbol table records names, not types, and the host's headers are
//! not available to a running loader. The only honest source of a signature is
//! the embedder, so this is a table — and, per the milestone's scope note, a
//! table written for the symbols real programs import rather than for all of
//! libSystem. It grows when something fails, and a failure names the symbol.
//!
//! ## Three answers, not two
//!
//! [`lookup`] distinguishes *unknown* from *refused*, and the distinction is
//! the useful one. `_qsort` is not missing from this table by accident: it
//! takes a guest function pointer, and calling it would hand the host's `qsort`
//! an x86-64 address to call as arm64. Recording that as
//! [`Lookup::Refused`] with a reason turns a crash into a sentence, and keeps
//! it out of the "someone should write this signature" list where it would sit
//! forever.
//!
//! [`REFUSED`] therefore holds three kinds of thing:
//!
//! - **Data**, which is imported through a slot and read, never called.
//! - **Callback takers**, which need the host→guest thunk (M3).
//! - **Stack walkers**, where passthrough is not merely unimplemented but
//!   *wrong*: `setjmp` saves host registers the guest cannot restore, and the
//!   host unwinder cannot walk guest frames (M5).
//!
//! ## Names
//!
//! Keys are spelled without the leading underscore a Mach-O adds, so `_strlen`
//! is looked up as `strlen` and `___stdoutp` as `__stdoutp`. [`lookup`] also
//! retries with an x86-only symbol variant suffix removed — `$INODE64`,
//! `$UNIX2003`, `$1050` — mirroring the same fallback `machish`'s native
//! provider applies when *resolving* the symbol, for the same reason: arm64
//! never had the variant and the signature is the plain one's.
//!
//! ## Tags
//!
//! `tag` is the provider's verbatim echo (see the crate note on the seam). No
//! provider in this workspace assigns one yet, so every tag is zero and lookup
//! falls through to the name. The parameter is threaded now, rather than added
//! later, because a provider that *does* tag — a virtualized shim library, say,
//! which knows exactly which function it handed back — must be able to name a
//! signature without a string comparison.

use super::ArgType as T;

/// One function's signature, as microlift wants it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FunctionAbi {
    /// The name *without* the Mach-O underscore, and `&'static str` because
    /// that is what `DynamicSymbol` requires.
    pub name: &'static str,
    /// `[0]` is the return type, `[1..]` the parameters — the layout
    /// `DynamicSymbol::arg_types` uses, so no reshuffling happens at the seam.
    pub types: &'static [T],
    /// C-variadic. Never fast-called, and only callable if [`crate::shims`]
    /// implements it: the two platforms disagree about where variadic arguments
    /// live, so there is no generic forwarding path.
    pub variadic: bool,
}

impl FunctionAbi {
    pub fn ret(&self) -> T {
        self.types.first().copied().unwrap_or(T::None)
    }

    pub fn params(&self) -> &'static [T] {
        self.types.split_first().map_or(&[], |(_, rest)| rest)
    }
}

/// What the table has to say about a symbol.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lookup {
    Known(&'static FunctionAbi),
    /// Described, and deliberately not callable. The payload is why.
    Refused(&'static str),
    Unknown,
}

/// Strip the leading underscore a Mach-O import carries.
///
/// Exactly one, not all of them: `___stdoutp` is the C identifier `__stdoutp`,
/// and eating every underscore would collide it with `_stdoutp`.
fn c_name(symbol: &str) -> &str {
    symbol.strip_prefix('_').unwrap_or(symbol)
}

/// Look a symbol up by tag first, then by name.
pub fn lookup(tag: u64, symbol: &str) -> Lookup {
    if tag != 0
        && let Some(abi) = by_tag(tag)
    {
        return Lookup::Known(abi);
    }
    let name = c_name(symbol);
    match lookup_exact(name) {
        Lookup::Unknown => match name.split_once('$') {
            // `_fts_open$INODE64` and friends: the variant only ever existed on
            // x86, and its signature is the plain symbol's.
            Some((base, _)) if !base.is_empty() => lookup_exact(base),
            _ => Lookup::Unknown,
        },
        found => found,
    }
}

fn lookup_exact(name: &str) -> Lookup {
    if let Ok(i) = SIGNATURES.binary_search_by_key(&name, |abi| abi.name) {
        return Lookup::Known(&SIGNATURES[i]);
    }
    if let Ok(i) = REFUSED.binary_search_by_key(&name, |(n, _)| *n) {
        return Lookup::Refused(REFUSED[i].1);
    }
    Lookup::Unknown
}

/// Signatures reachable by a provider-assigned tag.
///
/// Empty today — see the module note. The indirection exists so that adding a
/// tagging provider is a change in one place rather than a new mechanism.
fn by_tag(_tag: u64) -> Option<&'static FunctionAbi> {
    None
}

/// `(ret, args...)`, in `DynamicSymbol` order.
macro_rules! sig {
    ($name:literal, $ret:expr $(, $arg:expr)* $(,)?) => {
        FunctionAbi { name: $name, types: &[$ret $(, $arg)*], variadic: false }
    };
}

/// As [`sig`], for a C-variadic function. The fixed arguments are listed; the
/// ellipsis is implied by the flag.
macro_rules! vsig {
    ($name:literal, $ret:expr $(, $arg:expr)* $(,)?) => {
        FunctionAbi { name: $name, types: &[$ret $(, $arg)* , T::VariadicList], variadic: true }
    };
}

const V: T = T::None;
const I32: T = T::SignedWord;
const U32: T = T::UnsignedWord;
const I64: T = T::SignedDoubleword;
const U64: T = T::UnsignedDoubleword;
const F32: T = T::SinglePrecisionFp;
const F64: T = T::DoublePrecisionFp;
const P: T = T::Pointer;
const U8: T = T::UnsignedByte;

/// Known signatures, **sorted by name** — `signatures_are_sorted_and_unique`
/// enforces it, because [`lookup_exact`] binary-searches and an out-of-order
/// entry would simply go missing rather than fail.
///
/// Types are the C declarations, narrowed where C narrows: `size_t` is `U64`,
/// `int` is `I32`, every pointer is `P` regardless of what it points at. A
/// function whose real signature passes or returns a struct by value does not
/// belong here at all — see [`super::marshal`].
///
/// Variadics are interleaved rather than grouped, because the array is one
/// sorted keyspace; `vsig!` marks them.
pub static SIGNATURES: &[FunctionAbi] = &[
    sig!("__assert_rtn", V, P, P, I32, P),
    sig!("__bzero", V, P, U64),
    sig!("__error", P),
    sig!("__maskrune", I32, I32, U64),
    sig!("__memcpy_chk", P, P, P, U64, U64),
    sig!("__memmove_chk", P, P, P, U64, U64),
    sig!("__memset_chk", P, P, I32, U64, U64),
    vsig!("__snprintf_chk", I32, P, U64, I32, U64, P),
    vsig!("__sprintf_chk", I32, P, I32, U64, P),
    sig!("__stack_chk_fail", V),
    sig!("__strcat_chk", P, P, P, U64),
    sig!("__strcpy_chk", P, P, P, U64),
    sig!("__strlcat_chk", U64, P, P, U64, U64),
    sig!("__strlcpy_chk", U64, P, P, U64, U64),
    sig!("__strncat_chk", P, P, P, U64, U64),
    sig!("__strncpy_chk", P, P, P, U64, U64),
    sig!("__tolower", I32, I32),
    sig!("__toupper", I32, I32),
    sig!("abort", V),
    sig!("abs", I32, I32),
    sig!("access", I32, P, I32),
    sig!("acl_free", I32, P),
    sig!("acl_get_entry", I32, P, I32, P),
    sig!("acl_get_flag_np", I32, P, I32),
    sig!("acl_get_flagset_np", I32, P, P),
    sig!("acl_get_link_np", P, P, I32),
    sig!("acl_get_perm_np", I32, P, I32),
    sig!("acl_get_permset", I32, P, P),
    sig!("acl_get_qualifier", P, P),
    sig!("acl_get_tag_type", I32, P, P),
    sig!("acos", F64, F64),
    sig!("asin", F64, F64),
    vsig!("asprintf", I32, P, P),
    sig!("atan", F64, F64),
    sig!("atan2", F64, F64, F64),
    sig!("atof", F64, P),
    sig!("atoi", I32, P),
    sig!("atol", I64, P),
    sig!("atoll", I64, P),
    sig!("bcmp", I32, P, P, U64),
    sig!("bzero", V, P, U64),
    sig!("calloc", P, U64, U64),
    sig!("ceil", F64, F64),
    sig!("chdir", I32, P),
    sig!("clearerr", V, P),
    sig!("close", I32, I32),
    sig!("closedir", I32, P),
    sig!("compat_mode", U8, P, P),
    sig!("cos", F64, F64),
    sig!("dlclose", I32, P),
    sig!("dlerror", P),
    sig!("dlopen", P, P, I32),
    sig!("dlsym", P, P, P),
    vsig!("dprintf", I32, I32, P),
    sig!("dup", I32, I32),
    sig!("dup2", I32, I32, I32),
    vsig!("err", V, I32, P),
    vsig!("errx", V, I32, P),
    sig!("exit", V, I32),
    sig!("exp", F64, F64),
    sig!("exp2", F64, F64),
    sig!("fabs", F64, F64),
    sig!("fabsf", F32, F32),
    sig!("fclose", I32, P),
    vsig!("fcntl", I32, I32, I32),
    sig!("fdopen", P, I32, P),
    sig!("feof", I32, P),
    sig!("ferror", I32, P),
    sig!("fflagstostr", P, U64),
    sig!("fflush", I32, P),
    sig!("fgetc", I32, P),
    sig!("fgets", P, P, I32, P),
    sig!("fileno", I32, P),
    sig!("floor", F64, F64),
    sig!("floorf", F32, F32),
    sig!("fmax", F64, F64, F64),
    sig!("fmin", F64, F64, F64),
    sig!("fmod", F64, F64, F64),
    sig!("fopen", P, P, P),
    vsig!("fprintf", I32, P, P),
    sig!("fputc", I32, I32, P),
    sig!("fputs", I32, P, P),
    sig!("fread", U64, P, U64, U64, P),
    sig!("free", V, P),
    vsig!("fscanf", I32, P, P),
    sig!("fseek", I32, P, I64, I32),
    sig!("fstat", I32, I32, P),
    sig!("ftell", I64, P),
    sig!("fts_children", P, P, I32),
    sig!("fts_close", I32, P),
    sig!("fts_read", P, P),
    sig!("fts_set", I32, P, P, I32),
    sig!("fwrite", U64, P, U64, U64, P),
    sig!("getbsize", P, P, P),
    sig!("getcwd", P, P, U64),
    sig!("getegid", U32),
    sig!("getenv", P, P),
    sig!("geteuid", U32),
    sig!("getgid", U32),
    sig!("getopt", I32, I32, P, P),
    sig!("getopt_long", I32, I32, P, P, P, P),
    sig!("getpid", I32),
    sig!("getprogname", P),
    sig!("getuid", U32),
    sig!("getxattr", I64, P, P, P, U64, U32, I32),
    sig!("group_from_gid", P, U32, I32),
    sig!("hypot", F64, F64, F64),
    vsig!("ioctl", I32, I32, U64),
    sig!("isalnum", I32, I32),
    sig!("isalpha", I32, I32),
    sig!("isatty", I32, I32),
    sig!("isdigit", I32, I32),
    sig!("islower", I32, I32),
    sig!("isprint", I32, I32),
    sig!("ispunct", I32, I32),
    sig!("isspace", I32, I32),
    sig!("isupper", I32, I32),
    sig!("isxdigit", I32, I32),
    sig!("kill", I32, I32, I32),
    sig!("labs", I64, I64),
    sig!("ldexp", F64, F64, I32),
    sig!("listxattr", I64, P, P, U64, I32),
    sig!("localtime", P, P),
    sig!("log", F64, F64),
    sig!("log10", F64, F64),
    sig!("log2", F64, F64),
    sig!("lseek", I64, I32, I64, I32),
    sig!("lstat", I32, P, P),
    sig!("malloc", P, U64),
    sig!("malloc_size", U64, P),
    sig!("mbrtowc", U64, P, P, U64, P),
    sig!("mbtowc", I32, P, P, U64),
    sig!("memchr", P, P, I32, U64),
    sig!("memcmp", I32, P, P, U64),
    sig!("memcpy", P, P, P, U64),
    sig!("memmove", P, P, P, U64),
    sig!("memset", P, P, I32, U64),
    sig!("mkdir", I32, P, U32),
    sig!("nl_langinfo", P, I32),
    vsig!("open", I32, P, I32),
    vsig!("openat", I32, I32, P, I32),
    sig!("opendir", P, P),
    sig!("perror", V, P),
    sig!("posix_memalign", I32, P, U64, U64),
    sig!("pow", F64, F64, F64),
    sig!("powf", F32, F32, F32),
    vsig!("printf", I32, P),
    sig!("putc", I32, I32, P),
    sig!("putchar", I32, I32),
    sig!("puts", I32, P),
    sig!("putwchar", I32, I32),
    sig!("rand", I32),
    sig!("read", I64, I32, P, U64),
    sig!("readdir", P, P),
    sig!("readlink", I64, P, P, U64),
    sig!("realloc", P, P, U64),
    sig!("reallocf", P, P, U64),
    sig!("rewind", V, P),
    sig!("rmdir", I32, P),
    sig!("round", F64, F64),
    vsig!("scanf", I32, P),
    sig!("setenv", I32, P, P, I32),
    sig!("setlocale", P, I32, P),
    sig!("setvbuf", I32, P, P, I32, U64),
    sig!("sin", F64, F64),
    sig!("sleep", U32, U32),
    vsig!("snprintf", I32, P, U64, P),
    vsig!("sprintf", I32, P, P),
    sig!("sqrt", F64, F64),
    sig!("sqrtf", F32, F32),
    sig!("srand", V, U32),
    vsig!("sscanf", I32, P, P),
    sig!("stat", I32, P, P),
    sig!("strcasecmp", I32, P, P),
    sig!("strcat", P, P, P),
    sig!("strchr", P, P, I32),
    sig!("strcmp", I32, P, P),
    sig!("strcoll", I32, P, P),
    sig!("strcpy", P, P, P),
    sig!("strcspn", U64, P, P),
    sig!("strdup", P, P),
    sig!("strerror", P, I32),
    sig!("strftime", U64, P, U64, P, P),
    sig!("strlcat", U64, P, P, U64),
    sig!("strlcpy", U64, P, P, U64),
    sig!("strlen", U64, P),
    sig!("strmode", V, I32, P),
    sig!("strncasecmp", I32, P, P, U64),
    sig!("strncat", P, P, P, U64),
    sig!("strncmp", I32, P, P, U64),
    sig!("strncpy", P, P, P, U64),
    sig!("strndup", P, P, U64),
    sig!("strnlen", U64, P, U64),
    sig!("strpbrk", P, P, P),
    sig!("strrchr", P, P, I32),
    sig!("strspn", U64, P, P),
    sig!("strstr", P, P, P),
    sig!("strtod", F64, P, P),
    sig!("strtok", P, P, P),
    sig!("strtol", I64, P, P, I32),
    sig!("strtoll", I64, P, P, I32),
    sig!("strtonum", I64, P, I64, I64, P),
    sig!("strtoul", U64, P, P, I32),
    sig!("strtoull", U64, P, P, I32),
    sig!("sysctlbyname", I32, P, P, P, P, U64),
    vsig!("syslog", V, I32, P),
    sig!("tan", F64, F64),
    sig!("tgetent", I32, P, P),
    sig!("tgetstr", P, P, P),
    sig!("tgoto", P, P, I32, I32),
    sig!("time", I64, P),
    sig!("tolower", I32, I32),
    sig!("toupper", I32, I32),
    sig!("trunc", F64, F64),
    sig!("umask", U32, U32),
    sig!("ungetc", I32, I32, P),
    sig!("unlink", I32, P),
    sig!("unsetenv", I32, P),
    sig!("user_from_uid", P, U32, I32),
    sig!("usleep", I32, U32),
    sig!("uuid_unparse_upper", V, P, P),
    vsig!("warn", V, P),
    vsig!("warnx", V, P),
    sig!("wcwidth", I32, I32),
    sig!("write", I64, I32, P, U64),
];

/// A symbol with no public declaration anywhere on this system.
///
/// The milestone's "explicit unknown list", and the honest end of the refusal
/// spectrum: these are private SPI whose argument list cannot be read out of a
/// header, so the choice is to guess or to say so. Guessing is what the whole
/// module exists not to do.
const UNDECLARED: &str = "no public declaration; a guessed signature would fail silently";

/// Symbols that are described and deliberately not callable, **sorted by name**
/// for the same reason as [`SIGNATURES`].
pub static REFUSED: &[(&str, &str)] = &[
    ("_DefaultRuneLocale", "a data import, read through its slot and never called"),
    ("__mb_cur_max", "a data import, read through its slot and never called"),
    ("__stack_chk_guard", "a data import, read through its slot and never called"),
    ("__stderrp", "a data import, read through its slot and never called"),
    ("__stdinp", "a data import, read through its slot and never called"),
    ("__stdoutp", "a data import, read through its slot and never called"),
    ("_longjmp", "restores a stack the host's implementation never saw (M5)"),
    ("_setjmp", "saves host registers, not the guest's (M5)"),
    ("atexit", "takes a guest function pointer; needs the host->guest thunk (M3)"),
    ("bsearch", "takes a guest function pointer; needs the host->guest thunk (M3)"),
    ("dyld_stub_binder", "the lazy-binding entry point; every slot is bound eagerly"),
    ("environ", "a data import, read through its slot and never called"),
    ("fts_open", "takes a guest comparator; needs the host->guest thunk (M3)"),
    ("humanize_number", UNDECLARED),
    ("longjmp", "restores a stack the host's implementation never saw (M5)"),
    ("mbr_identifier_translate", UNDECLARED),
    ("optarg", "a data import, read through its slot and never called"),
    ("opterr", "a data import, read through its slot and never called"),
    ("optind", "a data import, read through its slot and never called"),
    ("optopt", "a data import, read through its slot and never called"),
    ("pthread_create", "takes a guest function pointer; needs the host->guest thunk (M3)"),
    ("qsort", "takes a guest function pointer; needs the host->guest thunk (M3)"),
    ("qsort_r", "takes a guest function pointer; needs the host->guest thunk (M3)"),
    ("setjmp", "saves host registers, not the guest's (M5)"),
    ("sigaction", "installs a guest function pointer as a handler (M5)"),
    ("siglongjmp", "restores a stack the host's implementation never saw (M5)"),
    ("signal", "installs a guest function pointer as a handler (M5)"),
    ("sigsetjmp", "saves host registers, not the guest's (M5)"),
    ("tputs", "takes a guest output function; needs the host->guest thunk (M3)"),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signatures_are_sorted_and_unique() {
        for pair in SIGNATURES.windows(2) {
            assert!(
                pair[0].name < pair[1].name,
                "{} and {} are out of order; binary_search would miss one",
                pair[0].name,
                pair[1].name
            );
        }
    }

    #[test]
    fn refusals_are_sorted_and_unique() {
        for pair in REFUSED.windows(2) {
            assert!(pair[0].0 < pair[1].0, "{} and {} are out of order", pair[0].0, pair[1].0);
        }
    }

    #[test]
    fn a_symbol_is_never_both_known_and_refused() {
        for abi in SIGNATURES {
            assert!(
                REFUSED.binary_search_by_key(&abi.name, |(n, _)| *n).is_err(),
                "{} is both described and refused",
                abi.name
            );
        }
    }

    #[test]
    fn the_leading_underscore_is_stripped_exactly_once() {
        assert!(matches!(lookup(0, "_strlen"), Lookup::Known(_)));
        assert!(matches!(lookup(0, "___stdoutp"), Lookup::Refused(_)));
        assert!(matches!(lookup(0, "_stdoutp"), Lookup::Unknown));
    }

    #[test]
    fn an_x86_only_variant_falls_back_to_the_plain_symbol() {
        // The same fallback machish applies when resolving the address; arm64
        // never had `$INODE64`, so what is known about the variant is whatever
        // is known about the plain symbol — here, that it takes a comparator.
        assert_eq!(lookup(0, "_fts_open"), lookup(0, "_fts_open$INODE64"));
        assert!(matches!(lookup(0, "_fts_open$INODE64"), Lookup::Refused(_)));
        assert_eq!(lookup(0, "_strtod$UNIX2003"), lookup(0, "_strtod"));
    }

    #[test]
    fn a_variadic_carries_its_fixed_arguments_and_the_marker() {
        let Lookup::Known(printf) = lookup(0, "_printf") else { panic!("no _printf") };
        assert!(printf.variadic);
        assert_eq!(printf.params(), &[T::Pointer, T::VariadicList]);
    }
}
