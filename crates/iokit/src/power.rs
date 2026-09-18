//! `IOPowerSources`, answered as a machine running on wall power.
//!
//! There is no battery to describe here and no `IOPMPowerSource` to ask, so the
//! list of power sources is empty. That is not a stand-in: it is what
//! `IOPSCopyPowerSourcesList` returns on a Mac mini, and a program that reads
//! it gets "no battery", which is true.
//!
//! The blob is the reason this is a module rather than three functions
//! returning null. `IOPSCopyPowerSourcesInfo` hands back an opaque `CFTypeRef`
//! whose only documented use is to be passed to the other two calls, and every
//! caller passes it on without checking it - so null does not reach a
//! diagnostic, it reaches `CFArrayGetCount(NULL)` one call later. An empty
//! dictionary is a real object that survives the retain, the pass and the
//! release.

#![allow(non_snake_case)]

use corefoundation::base::CFTypeRef;
use corefoundation::callbacks::Policy;
use corefoundation::collections;

/// `CFTypeRef IOPSCopyPowerSourcesInfo(void)` - a `Copy`, so the caller
/// releases it.
pub unsafe extern "C" fn IOPSCopyPowerSourcesInfo() -> CFTypeRef {
    collections::new_dictionary(
        &[],
        Policy::from_key_callbacks(),
        Policy::from_type_callbacks(),
        false,
    )
}

/// `CFArrayRef IOPSCopyPowerSourcesList(CFTypeRef blob)`.
///
/// Empty, and owned by the caller. The blob is not consulted: this
/// implementation puts nothing in it.
pub unsafe extern "C" fn IOPSCopyPowerSourcesList(_blob: CFTypeRef) -> CFTypeRef {
    collections::new_array(&[], Policy::from_type_callbacks(), false)
}

/// `CFDictionaryRef IOPSGetPowerSourceDescription(CFTypeRef blob, CFTypeRef ps)`.
///
/// Null, which is the answer for a source that is not in the list - and the
/// list is empty, so no caller can hold one that is. A `Get`, so nothing is
/// owned either way.
pub unsafe extern "C" fn IOPSGetPowerSourceDescription(
    _blob: CFTypeRef,
    _source: CFTypeRef,
) -> CFTypeRef {
    std::ptr::null_mut()
}

/// `CFRunLoopSourceRef IOPSNotificationCreateRunLoopSource(callback, context)`.
///
/// Null: nothing here changes power state, so a source would be one that never
/// fires. The callback is deliberately not retained - a program that gets null
/// knows it is not registered, where one holding a source that never fires
/// waits for a notification that cannot arrive.
pub unsafe extern "C" fn IOPSNotificationCreateRunLoopSource(
    _callback: *const std::ffi::c_void,
    _context: *mut std::ffi::c_void,
) -> CFTypeRef {
    std::ptr::null_mut()
}

exports::exports! {
    functions {
        "IOPSCopyPowerSourcesInfo" => IOPSCopyPowerSourcesInfo,
        "IOPSCopyPowerSourcesList" => IOPSCopyPowerSourcesList,
        "IOPSGetPowerSourceDescription" => IOPSGetPowerSourceDescription,
        "IOPSNotificationCreateRunLoopSource" => IOPSNotificationCreateRunLoopSource,
    }
}
