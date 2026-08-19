//! Symbols this runtime resolves for **itself**, against the host process.
//!
//! Distinct from `shims::host`, which answers the same question on a guest
//! import's behalf and refuses by name when it cannot.

use std::ffi::{CString, c_void};

/// `dlsym(RTLD_DEFAULT, name)`, or `None` for a name nothing loaded exports.
pub fn symbol_ptr(name: &str) -> Option<*mut c_void> {
    let c = CString::new(name).ok()?;
    // SAFETY: `RTLD_DEFAULT` is a valid pseudo-handle and `c` outlives the call.
    // `dlsym` returns null for a name it does not know, which is checked.
    let found = unsafe { libc::dlsym(libc::RTLD_DEFAULT, c.as_ptr()) };
    (!found.is_null()).then_some(found)
}

/// [`symbol_ptr`] as an address, which is the form a guest is handed.
pub fn symbol(name: &str) -> Option<u64> {
    symbol_ptr(name).map(|p| p as usize as u64)
}
