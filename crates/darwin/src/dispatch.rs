#![cfg_attr(target_os = "macos", allow(dead_code))]

use std::ffi::c_void;
use std::sync::atomic::{AtomicI64, Ordering};

/// The states a `dispatch_once_t` passes through.
///
/// Darwin's is a `long` the caller zero-initializes, so zero has to mean "not
/// yet". The other two values are this implementation's own: nothing reads a
/// `dispatch_once_t` except `dispatch_once`.
const NOT_YET: i64 = 0;
const RUNNING: i64 = 1;
const DONE: i64 = -1;

/// `void dispatch_once_f(dispatch_once_t *, void *context, dispatch_function_t)`.
///
/// # Safety
///
/// `predicate` must be a `long` that started at zero and is used for nothing
/// else, and `function` must be callable with `context`.
unsafe extern "C" fn once_f(
    predicate: *mut c_void,
    context: *mut c_void,
    function: Option<unsafe extern "C" fn(*mut c_void)>,
) {
    let (Some(function), false) = (function, predicate.is_null()) else {
        return;
    };
    // SAFETY: the caller's obligation. `AtomicI64` and `long` have the same
    // size and alignment, and nothing else touches this word.
    let state = unsafe { &*(predicate as *const AtomicI64) };
    if state.load(Ordering::Acquire) == DONE {
        return;
    }
    // Spun rather than parked. A `dispatch_once` body is short by construction -
    // it is what a lazy global is initialized by - and a wait here would need a
    // futex per predicate, which is a table keyed on an address a caller may
    // free. Recursion into the same predicate is a deadlock on Darwin too.
    loop {
        match state.compare_exchange(NOT_YET, RUNNING, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => break,
            Err(DONE) => return,
            Err(_) => std::hint::spin_loop(),
        }
    }
    // SAFETY: the caller's obligation.
    unsafe { function(context) };
    state.store(DONE, Ordering::Release);
}

/// Empty on macOS, where the host's libdispatch is the guest's.
#[cfg(target_os = "macos")]
pub fn symbol(_name: &str) -> Option<u64> {
    None
}

#[cfg(not(target_os = "macos"))]
pub fn symbol(name: &str) -> Option<u64> {
    (name == "dispatch_once_f").then(|| once_f as *const () as usize as u64)
}

#[cfg(target_os = "macos")]
pub fn provide(_lib: &mut machish::resolve::SyntheticDylib) {}

#[cfg(not(target_os = "macos"))]
pub fn provide(lib: &mut machish::resolve::SyntheticDylib) {
    crate::define_fn(lib, "_dispatch_once_f", once_f as *const ());
}

/// Everything [`symbol`] answers.
pub const NAMES: &[&str] = &["dispatch_once_f"];

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    static CALLS: AtomicUsize = AtomicUsize::new(0);

    unsafe extern "C" fn count(_context: *mut c_void) {
        CALLS.fetch_add(1, Ordering::SeqCst);
    }

    #[test]
    fn the_body_runs_once_however_many_threads_ask() {
        let predicate = Box::leak(Box::new(0i64)) as *mut i64 as *mut c_void;
        let address = predicate as usize;
        std::thread::scope(|s| {
            for _ in 0..8 {
                s.spawn(move || {
                    // SAFETY: a zero-initialized `long` this test owns.
                    unsafe { once_f(address as *mut c_void, std::ptr::null_mut(), Some(count)) };
                });
            }
        });
        assert_eq!(CALLS.load(Ordering::SeqCst), 1);
        // And a null predicate is declined rather than dereferenced.
        // SAFETY: the null case returns before any load.
        unsafe { once_f(std::ptr::null_mut(), std::ptr::null_mut(), Some(count)) };
        assert_eq!(CALLS.load(Ordering::SeqCst), 1);
    }
}
