//! `clock_gettime` and the clock ids it takes, which the two platforms number
//! differently.
//!
//! Only the `clockid_t` diverges - a `struct timespec` is two eight-byte words on
//! both - but it diverges in the worst possible way, by colliding rather than by
//! going out of range:
//!
//! | | Darwin | Linux |
//! |---|---|---|
//! | `CLOCK_MONOTONIC` | 6 | 1 |
//! | `CLOCK_MONOTONIC_COARSE` | - | 6 |
//! | `CLOCK_THREAD_CPUTIME_ID` | 16 | 3 |
//!
//! So a guest reading Darwin's `CLOCK_MONOTONIC` through the host's `clock_gettime`
//! is answered from `CLOCK_MONOTONIC_COARSE`: a clock that succeeds, advances, and is
//! quantised to the timer tick. Frame pacing built on it stutters and nothing reports
//! an error. Darwin's `CLOCK_THREAD_CPUTIME_ID` is out of range and merely fails.
//!
//! The other half of the mapping is that Darwin splits monotonic time by whether it
//! counts a sleep, where Linux splits it by whether it is NTP-adjusted. Darwin's
//! `CLOCK_MONOTONIC` runs while the machine sleeps and `CLOCK_UPTIME_RAW` does not,
//! which is `CLOCK_BOOTTIME` and `CLOCK_MONOTONIC` here. Counting the sleep is the
//! property a guest actually depends on, so it is what the mapping preserves; the
//! `_RAW` promise of no frequency adjustment has no host equivalent and is dropped.

// Compiled and tested on macOS, bound to nothing there: Darwin's clock ids are the
// host's. See [`symbol`].
#![cfg_attr(target_os = "macos", allow(dead_code))]

use std::ffi::c_void;

/// `clockid_t`, from Darwin's `<time.h>`.
pub mod id {
    #[allow(non_camel_case_types)]

    pub type clockid_t = i32;

    pub const REALTIME: clockid_t = 0;
    pub const MONOTONIC: clockid_t = 6;
    pub const MONOTONIC_RAW: clockid_t = 4;
    pub const MONOTONIC_RAW_APPROX: clockid_t = 5;
    pub const UPTIME_RAW: clockid_t = 8;
    pub const UPTIME_RAW_APPROX: clockid_t = 9;
    pub const PROCESS_CPUTIME_ID: clockid_t = 12;
    pub const THREAD_CPUTIME_ID: clockid_t = 16;
}

/// The host clocks the mapping picks from, named by the property being preserved
/// rather than by the host's spelling - a host without `CLOCK_BOOTTIME` still has to
/// answer, and answering with the nearest clock is what these names make explicit.
#[cfg(target_os = "linux")]
mod host {
    use libc::clockid_t;

    pub const REALTIME: clockid_t = libc::CLOCK_REALTIME;
    pub const WITH_SLEEP: clockid_t = libc::CLOCK_BOOTTIME;
    pub const WITHOUT_SLEEP: clockid_t = libc::CLOCK_MONOTONIC;
    pub const WITHOUT_SLEEP_COARSE: clockid_t = libc::CLOCK_MONOTONIC_COARSE;
    pub const PROCESS_CPU: clockid_t = libc::CLOCK_PROCESS_CPUTIME_ID;
    pub const THREAD_CPU: clockid_t = libc::CLOCK_THREAD_CPUTIME_ID;

    unsafe extern "C" {
        pub fn clock() -> clockid_t;
    }
}

/// As above, for a host with no clock that counts a sleep and none that is coarse.
/// Both fall back to plain monotonic, which differs only across a suspend.
#[cfg(not(target_os = "linux"))]
mod host {
    use libc::clockid_t;

    pub const REALTIME: clockid_t = libc::CLOCK_REALTIME;
    pub const WITH_SLEEP: clockid_t = libc::CLOCK_MONOTONIC;
    pub const WITHOUT_SLEEP: clockid_t = libc::CLOCK_MONOTONIC;
    pub const WITHOUT_SLEEP_COARSE: clockid_t = libc::CLOCK_MONOTONIC;
    pub const PROCESS_CPU: clockid_t = libc::CLOCK_PROCESS_CPUTIME_ID;
    pub const THREAD_CPU: clockid_t = libc::CLOCK_THREAD_CPUTIME_ID;
}

const CLOCK_MAPPING: [(id::clockid_t, libc::clockid_t); 8] = [
    (id::REALTIME, host::REALTIME),
    (id::MONOTONIC, host::WITH_SLEEP),
    (id::MONOTONIC_RAW, host::WITH_SLEEP),
    // No host clock is both coarse and sleep-counting; sleep-counting wins, and
    // the cost is only that the read is not the cheap one.
    (id::MONOTONIC_RAW_APPROX, host::WITH_SLEEP),
    (id::UPTIME_RAW, host::WITHOUT_SLEEP),
    (id::UPTIME_RAW_APPROX, host::WITHOUT_SLEEP_COARSE),
    (id::PROCESS_CPUTIME_ID, host::PROCESS_CPU),
    (id::THREAD_CPUTIME_ID, host::THREAD_CPU),
];

/// The host clock a guest's `clockid_t` names, or `None` if it names none.
///
/// An unrecognised id is refused rather than passed through: the host's own numbering
/// starts at zero too, so a wrong id is a *working* clock rather than an error.
fn translate(guest: id::clockid_t) -> Option<libc::clockid_t> {
    Some(match guest {
        id::REALTIME => host::REALTIME,
        id::MONOTONIC | id::MONOTONIC_RAW => host::WITH_SLEEP,
        // No host clock is both coarse and sleep-counting; sleep-counting wins, and
        // the cost is only that the read is not the cheap one.
        id::MONOTONIC_RAW_APPROX => host::WITH_SLEEP,
        id::UPTIME_RAW => host::WITHOUT_SLEEP,
        id::UPTIME_RAW_APPROX => host::WITHOUT_SLEEP_COARSE,
        id::PROCESS_CPUTIME_ID => host::PROCESS_CPU,
        id::THREAD_CPUTIME_ID => host::THREAD_CPU,
        _ => return None,
    })
}

