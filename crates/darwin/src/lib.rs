#![allow(
    clippy::assertions_on_constants,
    clippy::bool_assert_comparison,
    clippy::explicit_counter_loop,
    clippy::identity_op,
    clippy::box_default,
    clippy::chunks_exact_to_as_chunks,
    clippy::doc_lazy_continuation,
    clippy::len_without_is_empty,
    clippy::manual_contains,
    clippy::manual_slice_size_calculation,
    clippy::missing_const_for_thread_local,
    clippy::missing_safety_doc,
    clippy::needless_return,
    clippy::not_unsafe_ptr_arg_deref,
    clippy::question_mark,
    clippy::type_complexity,
    clippy::unnecessary_cast,
    clippy::unnecessary_lazy_evaluations,
    clippy::useless_vec
)]

//! The macOS API surface Rosetta 3 targets, and a host implementation of each entry.

use std::ffi::CString;

mod atomics;
pub mod backtrace;
pub mod bsd;
pub mod callback;
pub mod clock;
pub mod commoncrypto;
#[cfg(not(target_os = "macos"))]
pub mod compat;
pub mod data;
pub mod dirent;
pub mod dispatch;
pub mod divergent;
pub mod errno;
pub mod fcntl;
#[cfg(not(target_os = "macos"))]
pub mod fortify;
#[cfg(not(target_os = "macos"))]
pub mod glob;
pub mod heap;
pub mod idle;
pub mod ioctl;
pub mod kqueue;
pub mod libproc;
pub mod locale;
pub mod mach;
pub mod malloc;
pub mod math;
pub mod net;
mod oslog;
pub mod passthrough;
pub mod passwd;
pub mod path;
pub mod poll;
pub mod printf_l;
pub mod pthread;
pub mod random;
pub mod regex;
mod resolv;
pub mod rlimit;
pub mod sched;
pub mod select;
pub mod shm;
pub mod signal;
pub mod spawn;
pub mod stackprobe;
pub mod stat;
pub mod stdio;
pub mod strict;
pub mod string;
pub mod sysctl;
pub mod sysinfo;
#[cfg(not(target_os = "macos"))]
pub mod termios;
pub mod tls;
pub mod translation;
pub mod ulock;
pub mod uuid;
pub mod vmmap;
pub mod xattr;
pub mod xlocale;

/// Strip the leading underscore a Mach-O symbol carries.
pub fn c_name(symbol: &str) -> &str {
    symbol.strip_prefix('_').unwrap_or(symbol)
}

/// [`c_name`], with a `$` variant suffix folded away as well.
///
/// `_close$NOCANCEL`, `_fopen$DARWIN_EXTSN`, `_fts_open$INODE64`: the suffix is a
/// promise about the *base* symbol's behaviour, not a different function, so the
/// base name is the one to ask every list about.
///
/// `machish::resolve::SyntheticDylib` folds the same suffix for the definitions
/// this crate publishes. This is the other half: a suffixed name that would
/// otherwise be handed to `dlsym` verbatim, which no host library exports, and
/// which no entry of [`divergent`] would match either - so a divergent function
/// spelled with a suffix would be passed straight through.
pub fn base_name(symbol: &str) -> &str {
    let name = c_name(symbol);
    match name.split_once('$') {
        Some((base, variant)) if !base.is_empty() && !variant.is_empty() => base,
        _ => name,
    }
}

/// `dlsym(RTLD_DEFAULT, name)` - this crate's own lookup against the host.
///
/// Not the answer a guest import gets: that goes through [`symbol`], which consults
/// this crate's entries first and refuses a divergent name outright.
pub fn host_symbol(name: &str) -> Option<u64> {
    load_host_libraries();
    let c = CString::new(name).ok()?;
    // SAFETY: `RTLD_DEFAULT` is a valid pseudo-handle and `c` outlives the call.
    // `dlsym` returns null for a name it does not know, which is checked.
    let found = unsafe { libc::dlsym(libc::RTLD_DEFAULT, c.as_ptr()) };
    (!found.is_null()).then(|| found as usize as u64)
}

