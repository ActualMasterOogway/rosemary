//! A minimal IORegistry, with opaque services only for SDL HID devices.
//!
//! `IOServiceGetMatchingServices` returning an empty iterator is a real answer:
//! it is what a Mac says for a match nothing satisfies, and a program that
//! enumerates devices is written for it. Nothing here invents a device, because
//! a fabricated `IORegistryEntry` would be one whose properties the caller then
//! reads and believes.
//!
//! What the empty registry does have to be is *consistent*. A program takes an
//! iterator, drains it, releases it, and arms a notification with it; every one
//! of those has to succeed on nothing, or the failure lands somewhere other
//! than where the devices are missing. So:
//!
//! - a matching dictionary is a real CFDictionary, because the caller releases
//!   it and may put keys in it first;
//! - an iterator is `0`, and `IOIteratorNext(0)` is "no more" rather than an
//!   error - draining an empty iterator is what *arms* a matching notification,
//!   so a caller that cannot drain it never gets one;
//! - a notification port is a real object, because
//!   `IONotificationPortGetRunLoopSource` is called on it without a check;
//! - and every `IOObject` call takes `0` and answers success, because that is
//!   the only object handle a caller can be holding.
//!
//! HID devices are not enumerated through this registry. Their SDL instance id
//! is exposed as a service solely so HIDAPI can turn its `DevSrvsID:` path back
//! into the `IOHIDDeviceRef` it already enumerated through `IOHIDManager`.
//!
//! Deliberately absent: anything that would hand back a property value.
//! `IORegistryEntryCreateCFProperty` answers null - there is no entry it could
//! have come from, and a null property is a case every caller has, where a
//! made-up serial number or vendor id is not.

#![allow(non_snake_case)]

use std::ffi::{c_char, c_void};

use corefoundation::base::CFTypeRef;
use corefoundation::callbacks::Policy;
use corefoundation::collections;

type IOReturn = i32;
type KernReturn = i32;
/// `io_object_t`, `io_iterator_t`, `io_service_t`, `io_registry_entry_t`: all
/// `mach_port_t`, and all `0` here.
type IoObject = u32;

const SUCCESS: IOReturn = 0;
/// `kIOReturnNoDevice`. The honest failure for a lookup in an empty registry,
/// and distinct from `kIOReturnError` so a caller that tells them apart can.
const NO_DEVICE: IOReturn = -536_870_212;

/// `mach_port_t kIOMasterPortDefault` - and `kIOMainPortDefault`, its current
/// spelling.
///
/// `MACH_PORT_NULL`, which is its value on a Mac too: the constant means "ask
/// for the default", not "here is the master port". A guest loads it and hands
/// it straight to a lookup, so the only thing that matters is that the *cell*
/// exists - an unbound data import here is a load from address zero, which is
/// the fault this symbol was found by.
pub fn data_symbols() -> Vec<(String, u64)> {
    ["_kIOMasterPortDefault", "_kIOMainPortDefault"]
        .into_iter()
        .map(|name| (name.to_string(), corefoundation::base::global_cell(name, 0)))
        .collect()
}

/// `CFMutableDictionaryRef IOServiceMatching(const char *name)`.
///
/// The dictionary really does carry `IOProviderClass`, because a caller is
/// entitled to add keys to it and to read back what it asked for.
pub unsafe extern "C" fn IOServiceMatching(name: *const c_char) -> CFTypeRef {
    let key = corefoundation::string::from_bytes(b"IOProviderClass", false);
    // SAFETY: the caller's obligation; a null name gives an empty dictionary,
    // which is a match on everything and still matches nothing here.
    let value = match unsafe { corefoundation::string::cstr_bytes(name) } {
        Some(bytes) => corefoundation::string::from_bytes(bytes, false),
        None => return empty_dictionary(),
    };
    collections::new_dictionary(
        &[(key, value)],
        Policy::from_key_callbacks(),
        Policy::from_type_callbacks(),
        true,
    )
}

/// `CFMutableDictionaryRef IORegistryEntryIDMatching(uint64_t entryID)`.
pub unsafe extern "C" fn IORegistryEntryIDMatching(entry: u64) -> CFTypeRef {
    let key = corefoundation::string::from_bytes(b"IORegistryEntryID", false);
    let value = collections::new_number(collections::Number::Integer(entry as i64));
    collections::new_dictionary(
        &[(key, value)],
        Policy::from_key_callbacks(),
        Policy::from_type_callbacks(),
        true,
    )
}

fn empty_dictionary() -> CFTypeRef {
    collections::new_dictionary(
        &[],
        Policy::from_key_callbacks(),
        Policy::from_type_callbacks(),
        true,
    )
}

