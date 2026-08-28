//! The macOS API surface Rosetta 3 targets, and a host implementation of each entry.

use std::ffi::CString;

pub mod dispatch;
pub mod divergent;
pub mod errno;
pub mod fcntl;
pub mod ioctl;
pub mod locale;
pub mod mach;
pub mod passthrough;
pub mod pthread;
pub mod signal;
pub mod stdio;
pub mod sysctl;
pub mod sysinfo;
pub mod tls;

/// Strip the leading underscore a Mach-O symbol carries.
pub fn c_name(symbol: &str) -> &str {
    symbol.strip_prefix('_').unwrap_or(symbol)
}

/// `dlsym(RTLD_DEFAULT, name)` - this crate's own lookup against the host.
///
/// Not the answer a guest import gets: that goes through [`symbol`], which consults
/// this crate's entries first and refuses a divergent name outright.
pub fn host_symbol(name: &str) -> Option<u64> {
    load_math();
    let c = CString::new(name).ok()?;
    // SAFETY: `RTLD_DEFAULT` is a valid pseudo-handle and `c` outlives the call.
    // `dlsym` returns null for a name it does not know, which is checked.
    let found = unsafe { libc::dlsym(libc::RTLD_DEFAULT, c.as_ptr()) };
    (!found.is_null()).then(|| found as usize as u64)
}

/// Make sure the host's maths library is loaded before anything asks it for a
/// symbol.
///
/// On Darwin `cos` is in libSystem, which every process has. On glibc it is in
/// `libm.so.6`, which is loaded only if something linked against it - and this
/// runtime does not, so `dlsym(RTLD_DEFAULT, "cos")` is null and a guest's whole
/// maths library goes unresolved. One `dlopen` fixes every name at once.
fn load_math() {
    #[cfg(not(target_os = "macos"))]
    {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            // SAFETY: a soname the dynamic loader resolves, opened globally so
            // its exports join the default search and never closed.
            unsafe {
                libc::dlopen(c"libm.so.6".as_ptr(), libc::RTLD_LAZY | libc::RTLD_GLOBAL);
            }
        });
    }
}

/// The address a guest asking for `symbol` should be given, or `None`.
///
/// `None` has two causes that a caller may want to tell apart - the name is
/// [`divergent`], or nothing anywhere defines it - and [`refusal`] says which.
pub fn symbol(symbol: &str) -> Option<u64> {
    let name = c_name(symbol);
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
    let name = c_name(symbol);
    match divergent::reason(name) {
        Some(why) => format!("{symbol}: no Darwin-compatible implementation on this host: {why}"),
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
        .or_else(|| dispatch::symbol(name))
}

pub fn is_implemented(symbol: &str) -> bool {
    implemented(c_name(symbol)).is_some()
}

pub fn provider() -> machish::resolve::SyntheticDylib {
    let mut lib = machish::resolve::SyntheticDylib::new("<darwin>");
    lib.flat(true);
    stdio::provide(&mut lib);
    errno::provide(&mut lib);
    sysctl::provide(&mut lib);
    sysinfo::provide(&mut lib);
    pthread::provide(&mut lib);
    mach::provide(&mut lib);
    dispatch::provide(&mut lib);
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
        assert!(overlap.is_empty(), "claimed safe and also denied: {overlap:?}");
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
}
