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
pub mod bsd;
pub mod callback;
pub mod clock;
pub mod data;
pub mod dirent;
pub mod dispatch;
pub mod divergent;
pub mod errno;
pub mod fcntl;
pub mod heap;
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
pub mod rlimit;
pub mod sched;
pub mod select;
pub mod signal;
pub mod spawn;
pub mod stat;
pub mod stdio;
pub mod strict;
pub mod string;
pub mod sysctl;
pub mod sysinfo;
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

fn implemented(name: &str) -> Option<u64> {
    registered(name)
        .or_else(|| stdio::symbol(name))
        .or_else(|| errno::symbol(name))
        .or_else(|| sysctl::symbol(name))
        .or_else(|| sysinfo::symbol(name))
        .or_else(|| pthread::symbol(name))
        .or_else(|| mach::symbol(name))
        .or_else(|| clock::symbol(name))
        .or_else(|| ioctl_symbol(name))
        .or_else(|| sched_symbol(name))
        .or_else(|| random_symbol(name))
        .or_else(|| ulock::symbol(name))
        .or_else(|| spawn::symbol(name))
        .or_else(|| dispatch::symbol(name))
        .or_else(|| xlocale_symbol(name))
        .or_else(|| stat_symbol(name))
        .or_else(|| malloc_symbol(name))
        .or_else(|| vmmap_symbol(name))
        .or_else(|| poll_symbol(name))
        .or_else(|| select_symbol(name))
        .or_else(|| net_symbol(name))
        .or_else(|| xattr_symbol(name))
        .or_else(|| uuid_symbol(name))
        .or_else(|| string_symbol(name))
        .or_else(|| signal_symbol(name))
        .or_else(|| fcntl_symbol(name))
        .or_else(|| bsd_symbol(name))
        .or_else(|| callback_symbol(name))
        .or_else(|| passwd_symbol(name))
        .or_else(|| dirent_symbol(name))
        .or_else(|| rlimit_symbol(name))
        .or_else(|| libproc_symbol(name))
        .or_else(|| printf_l_symbol(name))
        .or_else(|| kqueue_symbol(name))
}

/// The variadic `_l` printers, which only an x86-64 host off Darwin publishes -
/// the trampolines behind them are that ABI's. See [`printf_l`].
fn printf_l_symbol(name: &str) -> Option<u64> {
    #[cfg(all(not(target_os = "macos"), target_arch = "x86_64"))]
    {
        return printf_l::symbol(name);
    }
    #[cfg(not(all(not(target_os = "macos"), target_arch = "x86_64")))]
    {
        let _ = name;
        None
    }
}

/// `kqueue`, which only a host that has no event queue of its own rebuilds out
/// of `poll`.
fn kqueue_symbol(name: &str) -> Option<u64> {
    #[cfg(not(target_os = "macos"))]
    {
        return kqueue::symbol(name);
    }
    #[cfg(target_os = "macos")]
    {
        let _ = name;
        None
    }
}

fn sched_symbol(name: &str) -> Option<u64> {
    #[cfg(not(target_os = "macos"))]
    {
        return sched::symbol(name);
    }
    #[cfg(target_os = "macos")]
    {
        let _ = name;
        None
    }
}

fn random_symbol(name: &str) -> Option<u64> {
    #[cfg(not(target_os = "macos"))]
    {
        return random::symbol(name);
    }
    #[cfg(target_os = "macos")]
    {
        let _ = name;
        None
    }
}

fn ioctl_symbol(name: &str) -> Option<u64> {
    #[cfg(not(target_os = "macos"))]
    {
        return ioctl::symbol(name);
    }
    #[cfg(target_os = "macos")]
    {
        let _ = name;
        None
    }
}

/// `<libproc.h>`, which only a host that has not got one publishes.
fn libproc_symbol(name: &str) -> Option<u64> {
    #[cfg(not(target_os = "macos"))]
    {
        return libproc::symbol(name);
    }
    #[cfg(target_os = "macos")]
    {
        let _ = name;
        None
    }
}

/// The resource limits, which only a host whose `RLIMIT_` numbers and
/// `RLIM_INFINITY` are not Darwin's rewrites.
fn rlimit_symbol(name: &str) -> Option<u64> {
    #[cfg(not(target_os = "macos"))]
    {
        return rlimit::symbol(name);
    }
    #[cfg(target_os = "macos")]
    {
        let _ = name;
        None
    }
}

