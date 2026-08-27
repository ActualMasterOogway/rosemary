use machish::resolve::SyntheticDylib;

/// This thread's `errno` slot.
///
/// The one accessor the rest of this project should use. `libc::__error` exists only
/// on Darwin and `libc::__errno_location` only on Linux, so a direct call to either
/// is a portability bug waiting for the other host.
pub fn location() -> *mut libc::c_int {
    // SAFETY: both spellings return this thread's own slot, live for as long as the
    // thread is, and neither takes an argument.
    #[cfg(target_os = "macos")]
    unsafe {
        libc::__error()
    }
    #[cfg(not(target_os = "macos"))]
    unsafe {
        libc::__errno_location()
    }
}

/// Read this thread's `errno`.
pub fn get() -> i32 {
    // SAFETY: `location` returns a live slot for this thread.
    unsafe { *location() }
}

/// Write this thread's `errno`.
pub fn set(value: i32) {
    // SAFETY: as above.
    unsafe { *location() = value };
}

/// `int *__error(void)`, as the guest imports it.
///
/// A real function rather than a rebound host symbol, so the guest gets the same
/// answer whichever spelling this host uses.
unsafe extern "C" fn error() -> *mut libc::c_int {
    location()
}

/// The guest's `errno`, put back exactly as it was.
///
/// A host library that returns successfully is still entitled to leave `errno` wherever it likes, and a guest
/// that checks it after a call this runtime made on its behalf sees the runtime's failure rather than its
/// own.
pub struct Held(i32);

impl Held {
    pub fn now() -> Held {
        Held(get())
    }
}

impl Drop for Held {
    fn drop(&mut self) {
        set(self.0);
    }
}

pub fn symbol(name: &str) -> Option<u64> {
    (name == "_error").then(|| error as *const () as usize as u64)
}

pub fn provide(lib: &mut SyntheticDylib) {
    // Two underscores as the Mach-O spells it: one from C mangling, one from the
    // identifier `__error`.
    crate::define_fn(lib, "___error", error as *const ());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_slot_is_this_thread_s_own() {
        set(libc::EPIPE);
        let mine = location() as usize;
        let theirs = std::thread::spawn(|| {
            set(libc::ENOENT);
            location() as usize
        })
        .join()
        .expect("the thread reads its own errno");
        assert_ne!(mine, theirs, "two threads must not share one slot");
        assert_eq!(get(), libc::EPIPE, "and this one is unchanged");
        set(0);
    }

    #[test]
    fn holding_errno_puts_it_back() {
        set(libc::EPIPE);
        {
            let _held = Held::now();
            set(libc::ENOENT);
        }
        assert_eq!(get(), libc::EPIPE);
        set(0);
    }

    /// The imported spelling and the accessor must reach the same slot, which is the
    /// only thing that makes the guest's `errno` and a shim's `errno` the same
    /// variable.
    #[test]
    fn the_imported_error_is_the_accessor() {
        let at = symbol("_error").expect("___error is defined");
        // SAFETY: `at` is this module's own `error`, whose prototype is `int *(void)`.
        let f: unsafe extern "C" fn() -> *mut libc::c_int =
            unsafe { std::mem::transmute(at as usize as *const ()) };
        // SAFETY: as above.
        assert_eq!(unsafe { f() }, location());
    }
}