/// The matching dictionary is **consumed** by every `IOServiceGetMatching*`
/// call, which is the one piece of IOKit ownership a caller does not do itself.
///
/// # Safety
///
/// `matching` must be null or a CFTypeRef.
unsafe fn consume(matching: CFTypeRef) {
    if !matching.is_null() {
        // SAFETY: the caller's obligation, and the call is a release of a
        // reference the caller has just handed over.
        unsafe { corefoundation::api::CFRelease(matching) };
    }
}

/// `kern_return_t IOServiceGetMatchingServices(port, matching, io_iterator_t *)`.
pub unsafe extern "C" fn IOServiceGetMatchingServices(
    _port: u32,
    matching: CFTypeRef,
    existing: *mut IoObject,
) -> KernReturn {
    // SAFETY: the reference the caller handed over.
    unsafe { consume(matching) };
    if !existing.is_null() {
        // SAFETY: an out parameter the caller sized.
        unsafe { existing.write(0) };
    }
    SUCCESS
}

/// Resolve the synthetic service id in an `IORegistryEntryIDMatching`
/// dictionary. Other matching dictionaries still match nothing.
pub unsafe extern "C" fn IOServiceGetMatchingService(_port: u32, matching: CFTypeRef) -> IoObject {
    let key = corefoundation::string::from_bytes(b"IORegistryEntryID", false);
    // SAFETY: the caller's matching dictionary.
    let value = unsafe { corefoundation::api::CFDictionaryGetValue(matching, key) };
    // SAFETY: a value from that dictionary, if present.
    let entry = unsafe { collections::number(value) }
        .and_then(|number| IoObject::try_from(number.as_i64()).ok())
        .unwrap_or(0);
    // SAFETY: the lookup does not retain its temporary key.
    unsafe { corefoundation::api::CFRelease(key) };
    // SAFETY: the reference the caller handed over.
    unsafe { consume(matching) };
    entry
}

/// `io_object_t IOIteratorNext(io_iterator_t)`. Zero ends the loop.
pub unsafe extern "C" fn IOIteratorNext(_iterator: IoObject) -> IoObject {
    0
}

/// `boolean_t IOIteratorIsValid(io_iterator_t)`.
///
/// True, and that is not the same as "there is something in it": invalid means
/// the registry changed under the iteration and the caller must start again, so
/// answering false would put a program that checks into a loop.
pub unsafe extern "C" fn IOIteratorIsValid(_iterator: IoObject) -> u32 {
    1
}

pub unsafe extern "C" fn IOObjectRelease(_object: IoObject) -> KernReturn {
    SUCCESS
}

pub unsafe extern "C" fn IOObjectRetain(_object: IoObject) -> KernReturn {
    SUCCESS
}

pub unsafe extern "C" fn IOObjectIsEqualTo(a: IoObject, b: IoObject) -> u32 {
    u32::from(a == b)
}

/// `io_registry_entry_t IORegistryEntryFromPath(port, const io_string_t path)`.
pub unsafe extern "C" fn IORegistryEntryFromPath(_port: u32, _path: *const c_char) -> IoObject {
    0
}

pub unsafe extern "C" fn IORegistryEntryGetParentEntry(
    _entry: IoObject,
    _plane: *const c_char,
    parent: *mut IoObject,
) -> KernReturn {
    if !parent.is_null() {
        // SAFETY: an out parameter the caller sized.
        unsafe { parent.write(0) };
    }
    NO_DEVICE
}

pub unsafe extern "C" fn IORegistryEntryGetRegistryEntryID(
    entry: IoObject,
    id: *mut u64,
) -> KernReturn {
    if !id.is_null() {
        // SAFETY: an out parameter the caller sized.
        unsafe { id.write(entry as u64) };
    }
    match entry {
        0 => NO_DEVICE,
        _ => SUCCESS,
    }
}

/// `CFTypeRef IORegistryEntryCreateCFProperty(entry, key, allocator, options)`.
pub unsafe extern "C" fn IORegistryEntryCreateCFProperty(
    _entry: IoObject,
    _key: CFTypeRef,
    _allocator: CFTypeRef,
    _options: u32,
) -> CFTypeRef {
    std::ptr::null_mut()
}

pub unsafe extern "C" fn IORegistryEntryCreateCFProperties(
    _entry: IoObject,
    properties: *mut CFTypeRef,
    _allocator: CFTypeRef,
    _options: u32,
) -> KernReturn {
    if !properties.is_null() {
        // SAFETY: an out parameter the caller sized.
        unsafe { properties.write(std::ptr::null_mut()) };
    }
    NO_DEVICE
}

/// An `IONotificationPortRef`. Opaque to the guest and never dereferenced by
/// it; it exists so that the pointer is not null.
struct NotificationPort {
    _private: u64,
}

/// `IONotificationPortRef IONotificationPortCreate(mach_port_t)`.
///
/// A real allocation, because the calls that follow it - `GetRunLoopSource`,
/// `GetMachPort`, `AddMatchingNotification` - are made without checking, and a
/// null here faults inside the first of them rather than saying anything.
pub unsafe extern "C" fn IONotificationPortCreate(_port: u32) -> *mut c_void {
    Box::into_raw(Box::new(NotificationPort { _private: 0 })).cast()
}

