//! The allocator the *guest* allocates and frees with.
//!
//! The same registry [`objc::allocator`] holds, for the same reason and in the
//! other direction. It is duplicated rather than shared because `objc` depends
//! on nothing but `libc` on purpose - it knows nothing about Mach-O and should
//! not learn - and the runtime above registers both from one place.
//!
//! ## Why a process has two allocators to begin with
//!
//! A guest image that ships its own allocator replaces `malloc` and `free` for
//! every image the loader bound, through `__DATA,__interpose`. That reaches the
//! guest's own images. It cannot reach this crate: this is host code compiled
//! into the runtime and linked against the host's libc long before the guest
//! existed.
//!
//! So there are two live allocators whenever a guest ships one, and every
//! pointer that crosses between the guest and this crate is a question about
//! which of them owns it:
//!
//! - a block this crate hands the guest to `free` must come from [`malloc`]
//!   here, not from `libc::malloc`;
//! - a block the guest hands this crate to free - `dispatch_data_create` with
//!   `DISPATCH_DATA_DESTRUCTOR_FREE` is the one that turns up - must go back
//!   through [`free`] here, not through `libc::free`.
//!
//! Getting it wrong does not fail at the call. The replacement allocator
//! accepts the foreign pointer, works out which of its own pages "owns" it from
//! the pointer's bits, and pushes it onto that page's free list - which now has
//! a *live* block on it. The crash is thousands of allocations later, on a
//! free-list next pointer that turns out to be ASCII text.

use std::ffi::c_void;
use std::sync::atomic::{AtomicUsize, Ordering};

static MALLOC: AtomicUsize = AtomicUsize::new(0);
static FREE: AtomicUsize = AtomicUsize::new(0);

/// Use `malloc` and `free` for blocks that cross to or from the guest.
///
/// Both must be callable directly from this process, which is true of the
/// native backend - where the guest's code *is* host code - and not of the
/// emulated one, where the guest's allocator is x86-64 that needs the run loop.
/// Nothing registers there, and the host's own is used.
pub fn register(malloc: usize, free: usize) {
    MALLOC.store(malloc, Ordering::Release);
    FREE.store(free, Ordering::Release);
}

/// Whether a guest allocator has been registered.
pub fn is_registered() -> bool {
    MALLOC.load(Ordering::Acquire) != 0 && FREE.load(Ordering::Acquire) != 0
}

/// `size` bytes from the allocator the guest also frees with.
///
/// # Safety
///
/// As `malloc`: the block is uninitialized, and null on failure.
pub unsafe fn malloc(size: usize) -> *mut c_void {
    let f = MALLOC.load(Ordering::Acquire);
    if f == 0 {
        // SAFETY: the host's own, with the caller's size.
        return unsafe { libc::malloc(size) };
    }
    // SAFETY: [`register`]'s contract - an address of `void *(size_t)` this
    // process can call.
    let malloc: unsafe extern "C" fn(usize) -> *mut c_void = unsafe { core::mem::transmute(f) };
    // SAFETY: as above.
    unsafe { malloc(size) }
}

/// Free a block the guest allocated, or one [`malloc`] returned.
///
/// # Safety
///
/// `block` came from the guest's allocator - which is what the guest promised
/// when it said `DISPATCH_DATA_DESTRUCTOR_FREE` - and is not freed twice.
pub unsafe fn free(block: *mut c_void) {
    let f = FREE.load(Ordering::Acquire);
    if f == 0 {
        // SAFETY: the caller's obligation. With nothing registered there is one
        // allocator and it is the host's.
        return unsafe { libc::free(block) };
    }
    // SAFETY: [`register`]'s contract - an address of `void(void *)` this
    // process can call.
    let free: unsafe extern "C" fn(*mut c_void) = unsafe { core::mem::transmute(f) };
    // SAFETY: as above.
    unsafe { free(block) }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Most programs have one allocator and never reach [`register`], so the
    /// unregistered path is the common one and has to work.
    #[test]
    fn without_a_guest_allocator_the_hosts_own_is_used() {
        // SAFETY: a size this test asks for and immediately frees.
        unsafe {
            let block = malloc(48);
            assert!(!block.is_null());
            block.cast::<u8>().write(7);
            free(block);
        }
    }

    #[test]
    fn a_registered_allocator_is_the_one_that_is_called() {
        use std::sync::atomic::AtomicU32;

        static FREED: AtomicU32 = AtomicU32::new(0);

        unsafe extern "C" fn counting_free(block: *mut c_void) {
            FREED.fetch_add(1, Ordering::Relaxed);
            // SAFETY: a block the host's own `malloc` returned.
            unsafe { libc::free(block) };
        }

        let (was_malloc, was_free) = (
            MALLOC.load(Ordering::Acquire),
            FREE.load(Ordering::Acquire),
        );
        register(libc::malloc as *const () as usize, counting_free as *const () as usize);
        assert!(is_registered());
        // SAFETY: a size this test asks for and immediately frees.
        unsafe { free(malloc(16)) };
        assert_eq!(FREED.load(Ordering::Relaxed), 1);
        register(was_malloc, was_free);
    }
}
