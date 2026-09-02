//! Mach, on a host that has not got one.
//!
//! Two very different things live here. The clock - `mach_absolute_time` and
//! the timebase - is not really a kernel interface at all and needs nothing but
//! a monotonic counter; it is in this file. Everything else is a synthetic
//! **port namespace**: names, rights, messages and a name server, built out of
//! unix sockets, and split across the modules below.
//!
//! The namespace exists for one reason. A program with a crash reporter -
//! Crashpad, which is most of them - registers a Mach exception port and talks
//! to its handler process over `mach_msg` before it will run a line of its own
//! code, and a program whose crash reporter fails to initialise does not
//! degrade, it stops. Refusing the Mach family by name, which is what this
//! crate did before, turns that into a segfault at address zero inside
//! libSystem with `mach_task_self_` the unbound data import behind it.
//!
//! Read [`wire`] first: it says what a port is made of, and the rest follows
//! from that.
//!
//! | Module | What it holds |
//! | --- | --- |
//! | [`wire`] | a port is a bound unix datagram socket; a right is its address |
//! | [`space`] | the task's names for the rights it holds |
//! | [`msg`] | `mach_msg`, and translating names at the process boundary |
//! | [`bootstrap`] | the name server two unrelated processes rendezvous through |
//! | [`task`] | `mach_task_self_`, the thread and host ports, exception ports |
//! | [`info`] | `task_info` and the host statistics, which are not port questions at all |
//! | [`sync`] | Mach semaphores, which are named out of the same space |
//! | [`kern`] | `kern_return_t` and the three numberings it is not |
//!
//! What is **not** here, and is refused rather than guessed at: exception
//! delivery (see [`task`]), out-of-line memory in a message (see [`msg`]),
//! `vm_`/`mach_vm_` against another task, and anything that would need a real
//! kernel to be watching - dead-name notifications above all.
//!
//! The ratio the timebase reports is 1/1, so a tick *is* a nanosecond. Darwin
//! on Apple Silicon reports 125/3 over a 24 MHz counter and a guest that reads
//! the timebase gets the same answer either way; one that assumes 1/1 without
//! asking - and they exist - is right here and wrong on the Mac, which is the
//! safer direction.

// The clock below is compiled and tested on macOS and bound to nothing there:
// the host's own Mach is the guest's. See [`symbol`].
#![cfg_attr(target_os = "macos", allow(dead_code))]

use std::ffi::c_void;

pub mod bootstrap;
pub mod info;
pub mod kern;
pub mod msg;
pub mod space;
pub mod sync;
pub mod task;
pub mod wire;

/// `mach_timebase_info_data_t`: the ratio from ticks to nanoseconds.
#[repr(C)]
struct TimebaseInfo {
    numer: u32,
    denom: u32,
}

/// Nanoseconds from `clock`, or zero if the host declines to say.
fn nanoseconds(clock: libc::clockid_t) -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `clock_gettime` writes the `timespec` it is given and nothing else.
    if unsafe { libc::clock_gettime(clock, &mut ts) } != 0 {
        return 0;
    }
    (ts.tv_sec as u64)
        .wrapping_mul(1_000_000_000)
        .wrapping_add(ts.tv_nsec as u64)
}

unsafe extern "C" fn absolute_time() -> u64 {
    nanoseconds(libc::CLOCK_MONOTONIC)
}

/// Ticks including time the machine spent asleep.
///
/// `CLOCK_BOOTTIME` where there is one; `CLOCK_MONOTONIC` otherwise, which
/// differs only across a suspend and is the closer of the two answers available.
unsafe extern "C" fn continuous_time() -> u64 {
    #[cfg(target_os = "linux")]
    let clock = libc::CLOCK_BOOTTIME;
    #[cfg(not(target_os = "linux"))]
    let clock = libc::CLOCK_MONOTONIC;
    nanoseconds(clock)
}

/// `kern_return_t mach_timebase_info(mach_timebase_info_t)`.
unsafe extern "C" fn timebase_info(info: *mut c_void) -> i32 {
    if info.is_null() {
        // `KERN_INVALID_ARGUMENT`.
        return 4;
    }
    // SAFETY: the caller passes a `mach_timebase_info_data_t` to fill in, which
    // is the two words written here.
    unsafe {
        *(info as *mut TimebaseInfo) = TimebaseInfo { numer: 1, denom: 1 };
    }
    0
}