/// Make sure the libraries a guest's imports come out of are loaded before
/// anything asks for a symbol.
///
/// On Darwin `cos` is in libSystem, which the loader has already. Off it, it
/// lives in a library that is loaded only if something linked against it - and
/// this runtime links neither - so `dlsym(RTLD_DEFAULT, "cos")` is null and a
/// guest's whole maths library goes unresolved. One `dlopen` fixes every name
/// at once.
///
/// GLU is a separate library off Darwin - `gluOrtho2D` is in the OpenGL
/// framework there and in `libGLU.so.1` here - so it is a second `dlopen` and
/// not an oversight.
///
/// GL itself is deliberately **not** opened here. Its entry points have to come
/// out of the library SDL made the context against, which is not necessarily
/// this loader's `libGL.so.1`, and a global one would answer every `gl` name
/// first. See `rosetta3::opengl`.
fn load_host_libraries() {
    #[cfg(not(target_os = "macos"))]
    {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            for soname in [c"libm.so.6", c"libGLU.so.1"] {
                // SAFETY: a soname the dynamic loader resolves, opened globally
                // so its exports join the default search, and never closed.
                unsafe {
                    libc::dlopen(soname.as_ptr(), libc::RTLD_LAZY | libc::RTLD_GLOBAL);
                }
            }
        });
    }
}

/// The address a guest asking for `symbol` should be given, or `None`.
///
/// `None` has two causes that a caller may want to tell apart - the name is
/// [`divergent`], or nothing anywhere defines it - and [`refusal`] says which.
pub fn symbol(symbol: &str) -> Option<u64> {
    let name = base_name(symbol);
    if let Some(addr) = implemented(name) {
        return Some(addr);
    }
    if divergent::contains(name) || strict::enabled() {
        return None;
    }
    host_symbol(name)
}

/// [`symbol`] without the [`strict`] refusal, for a shim naming what it forwards to.
///
/// A shim that calls the host's `printf` is the explicit implementation the strict
/// mode asks for, not the fallback it withdraws - so it keeps its answer.
pub fn shim_symbol(symbol: &str) -> Option<u64> {
    let name = base_name(symbol);
    if let Some(addr) = implemented(name) {
        return Some(addr);
    }
    if divergent::contains(name) {
        return None;
    }
    host_symbol(name)
}

/// Why [`symbol`] declined `symbol`, as a sentence.
pub fn refusal(symbol: &str) -> String {
    let name = base_name(symbol);
    match divergent::reason(name) {
        Some(why) => format!("{symbol}: no Darwin-compatible implementation on this host: {why}"),
        None if strict::enabled() && host_symbol(name).is_some() => format!(
            "{symbol}: nothing in this tree implements it, and {} withdraws the host's own",
            strict::VAR
        ),
        None => format!("{symbol}: nothing on this host defines it"),
    }
}

/// A source of answers registered by the crate above this one.
///
/// `darwin` cannot depend on `corefoundation` - the dependency runs the other
/// way - but the two have to agree: a name the loader binds to this tree's
/// `CFStringGetLength` must also be the one a `dlsym` finds, or a guest gets an
/// import that works and a `dlsym` of the same name that comes back null. The
/// runtime registers itself here at startup and this crate asks.
static REGISTERED: std::sync::RwLock<Vec<fn(&str) -> Option<u64>>> =
    std::sync::RwLock::new(Vec::new());

/// Add a source of answers, consulted by [`symbol`] alongside this crate's own.
///
/// Idempotent only by the caller's discipline: registering twice costs a second
/// lookup and answers the same.
pub fn register_provider(source: fn(&str) -> Option<u64>) {
    REGISTERED
        .write()
        .unwrap_or_else(|e| e.into_inner())
        .push(source);
}

fn registered(name: &str) -> Option<u64> {
    REGISTERED
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .find_map(|source| source(name))
}

