//! The calls at which a guest thread says it is doing nothing.
//!
//! A thread that asks to sleep or to yield is a thread waiting for another one,
//! and so a thread that is part-way through nothing. That is worth knowing for
//! a resource only the thread holding it can give up.
//!
//! The OpenGL context is such a resource here. AppKit lets a program make one
//! context current on two threads and serialise the two itself - which is what
//! a program with a render thread does, setting the renderer up on the thread
//! it starts on and drawing on the thread that draws - while the GL underneath
//! this runtime allows a context one thread at a time and has no call by which
//! one thread can take it from another. So the handover has to happen on the
//! holder, at a moment it is known not to be drawing, and a sleep the program
//! was making anyway is exactly that moment. See `appkit::opengl`.
//!
//! The cost is one relaxed load, and a call the guest was about to enter the
//! kernel from.

use std::sync::atomic::{AtomicUsize, Ordering};

use machish::resolve::SyntheticDylib;

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
    // SAFETY: no arguments.
    unsafe { libc::sched_yield() }
}

/// The names this module answers, which are the sleeps and the yield.
pub const NAMES: &[&str] = &["usleep", "sleep", "nanosleep", "sched_yield"];

pub fn symbol(name: &str) -> Option<u64> {
    let f = match name {
        "usleep" => usleep as *const (),
        "sleep" => sleep as *const (),
        "nanosleep" => nanosleep as *const (),
        "sched_yield" => sched_yield as *const (),
        _ => return None,
    };
    Some(f as usize as u64)
}

pub fn provide(lib: &mut SyntheticDylib) {
    for name in NAMES {
        crate::define_fn(lib, &format!("_{name}"), symbol(name).unwrap() as *const ());
    }
}