/// `CACurrentMediaTime`, which is `mach_absolute_time` in seconds.
///
/// Not Mach's, but the same clock read through a different framework, and a
/// guest that mixes the two must see one timeline.
unsafe extern "C" fn current_media_time() -> f64 {
    nanoseconds(libc::CLOCK_MONOTONIC) as f64 / 1e9
}

/// Empty on macOS, where the host's own Mach is the guest's.
#[cfg(target_os = "macos")]
pub fn symbol(_name: &str) -> Option<u64> {
    None
}

#[cfg(not(target_os = "macos"))]
pub fn symbol(name: &str) -> Option<u64> {
    let f: *const () = match name {
        "mach_absolute_time" => absolute_time as *const (),
        "mach_continuous_time" => continuous_time as *const (),
        "mach_timebase_info" => timebase_info as *const (),
        "CACurrentMediaTime" => current_media_time as *const (),

        "mach_error" => kern::mach_error as *const (),
        "mach_error_string" => kern::mach_error_string as *const (),
        "bootstrap_strerror" => kern::bootstrap_strerror as *const (),

        "mach_task_self" => task::mach_task_self as *const (),
        "mach_host_self" => task::mach_host_self as *const (),
        "mach_thread_self" => task::mach_thread_self as *const (),
        "task_info" => info::task_info as *const (),
        "host_page_size" => info::host_page_size as *const (),
        "host_statistics" => info::host_statistics as *const (),
        "host_statistics64" => info::host_statistics64 as *const (),
        "task_get_special_port" => task::task_get_special_port as *const (),
        "task_set_special_port" => task::task_set_special_port as *const (),

        // The exception-port family. `task`, `host` and `thread` are the same
        // three calls with a different first argument, and nothing here
        // delivers from any of them, so one implementation answers all three.
        "task_set_exception_ports" => task::task_set_exception_ports as *const (),
        "thread_set_exception_ports" => task::thread_set_exception_ports as *const (),
        "host_set_exception_ports" => task::host_set_exception_ports as *const (),
        "task_get_exception_ports" | "host_get_exception_ports"
        | "thread_get_exception_ports" => task::task_get_exception_ports as *const (),
        "task_swap_exception_ports" | "host_swap_exception_ports"
        | "thread_swap_exception_ports" => task::task_swap_exception_ports as *const (),

        "mach_port_allocate" => space::mach_port_allocate as *const (),
        "mach_port_deallocate" => space::mach_port_deallocate as *const (),
        "mach_port_destroy" => space::mach_port_destroy as *const (),
        "mach_port_mod_refs" => space::mach_port_mod_refs as *const (),
        "mach_port_get_refs" => space::mach_port_get_refs as *const (),
        "mach_port_insert_right" => space::mach_port_insert_right as *const (),
        "mach_port_extract_right" => space::mach_port_extract_right as *const (),
        "mach_port_type" => space::mach_port_type as *const (),
        "mach_port_insert_member" => space::mach_port_insert_member as *const (),
        "mach_port_move_member" => space::mach_port_move_member as *const (),
        "mach_port_set_attributes" => space::mach_port_set_attributes as *const (),
        "mach_port_request_notification" => space::mach_port_request_notification as *const (),

        "mach_msg" => msg::mach_msg as *const (),
        "mach_msg_overwrite" => msg::mach_msg_overwrite as *const (),
        "mach_msg_destroy" => msg::mach_msg_destroy as *const (),
        "mig_get_reply_port" => msg::mig_get_reply_port as *const (),
        "mig_put_reply_port" => msg::mig_put_reply_port as *const (),
        "mig_dealloc_reply_port" => msg::mig_dealloc_reply_port as *const (),

        "bootstrap_check_in" => bootstrap::bootstrap_check_in as *const (),
        "bootstrap_look_up" => bootstrap::bootstrap_look_up as *const (),
        "bootstrap_look_up2" => bootstrap::bootstrap_look_up2 as *const (),
        "bootstrap_register" => bootstrap::bootstrap_register as *const (),
        "bootstrap_parent" => bootstrap::bootstrap_parent as *const (),

        "semaphore_create" => sync::semaphore_create as *const (),
        "semaphore_destroy" => sync::semaphore_destroy as *const (),
        "semaphore_signal" => sync::semaphore_signal as *const (),
        "semaphore_signal_all" => sync::semaphore_signal_all as *const (),
        "semaphore_signal_thread" => sync::semaphore_signal_thread as *const (),
        "semaphore_wait" => sync::semaphore_wait as *const (),
        "semaphore_timedwait" => sync::semaphore_timedwait as *const (),

        _ => return None,
    };
    Some(f as usize as u64)
}

