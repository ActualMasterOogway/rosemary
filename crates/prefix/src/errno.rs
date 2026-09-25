/// This thread's host `errno` slot.
fn location() -> *mut libc::c_int {
    // SAFETY: every spelling returns this thread's own slot, live for as long
    // as the thread is, and none takes an argument.
    #[cfg(not(target_os = "linux"))]
    unsafe {
        libc::__error()
    }
    #[cfg(target_os = "linux")]
    unsafe {
        libc::__errno_location()
    }
}

#[inline(always)]
pub(crate) fn get() -> i32 {
    // SAFETY: `location` returns a live slot for this thread.
    unsafe { *location() }
}

#[inline(always)]
pub(crate) fn set(value: i32) {
    unsafe { *location() = value };
}

/// `errno` as it was when this was made, put back when it is dropped.
pub(crate) struct Held(i32);

impl Held {
    #[inline(always)]
    pub(crate) fn now() -> Held {
        Held(get())
    }
}

impl Drop for Held {
    #[inline(always)]
    fn drop(&mut self) {
        set(self.0);
    }
}
