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

        let (was_malloc, was_free) = (MALLOC.load(Ordering::Acquire), FREE.load(Ordering::Acquire));
        register(
            counting_malloc as *const () as usize,
            counting_free as *const () as usize,
        );
        assert!(is_registered());
        // SAFETY: a size this test asks for and immediately frees.
        unsafe { guest_free(guest_malloc(32)) };
        assert_eq!(ALLOCATED.load(Ordering::Relaxed), 1);
        assert_eq!(FREED.load(Ordering::Relaxed), 1);
        register(was_malloc, was_free);
    }
}
