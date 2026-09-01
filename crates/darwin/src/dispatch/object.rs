//! What every `dispatch_object_t` has in common: a refcount, a context, and a
//! finalizer.
//!
//! A guest holds these as opaque pointers, so a handle is the host address of the
//! allocation and nothing has to be mapped or translated. What does have to be right
//! is the lifetime: `dispatch_release` on the last reference runs the finalizer the
//! guest installed, and a queue released while work is still pending must not free
//! itself out from under the worker running it.

use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

/// Which object an allocation is, so a handle can be turned back into its type.
///
/// Checked rather than trusted: a guest that passes a semaphore to a queue call
/// would otherwise have its bytes reinterpreted as a queue.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u32)]
pub enum Kind {
    Queue,
    Group,
    Semaphore,
    Data,
}

/// The header every dispatch object begins with.
///
/// `#[repr(C)]` and first in each object, so a `*mut Header` recovered from a guest's
/// handle reads the right words whichever object it names.
#[repr(C)]
pub struct Header {
    /// Where Darwin keeps the Objective-C `isa`, since its dispatch objects are
    /// also Objective-C objects. Left null: nothing in this implementation reads
    /// it, and a guest that sends one of these a message gets a null-`isa` crash
    /// naming the line rather than a silent misread of the fields below.
    isa: u64,
    kind: Kind,
    refs: AtomicUsize,
    context: AtomicU64,
    finalizer: AtomicU64,
}

impl Header {
    pub fn new(kind: Kind) -> Header {
        Header {
            isa: 0,
            kind,
            refs: AtomicUsize::new(1),
            context: AtomicU64::new(0),
            finalizer: AtomicU64::new(0),
        }
    }

    pub fn kind(&self) -> Kind {
        self.kind
    }
}

/// A `dispatch_function_t`: what a finalizer and the `_f` entry points take.
pub type Function = unsafe extern "C" fn(*mut c_void);

/// The header a guest's handle names, or `None` if it names nothing usable.
///
/// # Safety
///
/// `handle` is null or an object this module allocated.
pub unsafe fn header<'a>(handle: u64) -> Option<&'a Header> {
    match handle {
        0 => None,
        // SAFETY: the caller's guarantee; every object begins with a `Header`.
        _ => Some(unsafe { &*(handle as *const Header) }),
    }
}

/// The header a guest's handle names, if it is of `kind`.
///
/// # Safety
///
/// As [`header`].
pub unsafe fn typed<'a>(handle: u64, kind: Kind) -> Option<&'a Header> {
    // SAFETY: the caller's guarantee.
    unsafe { header(handle) }.filter(|h| h.kind == kind)
}

/// Take one more reference. Returns the handle, as `dispatch_retain` does not.
pub fn retain(handle: u64) {
    // SAFETY: a live object, by the caller of the entry point above this.
    if let Some(h) = unsafe { header(handle) } {
        h.refs.fetch_add(1, Ordering::Relaxed);
    }
}

/// Drop one reference, running the finalizer and freeing at zero.
///
/// # Safety
///
/// `handle` is null or a live object of this module's, and the caller is giving up a
/// reference it holds.
pub unsafe fn release(handle: u64) {
    // SAFETY: the caller's guarantee.
    let Some(h) = (unsafe { header(handle) }) else {
        return;
    };
    if h.refs.fetch_sub(1, Ordering::AcqRel) != 1 {
        return;
    }
    // The finalizer runs before the memory goes, and is the guest's own function -
    // so it must not run with any lock of this module held.
    let (finalizer, context) = (
        h.finalizer.load(Ordering::Acquire),
        h.context.load(Ordering::Acquire),
    );
    if finalizer != 0 {
        // SAFETY: a `dispatch_function_t` the guest installed, called with the
        // context it installed alongside.
        unsafe {
            std::mem::transmute::<u64, Function>(finalizer)(context as *mut c_void);
        }
    }
    let kind = h.kind;
    // SAFETY: the refcount reached zero, so this reference is the only one and the
    // allocation is this module's own. Each arm names the type it was boxed as.
    unsafe {
        match kind {
            Kind::Queue => drop(Box::from_raw(handle as *mut super::queue::Queue)),
            Kind::Group => drop(Box::from_raw(handle as *mut super::sync::Group)),
            Kind::Semaphore => drop(Box::from_raw(handle as *mut super::sync::Semaphore)),
            Kind::Data => drop(Box::from_raw(handle as *mut super::data::Data)),
        }
    }
}

/// `void dispatch_retain(dispatch_object_t)`.
pub unsafe extern "C" fn retain_entry(handle: u64) {
    retain(handle);
}

/// `void dispatch_release(dispatch_object_t)`.
pub unsafe extern "C" fn release_entry(handle: u64) {
    // SAFETY: the guest is giving up a reference it holds.
    unsafe { release(handle) };
}

/// `void dispatch_set_context(dispatch_object_t, void *)`.
pub unsafe extern "C" fn set_context(handle: u64, context: *mut c_void) {
    // SAFETY: a live object.
    if let Some(h) = unsafe { header(handle) } {
        h.context.store(context as u64, Ordering::Release);
    }
}

/// `void *dispatch_get_context(dispatch_object_t)`.
pub unsafe extern "C" fn get_context(handle: u64) -> *mut c_void {
    // SAFETY: a live object.
    match unsafe { header(handle) } {
        Some(h) => h.context.load(Ordering::Acquire) as *mut c_void,
        None => std::ptr::null_mut(),
    }
}

/// `void dispatch_set_finalizer_f(dispatch_object_t, dispatch_function_t)`.
pub unsafe extern "C" fn set_finalizer_f(handle: u64, finalizer: u64) {
    // SAFETY: a live object.
    if let Some(h) = unsafe { header(handle) } {
        h.finalizer.store(finalizer, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU32;

    static FINALIZED: AtomicU32 = AtomicU32::new(0);

    unsafe extern "C" fn finalize(context: *mut c_void) {
        FINALIZED.store(context as u32, Ordering::Release);
    }

    /// The finalizer runs exactly once, at the last release, with the guest's own
    /// context - which is what a guest frees its own state in.
    #[test]
    fn the_last_release_finalizes() {
        FINALIZED.store(0, Ordering::Release);
        let queue = super::super::queue::create_serial("finalizer test");
        // SAFETY: a live object of this module's.
        unsafe {
            set_context(queue, 77 as *mut c_void);
            set_finalizer_f(queue, finalize as *const () as usize as u64);
            retain(queue);
            release(queue);
            assert_eq!(FINALIZED.load(Ordering::Acquire), 0, "still referenced");
            release(queue);
        }
        assert_eq!(FINALIZED.load(Ordering::Acquire), 77);
    }

    /// A handle of the wrong kind must be refused rather than reinterpreted.
    #[test]
    fn a_handle_carries_its_kind() {
        let queue = super::super::queue::create_serial("kind test");
        // SAFETY: a live object.
        unsafe {
            assert!(typed(queue, Kind::Queue).is_some());
            assert!(typed(queue, Kind::Semaphore).is_none());
            assert!(typed(0, Kind::Queue).is_none());
            release(queue);
        }
    }
}