#[cfg(target_os = "macos")]
pub fn provide(_lib: &mut machish::resolve::SyntheticDylib) {}

#[cfg(not(target_os = "macos"))]
pub fn provide(lib: &mut machish::resolve::SyntheticDylib) {
    for name in NAMES {
        crate::define_fn(lib, &format!("_{name}"), symbol(name).unwrap() as *const ());
    }
    // The three data imports. Unlike a function, an unbound one of these is not
    // refused at the call - it binds to zero and the guest's next instruction
    // dereferences it, which is a segfault at address zero with no symbol
    // anywhere near it. They are the reason this module has to exist at all
    // rather than leaving the family denied.
    for (mach_o, addr) in [
        ("_mach_task_self_", &raw const task::TASK_SELF_PORT as usize),
        ("_bootstrap_port", &raw const bootstrap::BOOTSTRAP_PORT as usize),
        ("_NDR_record", (&raw const task::NDR_RECORD).cast::<u8>() as usize),
    ] {
        crate::define_data(lib, mach_o, addr as u64);
    }
}

/// Every *function* [`symbol`] answers.
///
/// The data symbols are not here: [`provide`] publishes those by address, and
/// handing one out through `symbol` would let a caller bind it as code.
pub const NAMES: &[&str] = &[
    "mach_absolute_time",
    "mach_continuous_time",
    "mach_timebase_info",
    "CACurrentMediaTime",
    "mach_error",
    "mach_error_string",
    "bootstrap_strerror",
    "mach_task_self",
    "mach_host_self",
    "mach_thread_self",
    "task_info",
    "host_page_size",
    "host_statistics",
    "host_statistics64",
    "task_get_special_port",
    "task_set_special_port",
    "task_set_exception_ports",
    "task_get_exception_ports",
    "task_swap_exception_ports",
    "thread_set_exception_ports",
    "thread_get_exception_ports",
    "thread_swap_exception_ports",
    "host_set_exception_ports",
    "host_get_exception_ports",
    "host_swap_exception_ports",
    "mach_port_allocate",
    "mach_port_deallocate",
    "mach_port_destroy",
    "mach_port_mod_refs",
    "mach_port_get_refs",
    "mach_port_insert_right",
    "mach_port_extract_right",
    "mach_port_type",
    "mach_port_insert_member",
    "mach_port_move_member",
    "mach_port_set_attributes",
    "mach_port_request_notification",
    "mach_msg",
    "mach_msg_overwrite",
    "mach_msg_destroy",
    "mig_get_reply_port",
    "mig_put_reply_port",
    "mig_dealloc_reply_port",
    "bootstrap_check_in",
    "bootstrap_look_up",
    "bootstrap_look_up2",
    "bootstrap_register",
    "bootstrap_parent",
    "semaphore_create",
    "semaphore_destroy",
    "semaphore_signal",
    "semaphore_signal_all",
    "semaphore_signal_thread",
    "semaphore_wait",
    "semaphore_timedwait",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_clock_moves_forward_and_the_timebase_is_nanoseconds() {
        // SAFETY: neither takes an argument, and `timebase_info` is given a
        // real `mach_timebase_info_data_t`.
        unsafe {
            let first = absolute_time();
            let second = absolute_time();
            assert!(second >= first, "the monotonic clock went backwards");

            let mut info = TimebaseInfo { numer: 0, denom: 0 };
            assert_eq!(timebase_info(&mut info as *mut _ as *mut c_void), 0);
            assert_eq!((info.numer, info.denom), (1, 1));
            assert_eq!(timebase_info(std::ptr::null_mut()), 4);
        }
    }
}