/// `void IONotificationPortDestroy(IONotificationPortRef)`.
pub unsafe extern "C" fn IONotificationPortDestroy(port: *mut c_void) {
    if port.is_null() {
        return;
    }
    // SAFETY: the guest hands back what `IONotificationPortCreate` returned,
    // which is this crate's own `Box`.
    drop(unsafe { Box::from_raw(port.cast::<NotificationPort>()) });
}

/// `mach_port_t IONotificationPortGetMachPort(IONotificationPortRef)`.
///
/// `MACH_PORT_NULL`. A port from `darwin::mach` would be one nothing ever sends
/// to, and a caller that adds it to a run loop would then wait on a port that
/// cannot receive - which is the same nothing, arrived at more expensively.
pub unsafe extern "C" fn IONotificationPortGetMachPort(_port: *mut c_void) -> u32 {
    0
}

/// `kern_return_t IOServiceAddMatchingNotification(port, type, matching,
/// callback, refcon, io_iterator_t *)`.
///
/// Succeeds with an empty iterator. The caller must drain it - that is what
/// arms the notification on a Mac - and draining nothing is what it does here.
/// The callback is never called, because nothing is ever added to this
/// registry.
pub unsafe extern "C" fn IOServiceAddMatchingNotification(
    _port: *mut c_void,
    _notification_type: *const c_char,
    matching: CFTypeRef,
    _callback: *const c_void,
    _refcon: *mut c_void,
    notification: *mut IoObject,
) -> KernReturn {
    // SAFETY: the reference the caller handed over.
    unsafe { consume(matching) };
    if !notification.is_null() {
        // SAFETY: an out parameter the caller sized.
        unsafe { notification.write(0) };
    }
    SUCCESS
}

/// `void IODispatchCalloutFromMessage(void *unused, mach_msg_header_t *, void *reference)`.
///
/// Nothing here delivers an IOKit message, so there is never one to dispatch.
pub unsafe extern "C" fn IODispatchCalloutFromMessage(
    _unused: *mut c_void,
    _message: *mut c_void,
    _reference: *mut c_void,
) {
}

pub const NAMES: &[&str] = &[
    "IOServiceMatching",
    "IORegistryEntryIDMatching",
    "IOServiceGetMatchingServices",
    "IOServiceGetMatchingService",
    "IOIteratorNext",
    "IOIteratorIsValid",
    "IOObjectRelease",
    "IOObjectRetain",
    "IOObjectIsEqualTo",
    "IORegistryEntryFromPath",
    "IORegistryEntryGetParentEntry",
    "IORegistryEntryGetRegistryEntryID",
    "IORegistryEntryCreateCFProperty",
    "IORegistryEntryCreateCFProperties",
    "IONotificationPortCreate",
    "IONotificationPortDestroy",
    "IONotificationPortGetMachPort",
    "IOServiceAddMatchingNotification",
    "IODispatchCalloutFromMessage",
];

pub fn symbol(name: &str) -> Option<u64> {
    let addr = match name {
        "IOServiceMatching" => IOServiceMatching as *const (),
        "IORegistryEntryIDMatching" => IORegistryEntryIDMatching as *const (),
        "IOServiceGetMatchingServices" => IOServiceGetMatchingServices as *const (),
        "IOServiceGetMatchingService" => IOServiceGetMatchingService as *const (),
        "IOIteratorNext" => IOIteratorNext as *const (),
        "IOIteratorIsValid" => IOIteratorIsValid as *const (),
        "IOObjectRelease" => IOObjectRelease as *const (),
        "IOObjectRetain" => IOObjectRetain as *const (),
        "IOObjectIsEqualTo" => IOObjectIsEqualTo as *const (),
        "IORegistryEntryFromPath" => IORegistryEntryFromPath as *const (),
        "IORegistryEntryGetParentEntry" => IORegistryEntryGetParentEntry as *const (),
        "IORegistryEntryGetRegistryEntryID" => IORegistryEntryGetRegistryEntryID as *const (),
        "IORegistryEntryCreateCFProperty" => IORegistryEntryCreateCFProperty as *const (),
        "IORegistryEntryCreateCFProperties" => IORegistryEntryCreateCFProperties as *const (),
        "IONotificationPortCreate" => IONotificationPortCreate as *const (),
        "IONotificationPortDestroy" => IONotificationPortDestroy as *const (),
        "IONotificationPortGetMachPort" => IONotificationPortGetMachPort as *const (),
        "IOServiceAddMatchingNotification" => IOServiceAddMatchingNotification as *const (),
        "IODispatchCalloutFromMessage" => IODispatchCalloutFromMessage as *const (),
        _ => return None,
    };
    Some(addr as usize as u64)
}
