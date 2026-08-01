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
//! the useful one. `_sigaction` is not missing from this table by accident: it
//! installs a guest function pointer as a signal handler, and the handler would
//! be entered with a *host* `ucontext_t` the guest cannot read. Recording that
//! as [`Lookup::Refused`] with a reason turns a crash into a sentence, and keeps
//! it out of the "someone should write this signature" list where it would sit
//! forever.
//!
//! A refusal is also the honest record of a milestone boundary, and it moves
//! when the milestone does: `qsort`, `qsort_r`, `bsearch` and `atexit` were
//! refused for taking a guest function pointer, and are now ordinary entries
//! whose pointer argument is marked in [`FunctionAbi::callbacks`] and
//! substituted with a thunk. Nothing about their signatures changed.
//!
//! [`REFUSED`] therefore holds three kinds of thing:
//!
//! - **Data**, which is imported through a slot and read, never called.
//! - **Callback takers whose callback the thunk cannot yet carry**: a signal
//!   handler, which would be entered with a host `ucontext_t` (M5).
//!   `pthread_create` was here until M4 gave a new host thread its own guest
//!   stack and register context; nothing about its signature changed either.
//! - **Stack walkers**, where passthrough is not merely unimplemented but
//!   *wrong*: the host's unwinder cannot walk guest frames, so `pthread_cancel`
//!   is refused (M5). `setjmp` and `longjmp` were here for the same reason and
//!   are now ordinary entries with a shim behind them — the signature says what
//!   the guest passes, and [`crate::jmpbuf`] does the part the host cannot.
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
    /// Zero-based parameter indices that are **guest function pointers**.
    ///
    /// A pointer is a pointer to the marshaller; this is the one place the
    /// distinction matters, because handing the host an x86-64 address it will
    /// call as arm64 is the failure mode host→guest thunks exist to prevent.
    /// Each listed argument is substituted with a thunk on its way out — see
    /// [`crate::thunk`] — which is also why a signature carrying one can never
    /// be a *fast* import: the lifter would inline the call and pass the raw
    /// pointer.
    pub callbacks: &'static [u8],
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
        Lookup::Unknown => {
            if let Some(reason) = data_family(name) {
                return Lookup::Refused(reason);
            }
            match name.split_once('$') {
                // `_fts_open$INODE64` and friends: the variant only ever existed
                // on x86, and its signature is the plain symbol's.
                Some((base, _)) if !base.is_empty() => lookup_exact(base),
                _ => Lookup::Unknown,
            }
        }
        found => found,
    }
}

/// Data imports whose names carry a suffix, so [`REFUSED`] cannot list them.
///
/// Objective-C metadata is the only family in this shape: a class reference is
/// `_OBJC_CLASS_$_NSString`, one name per class the guest mentions, and an image
/// may mention hundreds. They are read through their slots and never called —
/// `_OBJC_CLASS_$_NSObject` is the *superclass field* of a guest class, which
/// [`crate::objc`] follows at registration — so refusing them by family says
/// exactly what listing them one by one would, and stays true for a class nobody
/// has heard of.
///
/// The alternative is that a real Objective-C program reports a dozen imports as
/// [`Lookup::Unknown`], which is the audit's word for "nobody has described
/// this" and would drown the one that really is undescribed.
fn data_family(name: &str) -> Option<&'static str> {
    const DATA: &str = "an Objective-C metadata import, read through its slot and never called";
    let families = [
        "OBJC_CLASS_$_",
        "OBJC_METACLASS_$_",
        "OBJC_IVAR_$_",
        // The type object a `@catch` clause matches against, which is data for
        // the same reason a C++ `type_info` is.
        "OBJC_EHTYPE_",
        "OBJC_PROTOCOL_$_",
        "OBJC_LABEL_PROTOCOL_$_",
        // The empty cache and vtable every `objc_class` the compiler emits
        // points at, and the `isa` of a constant `@"..."` string.
        "_objc_empty_cache",
        "_objc_empty_vtable",
        "__CFConstantStringClassReference",
    ];
    families.iter().any(|f| name.starts_with(f)).then_some(DATA)
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
        FunctionAbi { name: $name, types: &[$ret $(, $arg)*], variadic: false, callbacks: &[] }
    };
}

/// As [`sig`], for a C-variadic function. The fixed arguments are listed; the
/// ellipsis is implied by the flag.
macro_rules! vsig {
    ($name:literal, $ret:expr $(, $arg:expr)* $(,)?) => {
        FunctionAbi {
            name: $name,
            types: &[$ret $(, $arg)* , T::VariadicList],
            variadic: true,
            callbacks: &[],
        }
    };
}