/// `<dirent.h>`, which only a host whose `struct dirent` is not Darwin's
/// rewrites.
fn dirent_symbol(name: &str) -> Option<u64> {
    #[cfg(not(target_os = "macos"))]
    {
        return dirent::symbol(name);
    }
    #[cfg(target_os = "macos")]
    {
        let _ = name;
        None
    }
}

/// `<pwd.h>`, which only a host whose `struct passwd` is not Darwin's rewrites.
fn passwd_symbol(name: &str) -> Option<u64> {
    #[cfg(not(target_os = "macos"))]
    {
        return passwd::symbol(name);
    }
    #[cfg(target_os = "macos")]
    {
        let _ = name;
        None
    }
}

/// The names a Mac's libc exports and this host's does not, which only a host
/// that is missing them publishes.
/// The host functions whose *callback* is guest code, which on a host that is not
/// Darwin has to be entered with the guest's thread pointer. See [`callback`].
fn callback_symbol(name: &str) -> Option<u64> {
    #[cfg(not(target_os = "macos"))]
    {
        return callback::symbol(name);
    }
    #[cfg(target_os = "macos")]
    {
        let _ = name;
        None
    }
}

fn bsd_symbol(name: &str) -> Option<u64> {
    #[cfg(not(target_os = "macos"))]
    {
        return bsd::symbol(name);
    }
    #[cfg(target_os = "macos")]
    {
        let _ = name;
        None
    }
}

/// `fcntl`, which only a host whose commands and `O_` flags are not Darwin's
/// rewrites.
fn fcntl_symbol(name: &str) -> Option<u64> {
    #[cfg(not(target_os = "macos"))]
    {
        return fcntl::entry::symbol(name);
    }
    #[cfg(target_os = "macos")]
    {
        let _ = name;
        None
    }
}

/// The signal family, which only a host whose signal numbers, `sigset_t`,
/// `struct sigaction` and `stack_t` are not Darwin's publishes.
fn signal_symbol(name: &str) -> Option<u64> {
    #[cfg(not(target_os = "macos"))]
    {
        return signal::entry::symbol(name);
    }
    #[cfg(target_os = "macos")]
    {
        let _ = name;
        None
    }
}

/// The socket API, which only a host whose `sockaddr` is not Darwin's rewrites.
fn net_symbol(name: &str) -> Option<u64> {
    #[cfg(not(target_os = "macos"))]
    {
        return net::symbol(name);
    }
    #[cfg(target_os = "macos")]
    {
        let _ = name;
        None
    }
}

/// `<uuid/uuid.h>`, which only a host that keeps it out of its libc publishes.
fn uuid_symbol(name: &str) -> Option<u64> {
    #[cfg(not(target_os = "macos"))]
    {
        return uuid::symbol(name);
    }
    #[cfg(target_os = "macos")]
    {
        let _ = name;
        None
    }
}

/// Extended attributes, which only a host that namespaces them rewrites.
fn xattr_symbol(name: &str) -> Option<u64> {
    #[cfg(not(target_os = "macos"))]
    {
        return xattr::symbol(name);
    }
    #[cfg(target_os = "macos")]
    {
        let _ = name;
        None
    }
}

/// `select`, which only a host that once refused it on the strength of a
/// declaration publishes; see [`select`].
fn select_symbol(name: &str) -> Option<u64> {
    #[cfg(not(target_os = "macos"))]
    {
        return select::symbol(name);
    }
    #[cfg(target_os = "macos")]
    {
        let _ = name;
        None
    }
}

/// `poll`, which only a host whose `POLL*` bits are not Darwin's rewrites.
fn poll_symbol(name: &str) -> Option<u64> {
    #[cfg(not(target_os = "macos"))]
    {
        return poll::symbol(name);
    }
    #[cfg(target_os = "macos")]
    {
        let _ = name;
        None
    }
}

/// Apple's `<string.h>` extensions, which only a host without them publishes.
fn string_symbol(name: &str) -> Option<u64> {
    #[cfg(not(target_os = "macos"))]
    {
        return string::symbol(name);
    }
    #[cfg(target_os = "macos")]
    {
        let _ = name;
        None
    }
}

