//! The allocator the *guest* frees with.
//!
//! A handful of runtime entry points hand the guest a block and tell it to call
//! `free` on it - `class_copyMethodList` and the rest of the `copy...List`
//! family are the whole of it here. That block cannot come from this process's
//! own `malloc`, and the reason is not the obvious one.
//!
//! A guest image that ships its own allocator - mimalloc, jemalloc, a game
//! engine's own - replaces `malloc` and `free` for every image the loader
//! bound, through `__DATA,__interpose`. That reaches the guest's images, and it
//! *cannot* reach this one: the frameworks in this tree are host code compiled
//! into the runtime, linked against the host's libc long before the guest
//! existed. So `libc::malloc` here and `free` there are two different
//! allocators, and a block that crosses between them is freed by an allocator
//! that never made it.
//!
//! What that looks like when it goes wrong is worth writing down, because it
//! does not look like this: mimalloc accepts the foreign pointer, works out
//! which of its pages "owns" it from the pointer's own bits, and pushes it onto
//! that page's free list. The list now has a *live* block on it. Thousands of
//! allocations later some unrelated thread is handed that block, and the crash
//! is a fault on a free-list next pointer that turns out to be ASCII text.
//!
//! Registered by the runtime after linking, once it knows which `malloc` the
//! guest's own images resolved to. Until then - and in any process where the
//! guest has no allocator of its own - this is the host's, which is the right
//! answer when there is only one allocator.

use std::ffi::c_void;
use std::sync::atomic::{AtomicUsize, Ordering};

static MALLOC: AtomicUsize = AtomicUsize::new(0);
static FREE: AtomicUsize = AtomicUsize::new(0);

/// Use `malloc` and `free` for blocks handed to the guest.
///
/// Both must be callable directly from this process. That is true of the
/// native backend, where the guest's code *is* host code; it is not true under
/// emulation, where the guest's allocator is x86-64 the runtime cannot enter
/// without the run loop - so nothing registers there and the host's own is
/// used, which is what the emulated backend's guests already get for every
/// other host call.
pub fn register(malloc: usize, free: usize) {
    MALLOC.store(malloc, Ordering::Release);
    FREE.store(free, Ordering::Release);
}

/// Whether a guest allocator has been registered.
pub fn is_registered() -> bool {
    MALLOC.load(Ordering::Acquire) != 0 && FREE.load(Ordering::Acquire) != 0
}

/// `size` bytes the guest may `free`.
///
/// # Safety
///
/// As `malloc`: the block is uninitialized, and null on failure.
pub unsafe fn guest_malloc(size: usize) -> *mut c_void {
    let f = MALLOC.load(Ordering::Acquire);
    if f == 0 {
        // SAFETY: the host's own, with the caller's size.
        return unsafe { libc::malloc(size) };
    }
    // SAFETY: `register`'s contract - an address of `void *(size_t)` this
    // process can call.
    let malloc: unsafe extern "C" fn(usize) -> *mut c_void = unsafe { core::mem::transmute(f) };
    // SAFETY: as above.
    unsafe { malloc(size) }
}

/// Free a block [`guest_malloc`] returned.
///
/// # Safety
///
/// `block` came from [`guest_malloc`] and is not freed twice.
pub unsafe fn guest_free(block: *mut c_void) {
    let f = FREE.load(Ordering::Acquire);
    if f == 0 {
        // SAFETY: the caller's obligation, and the block came from the host's
        // own `malloc` because that is what answered when nothing was
        // registered.
        return unsafe { libc::free(block) };
    }
    // SAFETY: `register`'s contract - an address of `void(void *)` this process
    // can call.
    let free: unsafe extern "C" fn(*mut c_void) = unsafe { core::mem::transmute(f) };
    // SAFETY: as above.
    unsafe { free(block) }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Nothing registered is the host's own allocator, which has to keep
    /// working: most programs have one allocator and never reach `register`.
    #[test]
    fn without_a_guest_allocator_the_hosts_own_is_used_and_round_trips() {
        assert!(!is_registered() || MALLOC.load(Ordering::Acquire) != 0);
        // SAFETY: a size this test asks for and immediately frees.
        unsafe {
            let block = guest_malloc(64);
            assert!(!block.is_null());
            block.cast::<u8>().write(0xAB);
            guest_free(block);
        }
    }

    /// A registered pair is the one that gets called, in both directions - the
    /// bug this module exists for is a block allocated by one and freed by the
    /// other.
    #[test]
    fn a_registered_allocator_is_the_one_that_is_called() {
        use std::sync::atomic::AtomicU32;

        static ALLOCATED: AtomicU32 = AtomicU32::new(0);
        static FREED: AtomicU32 = AtomicU32::new(0);

        unsafe extern "C" fn counting_malloc(size: usize) -> *mut c_void {
            ALLOCATED.fetch_add(1, Ordering::Relaxed);
            // SAFETY: the host's own, with the caller's size.
            unsafe { libc::malloc(size) }
        }
        unsafe extern "C" fn counting_free(block: *mut c_void) {
            FREED.fetch_add(1, Ordering::Relaxed);
            // SAFETY: a block `counting_malloc` returned.
            unsafe { libc::free(block) };
        }

        let (was_malloc, was_free) = (
            MALLOC.load(Ordering::Acquire),
            FREE.load(Ordering::Acquire),
        );
        register(counting_malloc as *const () as usize, counting_free as *const () as usize);
        assert!(is_registered());
        // SAFETY: a size this test asks for and immediately frees.
        unsafe { guest_free(guest_malloc(32)) };
        assert_eq!(ALLOCATED.load(Ordering::Relaxed), 1);
        assert_eq!(FREED.load(Ordering::Relaxed), 1);
        register(was_malloc, was_free);
    }
}
