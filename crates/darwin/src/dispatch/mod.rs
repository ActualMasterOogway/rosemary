//! libdispatch: queues, groups, semaphores, and the objects they are made of.
//!
//! Darwin programs use libdispatch the way other programs use threads directly, so
//! this is not an optional corner of the platform - a guest that cannot
//! `dispatch_async` cannot start. There is no libdispatch on this host to forward to,
//! so the parts a guest actually reaches are implemented here:
//!
//! * [`queue`] - the queues and the pool of threads that drains them, including the
//!   main queue, which only the main thread may run.
//! * [`sync`] - `dispatch_group_t` and `dispatch_semaphore_t`.
//! * [`object`] - the refcount, context and finalizer every dispatch object has.
//! * [`time`] - `dispatch_time_t`, on the same clock as `mach_absolute_time`.
//! * [`block`] - invoking a block, and the *registered* copy and release. That
//!   registration is the one piece this crate does not own: a block must be copied
//!   and released by a single runtime, and the process's lives above this crate.
//!
//! What a guest hands in is guest code, and reaching it needs thunks this crate has
//! no vocabulary for. That is already handled before a call arrives:
//! `rosetta3::shims::block` rewrites a block's code pointers to native thunks and
//! then calls the entry points here, so everything below deals in ordinary host
//! function pointers.
//!
//! On macOS none of this is bound to a guest import - the host's libdispatch is the
//! guest's - but all of it is compiled and tested there.

#![cfg_attr(target_os = "macos", allow(dead_code))]

pub mod block;
pub mod data;
pub mod object;
pub mod queue;
pub mod sync;
pub mod time;

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
    let Some(function) = function else {
        return;
    };
    // SAFETY: the caller's obligation for the predicate, and `function` is
    // callable with `context` by the same.
    unsafe { once_with(predicate, || function(context)) };
}

/// `void dispatch_once(dispatch_once_t *, dispatch_block_t)`.
///
/// The block is not copied: it has run before this returns, so it cannot outlive
/// the caller's frame the way an asynchronous one would.
///
/// # Safety
///
/// As [`once_f`], and `block` is a block whose `invoke` is callable here.
unsafe extern "C" fn once(predicate: *mut c_void, block: u64) {
    if block == 0 {
        return;
    }
    // SAFETY: the caller's obligation.
    unsafe { once_with(predicate, || block::invoke(block)) };
}

/// Run `body` the first time this predicate is used, and never again.
///
/// # Safety
///
/// `predicate` must be a `long` that started at zero and is used for nothing
/// else, and `body` must be callable from this thread.
unsafe fn once_with(predicate: *mut c_void, body: impl FnOnce()) {
    if predicate.is_null() {
        return;
    }
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
    body();
    state.store(DONE, Ordering::Release);
}

/// Empty on macOS, where the host's libdispatch is the guest's.
#[cfg(target_os = "macos")]
pub fn symbol(_name: &str) -> Option<u64> {
    None
}

#[cfg(not(target_os = "macos"))]
pub fn symbol(name: &str) -> Option<u64> {
    macro_rules! at {
        ($f:expr) => {
            $f as *const () as usize as u64
        };
    }
    let address = match name {
        // Queues.
        "dispatch_queue_create" => at!(queue::queue_create),
        "dispatch_queue_create_with_target" => at!(queue::queue_create_with_target),
        "dispatch_queue_get_label" => at!(queue::queue_get_label),
        "dispatch_get_global_queue" => at!(queue::get_global_queue),

        // Submission.
        "dispatch_async" => at!(queue::async_),
        "dispatch_async_f" => at!(queue::async_f),
        "dispatch_sync" => at!(queue::sync),
        "dispatch_sync_f" => at!(queue::sync_f),
        "dispatch_after" => at!(queue::after),
        "dispatch_after_f" => at!(queue::after_f),
        "dispatch_once" => at!(once),
        "dispatch_once_f" => at!(once_f),

        // Groups.
        "dispatch_group_create" => at!(sync::group_create),
        "dispatch_group_enter" => at!(sync::group_enter),
        "dispatch_group_leave" => at!(sync::group_leave),
        "dispatch_group_notify" => at!(sync::group_notify),
        "dispatch_group_notify_f" => at!(sync::group_notify_f),
        "dispatch_group_async_f" => at!(sync::group_async_f),
        "dispatch_group_wait" => at!(sync::group_wait),

        // Semaphores.
        "dispatch_semaphore_create" => at!(sync::semaphore_create),
        "dispatch_semaphore_signal" => at!(sync::semaphore_signal),
        "dispatch_semaphore_wait" => at!(sync::semaphore_wait),

        // Time.
        "dispatch_time" => at!(time::time),

        // Data.
        "dispatch_data_create" => at!(data::create),
        "dispatch_data_get_size" => at!(data::get_size),

        // The object protocol every one of the above shares.
        "dispatch_retain" => at!(object::retain_entry),
        "dispatch_release" => at!(object::release_entry),
        "dispatch_set_context" => at!(object::set_context),
        "dispatch_get_context" => at!(object::get_context),
        "dispatch_set_finalizer_f" => at!(object::set_finalizer_f),

        _ => return None,
    };
    Some(address)
}

#[cfg(target_os = "macos")]
pub fn provide(_lib: &mut machish::resolve::SyntheticDylib) {}

/// Define every entry, and the three *data* symbols a guest takes the address of.
///
/// `dispatch_get_main_queue()` is an inline function in Darwin's SDK, so a guest
/// never calls anything to reach the main queue - it references `_dispatch_main_q`
/// directly, and the symbol has to resolve to a real object.
#[cfg(not(target_os = "macos"))]
pub fn provide(lib: &mut machish::resolve::SyntheticDylib) {
    // This runs on the main thread while the guest is being linked, which is what
    // makes it the place to record which thread that is.
    queue::note_main_thread();
    for name in NAMES {
        crate::define_fn(lib, &format!("_{name}"), symbol(name).unwrap() as *const ());
    }
    crate::define_data(lib, "__dispatch_main_q", queue::main_queue());
    crate::define_data(lib, "__dispatch_queue_attr_concurrent", queue::concurrent_attr());
    crate::define_data(lib, "__dispatch_data_destructor_free", data::destructor_free());
}

/// Everything [`symbol`] answers.
pub const NAMES: &[&str] = &[
    "dispatch_queue_create",
    "dispatch_queue_create_with_target",
    "dispatch_queue_get_label",
    "dispatch_get_global_queue",
    "dispatch_async",
    "dispatch_async_f",
    "dispatch_sync",
    "dispatch_sync_f",
    "dispatch_after",
    "dispatch_after_f",
    "dispatch_once",
    "dispatch_once_f",
    "dispatch_group_create",
    "dispatch_group_enter",
    "dispatch_group_leave",
    "dispatch_group_notify",
    "dispatch_group_notify_f",
    "dispatch_group_async_f",
    "dispatch_group_wait",
    "dispatch_semaphore_create",
    "dispatch_semaphore_signal",
    "dispatch_semaphore_wait",
    "dispatch_time",
    "dispatch_data_create",
    "dispatch_data_get_size",
    "dispatch_retain",
    "dispatch_release",
    "dispatch_set_context",
    "dispatch_get_context",
    "dispatch_set_finalizer_f",
];


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
