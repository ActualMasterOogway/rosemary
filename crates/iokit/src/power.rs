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

/// `CFDictionaryRef IOPSCopyExternalPowerAdapterDetails(void)`.
///
/// Null, which is what a Mac with no adapter attached answers: wattage and
/// identity are an SMC's to report, and the host exposes none, so naming an
/// adapter would be inventing one. A `Copy`, so nothing is owned either way.
pub unsafe extern "C" fn IOPSCopyExternalPowerAdapterDetails() -> CFTypeRef {
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
        "IOPSCopyExternalPowerAdapterDetails" => IOPSCopyExternalPowerAdapterDetails,
        "IOPSNotificationCreateRunLoopSource" => IOPSNotificationCreateRunLoopSource,
    }
}