/// The memory-mapping family, which only a host whose `MAP_`/`MADV_`/`MS_`
/// values are not Darwin's needs rewritten.
fn vmmap_symbol(name: &str) -> Option<u64> {
    #[cfg(not(target_os = "macos"))]
    {
        return vmmap::symbol(name);
    }
    #[cfg(target_os = "macos")]
    {
        let _ = name;
        None
    }
}

/// The zoned-allocator questions, which only a host with no zones answers.
fn malloc_symbol(name: &str) -> Option<u64> {
    #[cfg(not(target_os = "macos"))]
    {
        return malloc::symbol(name);
    }
    #[cfg(target_os = "macos")]
    {
        let _ = name;
        None
    }
}

/// The `struct stat` family and the path calls beside it, which only a host
/// whose layout is not Darwin's publishes.
fn stat_symbol(name: &str) -> Option<u64> {
    #[cfg(not(target_os = "macos"))]
    {
        return stat::symbol(name);
    }
    #[cfg(target_os = "macos")]
    {
        let _ = name;
        None
    }
}

/// The `xlocale.h` family, which only a host without it publishes.
fn xlocale_symbol(name: &str) -> Option<u64> {
    #[cfg(not(target_os = "macos"))]
    {
        return xlocale::symbol(name);
    }
    #[cfg(target_os = "macos")]
    {
        let _ = name;
        None
    }
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
    for name in SHIMMED_ABOVE.iter().chain(math::PLAIN) {
        // Only where the host has none: on Darwin these are libSystem's own, and
        // shadowing them would replace an implementation with a placeholder in
        // the one case the shim ever stopped intercepting. Under [`strict`] the
        // host answers nothing, so this tree's own entry is the only answer left.
        if (strict::enabled() || host_symbol(name).is_none())
            && let Some(f) = math::symbol(name)
        {
            define_fn(&mut lib, &format!("_{name}"), f);
        }
    }
    stdio::provide(&mut lib);
    errno::provide(&mut lib);
    sysctl::provide(&mut lib);
    sysinfo::provide(&mut lib);
    pthread::provide(&mut lib);
    mach::provide(&mut lib);
    clock::provide(&mut lib);
    #[cfg(not(target_os = "macos"))]
    sched::provide(&mut lib);
    #[cfg(not(target_os = "macos"))]
    random::provide(&mut lib);
    #[cfg(not(target_os = "macos"))]
    ioctl::provide(&mut lib);
    ulock::provide(&mut lib);
    spawn::provide(&mut lib);
    dispatch::provide(&mut lib);
    data::provide(&mut lib);
    #[cfg(not(target_os = "macos"))]
    {
        atomics::provide(&mut lib);
        xlocale::provide(&mut lib);
        stat::provide(&mut lib);
        malloc::provide(&mut lib);
        vmmap::provide(&mut lib);
        string::provide(&mut lib);
        poll::provide(&mut lib);
        select::provide(&mut lib);
        net::provide(&mut lib);
        xattr::provide(&mut lib);
        uuid::provide(&mut lib);
        signal::entry::provide(&mut lib);
        fcntl::entry::provide(&mut lib);
        bsd::provide(&mut lib);
        callback::provide(&mut lib);
        passwd::provide(&mut lib);
        dirent::provide(&mut lib);
        rlimit::provide(&mut lib);
        libproc::provide(&mut lib);
        oslog::provide(&mut lib);
        kqueue::provide(&mut lib);
        #[cfg(target_arch = "x86_64")]
        printf_l::provide(&mut lib);
    }
    lib
}

/// Define `mach_o` as a function, if `f` is non-null.
pub(crate) fn define_fn(lib: &mut machish::resolve::SyntheticDylib, mach_o: &str, f: *const ()) {
    if !f.is_null() {
        lib.define_fn(mach_o, f);
    }
}

/// Define `mach_o` as data at `addr`, if `addr` is non-zero.
pub(crate) fn define_data(lib: &mut machish::resolve::SyntheticDylib, mach_o: &str, addr: u64) {
    if addr != 0 {
        lib.define_data(mach_o, addr);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
