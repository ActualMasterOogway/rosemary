//! The macOS API surface Rosetta 3 targets, and a host implementation of each entry.

use std::ffi::CString;

pub mod divergent;
pub mod errno;
pub mod fcntl;
pub mod ioctl;
pub mod locale;
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
    let c = CString::new(name).ok()?;
    // SAFETY: `RTLD_DEFAULT` is a valid pseudo-handle and `c` outlives the call.
    // `dlsym` returns null for a name it does not know, which is checked.
    let found = unsafe { libc::dlsym(libc::RTLD_DEFAULT, c.as_ptr()) };
    (!found.is_null()).then(|| found as usize as u64)
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

fn implemented(name: &str) -> Option<u64> {
    stdio::symbol(name)
        .or_else(|| errno::symbol(name))
        .or_else(|| sysctl::symbol(name))
        .or_else(|| sysinfo::symbol(name))
        .or_else(|| pthread::symbol(name))
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
