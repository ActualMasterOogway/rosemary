//! The frameworks this tree implements, and whether they are the ones in use.
//!
//! Each is selected the same way [`crate::objc::host`] selects a runtime, and
//! for the same reason: a framework written here can then be exercised on macOS,
//! where its answers can be compared against Apple's, instead of only on the
//! host that has no alternative.
//!
//! The selection is not free-standing. Every CoreFoundation object is an object
//! of *this tree's* Objective-C runtime, so a builtin CoreFoundation over the
//! host's libobjc would hand Apple's runtime structures it would read as its
//! own. The frameworks therefore follow `ROSETTA3_OBJC` rather than carrying a
//! switch each.

use machish::resolve::{NativeDylibProvider, SyntheticDylib};

use crate::objc::host::{Backend, backend};

/// Whether this process is using the frameworks in this tree.
pub fn builtin() -> bool {
    backend() == Backend::Builtin
}

/// Tell `darwin` where to find these, so that a `dlsym` and the loader agree.
///
/// Called from everything below rather than from start-up: `darwin::symbol` is
/// asked during linking, which happens before `startup::run`, and a registration
/// that lands after the first question is a registration that did not happen.
fn install() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| darwin::register_provider(symbol));
}

/// The frameworks, as a library the loader can bind against.
///
/// Flat, like the rest of this runtime's providers: which framework an image
/// thinks `CFRelease` comes from is not the question.
pub fn provider() -> SyntheticDylib {
    install();
    let mut lib = SyntheticDylib::new("<rosetta3 frameworks>");
    lib.flat(true);
    if !builtin() {
        return lib;
    }
    for name in corefoundation::NAMES {
        if let Some(addr) = corefoundation::symbol(name) {
            lib.define_fn(&format!("_{name}"), addr as usize as *const ());
        }
    }
    for (name, addr) in corefoundation::data_symbols() {
        lib.define_data(&name, addr);
    }
    lib
}

/// The address `name` has in one of them, or `None`.
///
/// `name` is the C identifier, without the leading underscore. Registered with
/// [`darwin::register_provider`] so that a `dlsym` finds the same answer the
/// loader bound - an inconsistency there is a guest whose import works and whose
/// `dlsym` of the same name comes back null.
pub fn symbol(name: &str) -> Option<u64> {
    builtin().then(|| corefoundation::symbol(name)).flatten()
}

/// Keep the host's own framework symbols away from a guest using these.
///
/// The same argument as [`crate::objc::host::deny_foreign_classes`]: an object
/// from Apple's CoreFoundation handed to this one is a foreign structure read as
/// a native one. `darwin::divergent` already denies the `CF` and `NS` families
/// off macOS; this is what denies them *on* macOS when the builtin frameworks
/// are the ones in use.
pub fn deny_host_frameworks(provider: &mut NativeDylibProvider) {
    install();
    if !builtin() {
        return;
    }
    for prefix in ["_CF", "_NS", "_kCF", "_kNS"] {
        provider.deny_prefix(prefix);
    }
}