fn to_darwin(host: libc::clockid_t) -> Option<id::clockid_t> {
    let clock = CLOCK_MAPPING
        .iter()
        .find(|clock| clock.1 == host)
        .and_then(|clock| Some(clock.0));
    clock
}

/// `int clock_gettime(clockid_t, struct timespec *)`.
///
/// # Safety
///
/// `ts` is null or a writable `struct timespec *`, which is two eight-byte words on
/// Darwin and on this host alike.
unsafe extern "C" fn gettime(clock: id::clockid_t, ts: *mut c_void) -> i32 {
    let Some(host) = translate(clock) else {
        crate::errno::set(libc::EINVAL);
        return -1;
    };
    // SAFETY: the caller's guarantee, and the two layouts agree.
    unsafe { libc::clock_gettime(host, ts as *mut libc::timespec) }
}

/// `int clock_getres(clockid_t, struct timespec *)`.
///
/// # Safety
///
/// As [`gettime`].
unsafe extern "C" fn getres(clock: id::clockid_t, ts: *mut c_void) -> i32 {
    let Some(host) = translate(clock) else {
        crate::errno::set(libc::EINVAL);
        return -1;
    };
    // SAFETY: the caller's guarantee, and the two layouts agree.
    unsafe { libc::clock_getres(host, ts as *mut libc::timespec) }
}

/// `uint64_t clock_gettime_nsec_np(clockid_t)`, which has no host spelling at all.
///
/// Zero is the failure Darwin reports, with `errno` set; there is no other value a
/// `uint64_t` return can reserve.
unsafe extern "C" fn gettime_nsec_np(clock: id::clockid_t) -> u64 {
    let Some(host) = translate(clock) else {
        crate::errno::set(libc::EINVAL);
        return 0;
    };
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `clock_gettime` writes the `timespec` it is given and nothing else.
    if unsafe { libc::clock_gettime(host, &mut ts) } != 0 {
        return 0;
    }
    (ts.tv_sec as u64)
        .wrapping_mul(1_000_000_000)
        .wrapping_add(ts.tv_nsec as u64)
}

unsafe extern "C" fn clock() -> id::clockid_t {
    let host_clock = unsafe { host::clock() };
    let Some(darwin_clock) = to_darwin(host_clock) else {
        panic!("unsupported host clock {}", host_clock);
    };
    darwin_clock
}

/// Empty on macOS, where the guest's clock ids are the host's.
#[cfg(target_os = "macos")]
pub fn symbol(_name: &str) -> Option<u64> {
    None
}

#[cfg(not(target_os = "macos"))]
pub fn symbol(name: &str) -> Option<u64> {
    let f: *const () = match name {
        "clock_gettime" => gettime as *const (),
        "clock_getres" => getres as *const (),
        "clock_gettime_nsec_np" => gettime_nsec_np as *const (),
        "clock" => clock as *const (),
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
pub const NAMES: &[&str] = &["clock_gettime", "clock_getres", "clock_gettime_nsec_np"];

#[cfg(test)]
mod tests {
    use super::*;

    /// The collision this module exists for: Darwin's monotonic id is the host's
    /// coarse monotonic, so the mapping must not be the identity.
    #[test]
    #[cfg(target_os = "linux")]
    fn darwin_monotonic_is_not_the_host_s_number() {
        assert_eq!(id::MONOTONIC, libc::CLOCK_MONOTONIC_COARSE as id::clockid_t);
        assert_eq!(translate(id::MONOTONIC), Some(libc::CLOCK_BOOTTIME));
    }

    #[test]
    fn an_unknown_clock_is_refused_rather_than_read() {
        assert_eq!(translate(99), None);
        // SAFETY: no memory is touched on the refusing path.
        assert_eq!(unsafe { gettime_nsec_np(99) }, 0);
        assert_eq!(crate::errno::get(), libc::EINVAL);
        crate::errno::set(0);
    }

    #[test]
    fn every_named_clock_reads() {
        for clock in [
            id::REALTIME,
            id::MONOTONIC,
            id::MONOTONIC_RAW,
            id::MONOTONIC_RAW_APPROX,
            id::UPTIME_RAW,
            id::UPTIME_RAW_APPROX,
            id::PROCESS_CPUTIME_ID,
            id::THREAD_CPUTIME_ID,
        ] {
            // SAFETY: a live local of the right type.
            let mut ts = libc::timespec {
                tv_sec: 0,
                tv_nsec: 0,
            };
            // SAFETY: `ts` is a writable `struct timespec`.
            let rc = unsafe { gettime(clock, &mut ts as *mut _ as *mut c_void) };
            assert_eq!(rc, 0, "clock {clock} must read");
        }
    }

    /// The wall clock and a monotonic clock must not be confused for one another:
    /// the first is a Unix timestamp and the second a much smaller uptime.
    #[test]
    fn realtime_and_uptime_are_different_timelines() {
        // SAFETY: neither touches caller memory.
        let (real, up) = unsafe {
            (
                gettime_nsec_np(id::REALTIME),
                gettime_nsec_np(id::UPTIME_RAW),
            )
        };
        assert!(
            real > up,
            "the wall clock counts from 1970 and uptime from boot"
        );
    }
}
