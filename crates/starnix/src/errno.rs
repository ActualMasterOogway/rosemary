//! This thread's `errno`.

use crate::sys;

/// This thread's `errno` slot.
///
/// `libc::__errno_location` exists only on Linux and `libc::__error` only on
/// the BSDs, so a direct call to either is a portability bug waiting for the
/// other host.
#[inline(always)]
pub fn location() -> *mut libc::c_int {
    sys::errno_location()
}

#[inline(always)]
pub fn get() -> i32 {
    // SAFETY: `location` returns a live slot for this thread.
    unsafe { *location() }
}

#[inline(always)]
pub fn set(value: i32) {
    // SAFETY: `location` returns a live slot for this thread.
    unsafe { *location() = value };
}

/// `errno` as it was when this was made, put back when it is dropped.
pub struct Held(i32);

impl Held {
    #[inline(always)]
    pub fn now() -> Held {
        Held(get())
    }
}

impl Drop for Held {
    #[inline(always)]
    fn drop(&mut self) {
        set(self.0);
    }
}
