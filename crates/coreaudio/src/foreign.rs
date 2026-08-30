//! Handles this crate did not make.
//!
//! There is no `AUGraph` here, so a program that builds one gets the host's -
//! and its nodes are the host's `AudioUnit`s, reached through the same entry
//! point names this crate answers. A handle that is not in this crate's table
//! goes back to the framework that made it. Refusing instead is what leaves a
//! game whose whole mixer graph is Apple's with no sound and no diagnostic.
//!
//! Darwin only, and only while [`forwarding`] is on. Everywhere else this crate
//! is the only AudioToolbox there is: there is no `AUGraph` to produce a foreign
//! handle, so nothing here is consulted.

use std::ffi::{CStr, c_void};

/// Whether a foreign handle is handed back to the framework that made it.
///
/// `ROSETTA3_NO_FOREIGN` turns it off, which is how a run proves it needs no
/// host framework at all: a unit that is not this crate's is then refused with
/// the framework's own `kAudioUnitErr_InvalidElement` rather than serviced by
/// Apple's. `corefoundation::foreign` reads the same variable, for the same
/// reason and with the same default.
#[cfg(target_os = "macos")]
fn forwarding() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("ROSETTA3_NO_FOREIGN").is_none())
}

/// The framework's own entry point, by `dlsym` rather than a link.
///
/// This crate's own names are published to a guest through a synthetic library
/// rather than exported from the binary, so the lookup cannot find its way back
/// here.
#[cfg(target_os = "macos")]
fn entry(name: &CStr) -> Option<*mut c_void> {
    if !forwarding() {
        return None;
    }
    // SAFETY: a NUL-terminated name, and the default search order.
    let found = unsafe { libc::dlsym(libc::RTLD_DEFAULT, name.as_ptr()) };
    (!found.is_null()).then_some(found)
}

/// Off Darwin there is no other AudioToolbox, so every forward below stops here.
#[cfg(not(target_os = "macos"))]
fn entry(_name: &CStr) -> Option<*mut c_void> {
    None
}

/// Forward a one-argument `OSStatus` call, or `None` where there is nothing to
/// forward to.
pub fn status(name: &CStr, handle: *mut c_void) -> Option<i32> {
    let f = entry(name)?;
    // SAFETY: every name passed here is `OSStatus (*)(AudioUnit)`, and the
    // handle is one that framework made.
    Some(unsafe {
        let f: unsafe extern "C" fn(*mut c_void) -> i32 = std::mem::transmute(f);
        f(handle)
    })
}

/// Forward `AudioUnitSetProperty`.
///
/// # Safety
///
/// As the entry point's: `data` is `size` readable bytes for the property.
pub unsafe fn set_property(
    handle: *mut c_void,
    id: u32,
    scope: u32,
    element: u32,
    data: *const c_void,
    size: u32,
) -> Option<i32> {
    let f = entry(c"AudioUnitSetProperty")?;
    // SAFETY: the declaration is the framework's, and the handle is one it made.
    Some(unsafe {
        let f: unsafe extern "C" fn(*mut c_void, u32, u32, u32, *const c_void, u32) -> i32 =
            std::mem::transmute(f);
        f(handle, id, scope, element, data, size)
    })
}

/// Forward `AudioUnitGetProperty`.
///
/// # Safety
///
/// As the entry point's: `out` is writable for `*size` bytes.
pub unsafe fn get_property(
    handle: *mut c_void,
    id: u32,
    scope: u32,
    element: u32,
    out: *mut c_void,
    size: *mut u32,
) -> Option<i32> {
    let f = entry(c"AudioUnitGetProperty")?;
    // SAFETY: the declaration is the framework's, and the handle is one it made.
    Some(unsafe {
        let f: unsafe extern "C" fn(*mut c_void, u32, u32, u32, *mut c_void, *mut u32) -> i32 =
            std::mem::transmute(f);
        f(handle, id, scope, element, out, size)
    })
}