/// Every module this crate publishes, once.
///
/// `provider` and `implemented` are both this list. They used to be two, in two
/// hand-kept orders, and a module in one and not the other is a name the loader
/// binds and a `dlsym` answers null for - which is what had happened to
/// `atomics` and `oslog`.
///
/// The order is the publishing order. Nothing here may define a name another
/// entry defines - see `no_module_defines_a_name_another_one_does`, which is
/// what makes the order not matter.
static MODULES: &[exports::Module] = &[
    idle::MODULE,
    stdio::MODULE,
    errno::MODULE,
    sysctl::MODULE,
    sysinfo::MODULE,
    PTHREAD,
    mach::MODULE,
    clock::MODULE,
    commoncrypto::MODULE,
    #[cfg(not(target_os = "macos"))]
    compat::MODULE,
    #[cfg(not(target_os = "macos"))]
    fortify::MODULE,
    #[cfg(not(target_os = "macos"))]
    glob::MODULE,
    #[cfg(not(target_os = "macos"))]
    regex::MODULE,
    #[cfg(not(target_os = "macos"))]
    termios::MODULE,
    #[cfg(not(target_os = "macos"))]
    sched::MODULE,
    #[cfg(not(target_os = "macos"))]
    random::MODULE,
    #[cfg(not(target_os = "macos"))]
    ioctl::MODULE,
    ulock::MODULE,
    spawn::MODULE,
    dispatch::MODULE,
    data::MODULE,
    #[cfg(not(target_os = "macos"))]
    atomics::MODULE,
    #[cfg(not(target_os = "macos"))]
    xlocale::MODULE,
    #[cfg(not(target_os = "macos"))]
    stat::MODULE,
    #[cfg(not(target_os = "macos"))]
    malloc::MODULE,
    #[cfg(not(target_os = "macos"))]
    vmmap::MODULE,
    #[cfg(not(target_os = "macos"))]
    string::MODULE,
    #[cfg(not(target_os = "macos"))]
    poll::MODULE,
    #[cfg(not(target_os = "macos"))]
    select::MODULE,
    #[cfg(not(target_os = "macos"))]
    net::MODULE,
    #[cfg(not(target_os = "macos"))]
    xattr::MODULE,
    #[cfg(not(target_os = "macos"))]
    uuid::MODULE,
    #[cfg(not(target_os = "macos"))]
    signal::entry::MODULE,
    #[cfg(not(target_os = "macos"))]
    fcntl::entry::MODULE,
    #[cfg(not(target_os = "macos"))]
    bsd::MODULE,
    #[cfg(not(target_os = "macos"))]
    callback::MODULE,
    #[cfg(not(target_os = "macos"))]
    passwd::MODULE,
    #[cfg(not(target_os = "macos"))]
    dirent::MODULE,
    #[cfg(not(target_os = "macos"))]
    rlimit::MODULE,
    #[cfg(not(target_os = "macos"))]
    shm::MODULE,
    #[cfg(not(target_os = "macos"))]
    libproc::MODULE,
    #[cfg(not(target_os = "macos"))]
    oslog::MODULE,
    #[cfg(not(target_os = "macos"))]
    kqueue::MODULE,
    #[cfg(not(target_os = "macos"))]
    backtrace::MODULE,
    #[cfg(not(target_os = "macos"))]
    stackprobe::MODULE,
    #[cfg(all(not(target_os = "macos"), target_arch = "x86_64"))]
    printf_l::MODULE,
    #[cfg(all(not(target_os = "macos"), target_arch = "x86_64"))]
    resolv::MODULE,
];

/// `pthread` as a row, written out because it has no table to generate one.
///
/// Its `provide` binds most names to the *host's* address, looked up at run
/// time, so there is no list of names to derive - see `pthread::provide`.
const PTHREAD: exports::Module = exports::Module {
    path: "darwin::pthread",
    names: pthread_names,
    symbol: pthread::symbol,
    provide: pthread::provide,
};

fn pthread_names() -> exports::Names {
    exports::Names::new()
}

fn implemented(name: &str) -> Option<u64> {
    registered(name)
        .or_else(|| exports::Module::resolve(MODULES, name))
        // Not a row: it probes the host and consults `strict`, so which answer
        // it gives depends on more than the name. See `math_symbol`.
        .or_else(|| math_symbol(name))
}

/// The part of [`math`] this crate answers everywhere, which is not the whole
/// of it: the four Darwin-ABI returns in [`SHIMMED_ABOVE`] are intercepted by
/// the runtime above and must stay libSystem's on Darwin.
///
/// The same two conditions [`provider`] publishes under, so that a `dlsym` of
/// one of these names and an import of it answer the same address.
fn math_symbol(name: &str) -> Option<u64> {
    let ours = || math::symbol(name).map(|f| f as usize as u64);
    if math::SHADOWED.contains(&name) {
        return ours();
    }
    let unclaimed = strict::enabled() || host_symbol(name).is_none();
    (math::PLAIN.contains(&name) && unclaimed)
        .then(ours)
        .flatten()
}

pub fn is_implemented(symbol: &str) -> bool {
    implemented(base_name(symbol)).is_some()
}

/// Names the **runtime above** shims and no host exports.
///
/// A shim is only reached once its import resolves: `abi::call_import_inner`
/// refuses a null address *before* it looks for one, so a name nothing defines
/// fails as an unsatisfied import and never reaches the code that implements it.
/// On Darwin these are libSystem's and resolve on their own; off it, nothing
/// defines them and [`crate::math`] is what makes the slot non-null.
///
/// Each is a Darwin ABI of its own rather than a missing function: `__sincos_stret`
/// returns two doubles in the vector registers, which no glibc entry point does.
/// The runtime above intercepts all four for an emulated guest and never reaches
/// the published address; a native guest calls it, so it is a real implementation
/// rather than a placeholder.
pub const SHIMMED_ABOVE: &[&str] = &["__sincosf_stret", "__sincos_stret", "__divdc3", "__mulsc3"];

