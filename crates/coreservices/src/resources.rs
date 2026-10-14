//! The Resource Manager, with no resource file open.
//!
//! A 64-bit program still links it for legacy lookups - Python 2's `MacOS`
//! module fetches `'Estr'` resources to word an `OSStatus` - and every caller
//! already handles "not found", which is the true answer here: no resource
//! fork or `.rsrc` file is ever opened into the chain.

use std::cell::Cell;
use std::ffi::c_void;

type OSErr = i16;
type Handle = *mut *mut c_void;

const NO_ERR: OSErr = 0;
/// `resNotFound`.
const RES_NOT_FOUND: OSErr = -192;

thread_local! {
    /// What `ResError` reports: the Resource Manager's last answer, per thread
    /// as Carbon keeps it.
    static RES_ERROR: Cell<OSErr> = const { Cell::new(NO_ERR) };
}

/// `Handle GetResource(ResType type, ResID id)`
pub extern "C" fn GetResource(_kind: u32, _id: i16) -> Handle {
    RES_ERROR.with(|e| e.set(RES_NOT_FOUND));
    std::ptr::null_mut()
}

/// `Handle Get1Resource(ResType type, ResID id)`, the current file only.
pub extern "C" fn Get1Resource(kind: u32, id: i16) -> Handle {
    GetResource(kind, id)
}

/// `OSErr ResError(void)`
pub extern "C" fn ResError() -> OSErr {
    RES_ERROR.with(Cell::get)
}

/// `void ReleaseResource(Handle)`. No handle this module hands out is ever
/// non-null, so there is nothing to release.
pub extern "C" fn ReleaseResource(_resource: Handle) {
    RES_ERROR.with(|e| e.set(RES_NOT_FOUND));
}

/// `void HLock(Handle)`. Handles here never move, so locking is a no-op.
pub extern "C" fn HLock(_handle: Handle) {}

/// `void HUnlock(Handle)`
pub extern "C" fn HUnlock(_handle: Handle) {}

/// `UInt32 TickCount(void)`: sixtieths of a second since startup, which wraps
/// as it does on a Mac.
pub extern "C" fn TickCount() -> u32 {
    let mut now = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `now` is writable storage for a `timespec`.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut now) };
    let ticks = now.tv_sec as u64 * 60 + now.tv_nsec as u64 * 60 / 1_000_000_000;
    ticks as u32
}

exports::exports! {
    functions {
        "GetResource" => GetResource,
        "Get1Resource" => Get1Resource,
        "ResError" => ResError,
        "ReleaseResource" => ReleaseResource,
        "HLock" => HLock,
        "HUnlock" => HUnlock,
        "TickCount" => TickCount,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// With no resource file open every lookup misses, and says why.
    #[test]
    fn a_resource_lookup_reports_not_found() {
        assert!(GetResource(u32::from_be_bytes(*b"Estr"), -43).is_null());
        assert_eq!(ResError(), RES_NOT_FOUND);
    }

    #[test]
    fn tick_count_advances() {
        let before = TickCount();
        std::thread::sleep(std::time::Duration::from_millis(40));
        assert!(TickCount().wrapping_sub(before) >= 2);
    }
}
