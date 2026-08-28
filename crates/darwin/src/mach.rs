//! The parts of Mach a program uses for time.
//!
//! Everything else Mach does - ports, tasks, threads, `vm_` - is refused by
//! name; see [`crate::divergent`]. The clock is the exception because it is not
//! really a kernel interface at all: `mach_absolute_time` is a monotonic tick
//! count and a ratio to turn it into nanoseconds, and every host has both.
//!
//! The ratio here is 1/1, so a tick *is* a nanosecond. Darwin on Apple Silicon
//! reports 125/3 over a 24 MHz counter and a guest that reads the timebase gets
//! the same answer either way; one that assumes 1/1 without asking - and they
//! exist - is right here and wrong on the Mac, which is the safer direction.

// Everything below is compiled and tested on macOS and bound to nothing there:
// the host's own Mach is the guest's. See [`symbol`].
#![cfg_attr(target_os = "macos", allow(dead_code))]

use std::ffi::c_void;

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
}

/// Everything [`symbol`] answers.
pub const NAMES: &[&str] = &[
    "mach_absolute_time",
    "mach_continuous_time",
    "mach_timebase_info",
    "CACurrentMediaTime",
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