pub fn provider() -> machish::resolve::SyntheticDylib {
    let mut lib = machish::resolve::SyntheticDylib::new("<darwin>");
    lib.flat(true);
    // Every definition goes through the null guard: a slot holding zero is
    // worse than an unbound import, which is at least refused by name at the
    // call rather than jumping to address zero.
    let out = &mut exports::NonNull(&mut lib);
    for name in SHIMMED_ABOVE.iter().chain(math::PLAIN) {
        // Only where the host has none: on Darwin these are libSystem's own, and
        // shadowing them would replace an implementation with a placeholder in
        // the one case the shim ever stopped intercepting. Under [`strict`] the
        // host answers nothing, so this tree's own entry is the only answer left.
        if (strict::enabled() || host_symbol(name).is_none())
            && let Some(f) = math::symbol(name)
        {
            define_fn(out, &format!("_{name}"), f);
        }
    }
    // These the host does define, and differently - see [`math::SHADOWED`].
    for name in math::SHADOWED {
        if let Some(f) = math::symbol(name) {
            define_fn(out, &format!("_{name}"), f);
        }
    }
    exports::Module::provide_all(MODULES, out);
    lib
}

/// Define `mach_o` as a function, for the modules not yet on the table.
pub(crate) fn define_fn(out: &mut dyn exports::Exports, mach_o: &str, f: *const ()) {
    out.function(mach_o, f as usize as u64);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The publishing order is the lookup order, and neither may decide an
    /// answer. Two modules defining one name would make it decide both: the
    /// library is a map, so the later definition wins the link, while
    /// `implemented` stops at the earlier one and a `dlsym` gets that.
    #[test]
    fn no_module_defines_a_name_another_one_does() {
        let mut seen: Vec<(&str, &str)> = Vec::new();
        for module in MODULES {
            for name in (module.names)() {
                if let Some((_, first)) = seen.iter().find(|(n, _)| *n == name) {
                    panic!("{name} is defined by both {first} and {}", module.path);
                }
                seen.push((name, module.path));
            }
        }
    }

    /// Every name a module publishes is one a `dlsym` answers - the invariant
    /// the two lists could not state when they were two, and which `atomics` and
    /// `oslog` had been breaking.
    ///
    /// Through `symbol` rather than `implemented`, in the Mach-O spelling a
    /// guest actually asks with, so the underscore this crate strips is part of
    /// what is checked.
    #[test]
    fn a_published_name_is_the_one_dlsym_answers() {
        for module in MODULES {
            for name in (module.names)() {
                let mach_o = format!("_{name}");
                assert!(
                    symbol(&mach_o).is_some(),
                    "{}: {mach_o} is published and unanswered",
                    module.path
                );
            }
        }
    }

    /// A name cannot be both claimed safe to pass through and denied as divergent -
    /// the two lists are the same question answered opposite ways, and a name in both
    /// means one of them is wrong.
    #[test]
    fn nothing_is_both_claimed_safe_and_divergent() {
        let overlap: Vec<&str> = passthrough::CLAIMED
            .iter()
            .copied()
            .filter(|n| divergent::contains(n))
            .collect();
        assert!(
            overlap.is_empty(),
            "claimed safe and also denied: {overlap:?}"
        );
    }

    /// Nor can a name this crate implements be claimed as a passthrough: the whole
    /// reason it has an implementation is that the host's own is not the answer.
    #[test]
    fn nothing_implemented_is_also_claimed_safe() {
        let overlap: Vec<&str> = passthrough::CLAIMED
            .iter()
            .copied()
            .filter(|n| implemented(n).is_some())
            .collect();
        assert!(
            overlap.is_empty(),
            "claimed safe and also implemented here: {overlap:?}"
        );
    }

    #[test]
    fn the_mach_o_underscore_is_not_part_of_the_name() {
        assert_eq!(c_name("_sysctl"), "sysctl");
        assert_eq!(c_name("__error"), "_error");
        assert_eq!(c_name("sysctl"), "sysctl");
    }

    /// A `$` suffix names the same function, so every list is asked about the
    /// base - including [`divergent`], which would otherwise pass a suffixed
    /// spelling of a denied name straight through to the host.
    #[test]
    fn a_variant_suffix_is_not_part_of_the_name() {
        assert_eq!(base_name("_close$NOCANCEL"), "close");
        assert_eq!(base_name("_fopen$DARWIN_EXTSN"), "fopen");
        assert_eq!(base_name("_getdirentries$INODE64"), "getdirentries");
        assert!(divergent::contains(base_name("_getdirentries$INODE64")));
        // Neither half may be empty, or `$` alone would name everything.
        assert_eq!(base_name("_$NOCANCEL"), "$NOCANCEL");
        assert_eq!(base_name("_close$"), "close$");
    }
}
