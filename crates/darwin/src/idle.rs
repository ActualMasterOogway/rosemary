use std::sync::atomic::{AtomicUsize, Ordering};

/// The registered hook, as an address. Zero until something registers one.
static HOOK: AtomicUsize = AtomicUsize::new(0);

/// Have `f` run whenever a guest thread sleeps or yields.
///
/// One hook, replaced rather than chained: there is one caller, and a list
/// would be a lock on a path whose whole point is being cheap.
pub fn on_idle(f: fn()) {
    HOOK.store(f as usize, Ordering::Release);
}

/// Run the hook, if one is registered. The first thing each shim below does.
fn reached() {
    let f = HOOK.load(Ordering::Acquire);
    if f != 0 {
        // SAFETY: `on_idle` stores exactly a `fn()` and nothing else writes it.
        let f: fn() = unsafe { std::mem::transmute(f) };
        f();
    }
}

// Each forwards to the host's own after the hook: the sleeping is identical on
// both platforms, and the wrapper exists only for the line above it. `timespec`
// is two 64-bit fields on both, so `nanosleep`'s arguments cross untouched.

unsafe extern "C" fn usleep(micros: u32) -> i32 {
    reached();
    // SAFETY: an integer count of microseconds.
    unsafe { libc::usleep(micros) }
}

unsafe extern "C" fn sleep(seconds: u32) -> u32 {
    reached();
    // SAFETY: an integer count of seconds.
    unsafe { libc::sleep(seconds) }
}

unsafe extern "C" fn nanosleep(requested: *const libc::timespec, left: *mut libc::timespec) -> i32 {
    reached();
    // SAFETY: the guest's own `timespec` pointers, which this does not read.
    unsafe { libc::nanosleep(requested, left) }
}

unsafe extern "C" fn sched_yield() -> i32 {
    reached();
    unsafe { libc::sched_yield() }
}

exports::exports! {
    functions {
        "usleep" => usleep,
        "sleep" => sleep,
        "nanosleep" => nanosleep,
        "sched_yield" => sched_yield,
    }
}