/// As [`sig`], for a function taking guest function pointers. `$cb` lists the
/// zero-based parameter indices that are callbacks.
macro_rules! cbsig {
    ([$($cb:literal),* $(,)?], $name:literal, $ret:expr $(, $arg:expr)* $(,)?) => {
        FunctionAbi {
            name: $name,
            types: &[$ret $(, $arg)*],
            variadic: false,
            callbacks: &[$($cb),*],
        }
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
    // The C++ exception surface, all of it shimmed: see [`crate::unwind`] for
    // why none of it can reach the host's libc++abi. `_ZSt9terminatev` is the
    // exception to the exception — `std::terminate` really does mean "stop this
    // process", and the host's does that correctly.
    sig!("_Unwind_Resume", V, P),
    // What a guest class deriving from `std::exception` reaches: the base's
    // constructor and destructor, and `operator delete`. Ordinary host calls —
    // the object is guest memory the host writes a vtable pointer into, which
    // works for the same reason a `pthread_mutex_t` does.
    sig!("_ZNKSt9exception4whatEv", P, P),
    sig!("_ZNSt9exceptionC1Ev", V, P),
    sig!("_ZNSt9exceptionC2Ev", V, P),
    sig!("_ZNSt9exceptionD0Ev", V, P),
    sig!("_ZNSt9exceptionD1Ev", V, P),
    sig!("_ZNSt9exceptionD2Ev", V, P),
    sig!("_ZSt9terminatev", V),
    sig!("_ZdaPv", V, P),
    sig!("_ZdlPv", V, P),
    sig!("_Znam", P, U64),
    sig!("_Znwm", P, U64),
    sig!("__assert_rtn", V, P, P, I32, P),
    sig!("__bzero", V, P, U64),
    sig!("__cxa_allocate_exception", P, U64),
    // Shimmed, and listed for the reasons `sigaction` is: the entry keeps it out
    // of the audit's unknown list and off the fast path. The handler is a guest
    // function pointer, but no `cbsig!` marks it — [`crate::shims::atexit`]
    // substitutes it itself, because it must also replace the DSO handle.
    sig!("__cxa_atexit", I32, P, P, P),
    sig!("__cxa_begin_catch", P, P),
    sig!("__cxa_end_catch", V),
    sig!("__cxa_free_exception", V, P),
    sig!("__cxa_rethrow", V),
    sig!("__cxa_throw", V, P, P, P),
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
    // The Darwin spellings that skip the signal mask, imported as `__longjmp`
    // and `__setjmp`. One implementation serves both pairs: the distinction is
    // about a mask this runtime does not model yet. See [`crate::jmpbuf`].
    sig!("_longjmp", V, P, I32),
    sig!("_setjmp", I32, P),
    // Both spellings of the thread-local bootstrap. It is an ordinary host
    // function taking the descriptor's address and returning this thread's
    // address for the variable — see [`crate::tls`] — and it is reached through
    // an *indirect* call (`callq *(%rdi)`), not a stub, so the signature is what
    // lets the runtime service it at all.
    sig!("_tlv_bootstrap", P, P),
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
    // `atexit`, `bsearch`, `qsort` and `qsort_r` were refused until the
    // host->guest thunk existed. What changed is only that the pointer they
    // take can now be substituted; the signature was never in doubt.
    cbsig!([0], "atexit", I32, P),
    sig!("atof", F64, P),
    sig!("atoi", I32, P),
    sig!("atol", I64, P),
    sig!("atoll", I64, P),
    sig!("bcmp", I32, P, P, U64),
    cbsig!([4], "bsearch", P, P, P, U64, U64, P),
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
    // The comparator takes `const FTSENT **`, which is still just a pointer:
    // `ls` sorts its output with one, so this is the entry that makes the
    // milestone-1 acceptance binary produce output at all.
    cbsig!([2], "fts_open", P, P, I32, P),
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
    // `longjmp` and `setjmp` are *shimmed*, not forwarded: the host's would save
    // and restore arm64 registers and the JIT's own stack frame, which is the
    // one kind of wrongness that does not fault. The signature is here anyway
    // because it is what stops them being promoted to an inlined fast import and
    // what names them at an exit — see [`crate::jmpbuf`].
    sig!("longjmp", V, P, I32),
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
    // ## The pthread family
    //
    // Passthrough is correct here for a reason worth stating rather than
    // assuming, because it is the whole basis of M4: every opaque pthread type
    // is **byte-identical** on x86_64 and arm64 macOS — `pthread_mutex_t` is 64
    // bytes on both, `pthread_cond_t` 48, `pthread_rwlock_t` 200,
    // `pthread_attr_t` 64, `pthread_t` a pointer. The guest allocates one on its
    // own stack and the host's libpthread operates on it in place, with no
    // translation at all. (`pthread_types_agree_across_architectures` checks
    // this rather than trusting it.)
    //
    // What is *not* passthrough is the start routine, which is guest code and
    // goes out as a thunk like any other callback — the difference being that
    // the host calls it on a brand-new host thread, which is why M4 needed
    // per-thread guest state before this entry could exist. See
    // [`crate::thread`].
    sig!("pthread_attr_destroy", I32, P),
    sig!("pthread_attr_getstacksize", I32, P, P),
    sig!("pthread_attr_init", I32, P),
    sig!("pthread_attr_setdetachstate", I32, P, I32),
    sig!("pthread_attr_setstacksize", I32, P, U64),
    sig!("pthread_cond_broadcast", I32, P),
    sig!("pthread_cond_destroy", I32, P),
    sig!("pthread_cond_init", I32, P, P),
    sig!("pthread_cond_signal", I32, P),
    sig!("pthread_cond_timedwait", I32, P, P, P),
    sig!("pthread_cond_wait", I32, P, P),
    cbsig!([2], "pthread_create", I32, P, P, P, P),
    sig!("pthread_detach", I32, U64),
    sig!("pthread_equal", I32, U64, U64),
    // Terminates the calling host thread from inside our thunk: the Rust frames
    // between here and the JIT entry are abandoned rather than unwound. That is
    // what Darwin's own `pthread_exit` does to any C frame, and it is safe here
    // only because the runtime never holds a lock across guest code. Thread-local
    // destructors still run, so this thread's guest stack and TLS blocks are
    // still released.
    sig!("pthread_exit", V, P),
    sig!("pthread_getname_np", I32, U64, P, U64),
    sig!("pthread_getspecific", P, U64),
    sig!("pthread_join", I32, U64, P),
    // The destructor is guest code, called by libpthread on thread teardown.
    cbsig!([1], "pthread_key_create", I32, P, P),
    sig!("pthread_key_delete", I32, U64),
    sig!("pthread_mutex_destroy", I32, P),
    sig!("pthread_mutex_init", I32, P, P),
    sig!("pthread_mutex_lock", I32, P),
    sig!("pthread_mutex_trylock", I32, P),
    sig!("pthread_mutex_unlock", I32, P),
    sig!("pthread_mutexattr_destroy", I32, P),
    sig!("pthread_mutexattr_init", I32, P),
    sig!("pthread_mutexattr_settype", I32, P, I32),
    // The initialiser is guest code, and `pthread_once` guarantees it runs
    // exactly once — a guarantee that survives thunking, since the thunk is
    // stable per guest address.
    cbsig!([1], "pthread_once", I32, P, P),
    sig!("pthread_rwlock_destroy", I32, P),
    sig!("pthread_rwlock_init", I32, P, P),
    sig!("pthread_rwlock_rdlock", I32, P),
    sig!("pthread_rwlock_unlock", I32, P),
    sig!("pthread_rwlock_wrlock", I32, P),
    sig!("pthread_self", U64),
    sig!("pthread_setname_np", I32, P),
    sig!("pthread_setspecific", I32, U64, P),
    sig!("putc", I32, I32, P),
    sig!("putchar", I32, I32),
    sig!("puts", I32, P),
    sig!("putwchar", I32, I32),
    cbsig!([3], "qsort", V, P, U64, U64, P),
    cbsig!([4], "qsort_r", V, P, U64, U64, P, P),
    // `raise` and `kill` deliver to this process, so a guest handler installed
    // through [`crate::signal`] is reached exactly the way the kernel reaches
    // any other: asynchronously, on whichever thread the kernel picks.
    sig!("raise", I32, I32),
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
    sig!("setjmp", I32, P),
    sig!("setlocale", P, I32, P),
    sig!("setvbuf", I32, P, P, I32, U64),
    // Shimmed, like the `setjmp` family and for a related reason: the handler
    // is a guest function pointer that crosses out as a thunk and comes *back*
    // in `signal`'s result and `sigaction`'s `oldact`. See [`crate::signal`].
    sig!("sigaction", I32, I32, P, P),
    sig!("siglongjmp", V, P, I32),
    sig!("signal", P, I32, P),
    sig!("sigsetjmp", I32, P, I32),
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
    // The other spelling; see `_tlv_bootstrap` above.
    sig!("tlv_bootstrap", P, P),
    sig!("tolower", I32, I32),
    sig!("toupper", I32, I32),
    // The output function is called once per character: `tputs` is how a
    // terminal-aware program emits a capability string, and `/bin/ls` reaches
    // it whenever its output is a terminal.
    cbsig!([2], "tputs", I32, P, I32, P),
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
    (
        "_DefaultRuneLocale",
        "a data import, read through its slot and never called",
    ),
    // A `type_info` and the vtable a guest's own `type_info` objects point at.
    // Both are read through their slots and compared by address, which is what
    // makes a `catch` clause decidable at all — see [`crate::unwind`].
    (
        "_ZTISt9exception",
        "a data import, read through its slot and never called",
    ),
    (
        "_ZTIi",
        "a data import, read through its slot and never called",
    ),
    (
        "_ZTVN10__cxxabiv117__class_type_infoE",
        "a data import, read through its slot",
    ),
    (
        "_ZTVN10__cxxabiv120__si_class_type_infoE",
        "a data import, read through its slot",
    ),
    (
        "_ZTVN10__cxxabiv121__vmi_class_type_infoE",
        "a data import, read through its slot",
    ),
    (
        "_ZTVSt9exception",
        "a data import, read through its slot and never called",
    ),
    // The personality routine a frame's LSDA names. Read as an address and
    // never called: this runtime interprets the LSDA itself rather than handing
    // it to libunwind, which is the whole of [`crate::unwind`].
    (
        "__gxx_personality_v0",
        "a personality routine, named by an LSDA and never called",
    ),
    (
        "__mb_cur_max",
        "a data import, read through its slot and never called",
    ),
    (
        "__objc_personality_v0",
        "a personality routine, named by an LSDA and never called",
    ),
    (
        "__stack_chk_guard",
        "a data import, read through its slot and never called",
    ),
    (
        "__stderrp",
        "a data import, read through its slot and never called",
    ),
    (
        "__stdinp",
        "a data import, read through its slot and never called",
    ),
    (
        "__stdoutp",
        "a data import, read through its slot and never called",
    ),
    (
        "dyld_stub_binder",
        "the lazy-binding entry point; every slot is bound eagerly",
    ),
    (
        "environ",
        "a data import, read through its slot and never called",
    ),
    ("humanize_number", UNDECLARED),
    ("mbr_identifier_translate", UNDECLARED),
    (
        "optarg",
        "a data import, read through its slot and never called",
    ),
    (
        "opterr",
        "a data import, read through its slot and never called",
    ),
    (
        "optind",
        "a data import, read through its slot and never called",
    ),
    (
        "optopt",
        "a data import, read through its slot and never called",
    ),
    (
        "pthread_cancel",
        "cancellation unwinds a stack the host's unwinder never saw (M5)",
    ),
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
            assert!(
                pair[0].0 < pair[1].0,
                "{} and {} are out of order",
                pair[0].0,
                pair[1].0
            );
        }
    }

    #[test]
    fn a_symbol_is_never_both_known_and_refused() {
        for abi in SIGNATURES {
            assert!(
                REFUSED
                    .binary_search_by_key(&abi.name, |(n, _)| *n)
                    .is_err(),
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
        // `fts_open` is a signature now that the thunk exists, and the point
        // of this assertion is unchanged: the `$INODE64` spelling finds
        // whatever the plain name has to say.
        assert!(
            matches!(lookup(0, "_fts_open$INODE64"), Lookup::Known(abi) if abi.name == "fts_open")
        );
        assert_eq!(lookup(0, "_strtod$UNIX2003"), lookup(0, "_strtod"));
    }

    #[test]
    fn objc_metadata_is_refused_as_data_rather_than_reported_unknown() {
        // One name per class an image mentions, so this cannot be a list. The
        // distinction that matters is refused-versus-unknown: unknown is the
        // audit's word for a symbol nobody has described, and these are
        // described exactly — they are never called.
        for name in [
            "_OBJC_CLASS_$_NSObject",
            "_OBJC_METACLASS_$_NSObject",
            "_OBJC_IVAR_$_Dog._weight",
            "_OBJC_EHTYPE_id",
            "__objc_empty_cache",
            "___CFConstantStringClassReference",
        ] {
            assert!(matches!(lookup(0, name), Lookup::Refused(_)), "{name}");
        }
        // Not a blanket "anything starting with OBJC": the entry points are
        // functions with signatures, and swallowing one here would hide it.
        assert!(matches!(lookup(0, "_objc_msgSend"), Lookup::Unknown));
    }

    #[test]
    fn a_variadic_carries_its_fixed_arguments_and_the_marker() {
        let Lookup::Known(printf) = lookup(0, "_printf") else {
            panic!("no _printf")
        };
        assert!(printf.variadic);
        assert_eq!(printf.params(), &[T::Pointer, T::VariadicList]);
    }
}
