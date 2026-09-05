//! `getrlimit`/`setrlimit`, where the resource numbers and the sentinel for
//! "no limit" both differ.
//!
//! `struct rlimit` is the same sixteen bytes on both platforms, which is what
//! makes this look safe to pass through. Two things inside it are not:
//!
//! | resource | Darwin | this host |
//! |---|---|---|
//! | `RLIMIT_AS` / `RLIMIT_RSS` | 5 | 9 / 5 |
//! | `RLIMIT_MEMLOCK` | 6 | 8 |
//! | `RLIMIT_NPROC` | 7 | 6 |
//! | `RLIMIT_NOFILE` | **8** | **7** |
//!
//! So a program asking Darwin's `RLIMIT_NOFILE` is answered here about the
//! memory it may lock, and one asking for the process limit is told about
//! address space. Every one of those succeeds.
//!
//! And `RLIM_INFINITY` is `1<<63 - 1` on Darwin and `~0` here - the sentinel
//! trap from `SEM_FAILED`, one header over. A guest that compares the limit it
//! read against its own `RLIM_INFINITY` concludes the limit is finite and
//! enormous, and one that hands `RLIM_INFINITY` back to `setrlimit` asks for a
//! specific huge number instead of "no limit".

#![cfg(not(target_os = "macos"))]

use std::ffi::c_int;

use machish::resolve::SyntheticDylib;

/// Darwin's `struct rlimit`. The same layout as this host's, and named here so
/// the guest's is never confused for it.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct Rlimit {
    pub cur: u64,
    pub max: u64,
}

const _: () = assert!(size_of::<Rlimit>() == size_of::<libc::rlimit>());

/// Darwin's `RLIM_INFINITY`.
const DARWIN_INFINITY: u64 = (1 << 63) - 1;

/// The host resource `darwin` is naming, or `None` for one this host has no
/// counterpart for.
///
/// `RLIMIT_CPU` through `RLIMIT_CORE` agree; everything above them does not.
/// Darwin's `RLIMIT_AS` and `RLIMIT_RSS` are the same number, and it is the
/// address space that is enforced there, so it maps to this host's
/// `RLIMIT_AS`.
fn resource(darwin: c_int) -> Option<u32> {
    Some(match darwin {
        0..=4 => darwin as u32,
        5 => libc::RLIMIT_AS,
        6 => libc::RLIMIT_MEMLOCK,
        7 => libc::RLIMIT_NPROC,
        8 => libc::RLIMIT_NOFILE,
        _ => return None,
    })
}

fn to_guest(host: u64) -> u64 {
    match host == libc::RLIM_INFINITY {
        true => DARWIN_INFINITY,
        false => host,
    }
}

fn to_host(guest: u64) -> u64 {
    match guest >= DARWIN_INFINITY {
        true => libc::RLIM_INFINITY,
        false => guest,
    }
}

/// `int getrlimit(int, struct rlimit *)`
///
/// # Safety
///
/// `out` is null or a writable `struct rlimit`.
pub unsafe extern "C" fn getrlimit(darwin: c_int, out: *mut Rlimit) -> c_int {
    if out.is_null() {
        crate::errno::set(libc::EFAULT);
        return -1;
    }
    let Some(resource) = resource(darwin) else {
        crate::errno::set(libc::EINVAL);
        return -1;
    };
    let mut host = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: a live `struct rlimit` and a resource this host knows.
    if unsafe { libc::getrlimit(resource, &raw mut host) } != 0 {
        return -1;
    }
    // SAFETY: the caller states `out` is writable.
    unsafe {
        *out = Rlimit {
            cur: to_guest(host.rlim_cur),
            max: to_guest(host.rlim_max),
        }
    };
    0
}

/// `int setrlimit(int, const struct rlimit *)`
///
/// # Safety
///
/// `value` is null or a readable `struct rlimit`.
pub unsafe extern "C" fn setrlimit(darwin: c_int, value: *const Rlimit) -> c_int {
    if value.is_null() {
        crate::errno::set(libc::EFAULT);
        return -1;
    }
    let Some(resource) = resource(darwin) else {
        crate::errno::set(libc::EINVAL);
        return -1;
    };
    // SAFETY: the caller states `value` is readable.
    let value = unsafe { *value };
    let host = libc::rlimit {
        rlim_cur: to_host(value.cur),
        rlim_max: to_host(value.max),
    };
    // SAFETY: a live `struct rlimit` and a resource this host knows.
    unsafe { libc::setrlimit(resource, &raw const host) }
}

pub const NAMES: &[&str] = &["getrlimit", "setrlimit"];

pub fn symbol(name: &str) -> Option<u64> {
    let f: *const () = match name {
        "getrlimit" => getrlimit as *const (),
        "setrlimit" => setrlimit as *const (),
        _ => return None,
    };
    Some(f as usize as u64)
}

pub fn provide(lib: &mut SyntheticDylib) {
    for name in NAMES {
        crate::define_fn(
            lib,
            &format!("_{name}"),
            symbol(name).unwrap_or(0) as *const (),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Darwin's `RLIMIT_NOFILE`, which is this host's `RLIMIT_MEMLOCK` if the
    /// number is passed through.
    const DARWIN_NOFILE: c_int = 8;

    #[test]
    fn every_listed_name_resolves() {
        for name in NAMES {
            assert!(symbol(name).is_some(), "{name}");
        }
    }

    #[test]
    fn the_descriptor_limit_is_the_descriptor_limit_and_not_the_lock_limit() {
        let mut guest = Rlimit::default();
        // SAFETY: a live `struct rlimit`.
        assert_eq!(unsafe { getrlimit(DARWIN_NOFILE, &raw mut guest) }, 0);

        let mut host = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        // SAFETY: as above, asked of this host directly.
        assert_eq!(
            unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &raw mut host) },
            0
        );
        assert_eq!(guest.cur, to_guest(host.rlim_cur));
        assert!(guest.cur > 0, "a process can open a file");
    }

    /// The sentinel is the trap: an unlimited resource must come back as the
    /// number the guest spells `RLIM_INFINITY`, not as this host's.
    #[test]
    fn no_limit_is_reported_in_the_guests_spelling() {
        assert_eq!(to_guest(libc::RLIM_INFINITY), DARWIN_INFINITY);
        assert_eq!(to_host(DARWIN_INFINITY), libc::RLIM_INFINITY);
        assert_ne!(DARWIN_INFINITY, libc::RLIM_INFINITY);
        // A real one, read through the entry point: core dumps are unlimited
        // on most hosts, and `RLIMIT_CORE` is 4 on both.
        let mut guest = Rlimit::default();
        // SAFETY: a live `struct rlimit`.
        assert_eq!(unsafe { getrlimit(4, &raw mut guest) }, 0);
        assert!(guest.max <= DARWIN_INFINITY, "never this host's sentinel");
    }

    #[test]
    fn a_resource_this_host_has_no_counterpart_for_is_refused() {
        let mut guest = Rlimit::default();
        // SAFETY: a live `struct rlimit`, and a resource number Darwin does
        // not define either.
        assert_eq!(unsafe { getrlimit(64, &raw mut guest) }, -1);
        assert_eq!(crate::errno::get(), libc::EINVAL);
        // SAFETY: a null buffer is what this checks.
        assert_eq!(
            unsafe { getrlimit(DARWIN_NOFILE, std::ptr::null_mut()) },
            -1
        );
    }
}
