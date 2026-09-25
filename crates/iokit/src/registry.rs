#![allow(non_snake_case)]

use std::collections::{BTreeMap, VecDeque};
use std::ffi::{c_char, c_void};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};

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

/// Iterator handles are kept separate from the positive SDL instance ids used
/// as synthetic `io_service_t`s.
static NEXT_ITERATOR: AtomicU32 = AtomicU32::new(0x8000_0000);
static ITERATORS: Mutex<BTreeMap<IoObject, VecDeque<IoObject>>> = Mutex::new(BTreeMap::new());

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
    let services = crate::api::services();
    // SAFETY: the reference the caller handed over.
    unsafe { consume(matching) };
    if !existing.is_null() {
        let iterator = NEXT_ITERATOR.fetch_add(1, Ordering::Relaxed);
        if !services.is_empty() {
            ITERATORS
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(iterator, services.iter().copied().collect());
        }
        crate::trace_hid(format_args!(
            "IOServiceGetMatchingServices -> {} service(s)",
            services.len()
        ));
        // SAFETY: an out parameter the caller sized.
        unsafe { existing.write((!services.is_empty()).then_some(iterator).unwrap_or(0)) };
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
    crate::trace_hid(format_args!("IOServiceGetMatchingService -> {entry}"));
    entry
}

/// `io_object_t IOIteratorNext(io_iterator_t)`. Zero ends the loop.
pub unsafe extern "C" fn IOIteratorNext(iterator: IoObject) -> IoObject {
    ITERATORS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_mut(&iterator)
        .and_then(VecDeque::pop_front)
        .unwrap_or(0)
}

/// `boolean_t IOIteratorIsValid(io_iterator_t)`.
///
/// True, and that is not the same as "there is something in it": invalid means
/// the registry changed under the iteration and the caller must start again, so
/// answering false would put a program that checks into a loop.
pub unsafe extern "C" fn IOIteratorIsValid(_iterator: IoObject) -> u32 {
    1
}

pub unsafe extern "C" fn IOObjectRelease(object: IoObject) -> KernReturn {
    ITERATORS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&object);
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

fn property(device: &crate::hid::Device, key: &[u8]) -> CFTypeRef {
    let number = |value: i64| collections::new_number(collections::Number::Integer(value));
    match key {
        b"VendorID" => number(device.vendor as i64),
        b"ProductID" => number(device.product as i64),
        b"VersionNumber" => number(device.version as i64),
        b"PrimaryUsagePage" | b"DeviceUsagePage" => number(device.elements[0].page as i64),
        b"PrimaryUsage" | b"DeviceUsage" => number(device.elements[0].usage as i64),
        b"LocationID" | b"IOHIDLocationID" => number(device.id as i64),
        b"MaxInputReportSize" => number(device.max_input_report_size() as i64),
        b"ReportDescriptor" | b"IOHIDReportDescriptorKey" => {
            collections::new_data(device.report_descriptor())
        }
        b"DeviceUsagePairs" => device_usage_pairs(device),
        b"Product" | b"Manufacturer" => {
            corefoundation::string::from_bytes(device.name.as_bytes(), false)
        }
        b"Transport" => corefoundation::string::from_bytes(b"USB", false),
        b"bInterfaceNumber" => number(0),
        _ => std::ptr::null_mut(),
    }
}

fn device_usage_pairs(device: &crate::hid::Device) -> CFTypeRef {
    let page = corefoundation::string::from_bytes(b"DeviceUsagePage", false);
    let usage = corefoundation::string::from_bytes(b"DeviceUsage", false);
    let page_value =
        collections::new_number(collections::Number::Integer(device.elements[0].page as i64));
    let usage_value = collections::new_number(collections::Number::Integer(
        device.elements[0].usage as i64,
    ));
    let pair = collections::new_dictionary(
        &[(page, page_value), (usage, usage_value)],
        Policy::from_key_callbacks(),
        Policy::from_type_callbacks(),
        false,
    );
    // SAFETY: the dictionary retained its entries.
    unsafe {
        corefoundation::api::CFRelease(page);
        corefoundation::api::CFRelease(usage);
        corefoundation::api::CFRelease(page_value);
        corefoundation::api::CFRelease(usage_value);
    }
    let pairs = collections::new_array(&[pair], Policy::from_type_callbacks(), false);
    // SAFETY: the array retained the dictionary.
    unsafe { corefoundation::api::CFRelease(pair) };
    pairs
}

fn registry_properties(device: &crate::hid::Device) -> CFTypeRef {
    let entries = [
        (
            b"IOProviderClass".as_slice(),
            corefoundation::string::from_bytes(b"IOHIDDevice", false),
        ),
        (b"VendorID".as_slice(), property(device, b"VendorID")),
        (b"ProductID".as_slice(), property(device, b"ProductID")),
        (
            b"VersionNumber".as_slice(),
            property(device, b"VersionNumber"),
        ),
        (
            b"PrimaryUsagePage".as_slice(),
            property(device, b"PrimaryUsagePage"),
        ),
        (
            b"PrimaryUsage".as_slice(),
            property(device, b"PrimaryUsage"),
        ),
        (b"LocationID".as_slice(), property(device, b"LocationID")),
        (
            b"MaxInputReportSize".as_slice(),
            property(device, b"MaxInputReportSize"),
        ),
        (
            b"ReportDescriptor".as_slice(),
            property(device, b"ReportDescriptor"),
        ),
        (
            b"DeviceUsagePairs".as_slice(),
            property(device, b"DeviceUsagePairs"),
        ),
        (b"Product".as_slice(), property(device, b"Product")),
        (
            b"Manufacturer".as_slice(),
            property(device, b"Manufacturer"),
        ),
        (b"Transport".as_slice(), property(device, b"Transport")),
        (
            b"bInterfaceNumber".as_slice(),
            property(device, b"bInterfaceNumber"),
        ),
    ];
    let keys: Vec<_> = entries
        .iter()
        .map(|(key, _)| corefoundation::string::from_bytes(key, false))
        .collect();
    let pairs: Vec<_> = keys
        .iter()
        .zip(entries)
        .map(|(key, (_, value))| (*key, value))
        .collect();
    let properties = collections::new_dictionary(
        &pairs,
        Policy::from_key_callbacks(),
        Policy::from_type_callbacks(),
        false,
    );
    for key in keys {
        // SAFETY: the dictionary retained each key.
        unsafe { corefoundation::api::CFRelease(key) };
    }
    for (_, value) in entries {
        // SAFETY: the dictionary retained every property value.
        unsafe { corefoundation::api::CFRelease(value) };
    }
    properties
}

/// `CFTypeRef IORegistryEntryCreateCFProperty(entry, key, allocator, options)`.
pub unsafe extern "C" fn IORegistryEntryCreateCFProperty(
    entry: IoObject,
    key: CFTypeRef,
    _allocator: CFTypeRef,
    _options: u32,
) -> CFTypeRef {
    let key = unsafe { corefoundation::string::as_string(key) }
        .map(|s| s.utf8())
        .unwrap_or_default();
    let value = crate::api::device_for_service(entry)
        .map(|device| property(&device, &key))
        .unwrap_or(std::ptr::null_mut());
    crate::trace_hid(format_args!(
        "IORegistryEntryCreateCFProperty entry={entry} key={} -> {}",
        String::from_utf8_lossy(&key),
        !value.is_null()
    ));
    value
}

pub unsafe extern "C" fn IORegistryEntryCreateCFProperties(
    entry: IoObject,
    properties: *mut CFTypeRef,
    _allocator: CFTypeRef,
    _options: u32,
) -> KernReturn {
    let values = crate::api::device_for_service(entry).map(|device| registry_properties(&device));
    crate::trace_hid(format_args!(
        "IORegistryEntryCreateCFProperties entry={entry} -> {}",
        values.is_some()
    ));
    if let Some(values) = values {
        if !properties.is_null() {
            // SAFETY: an out parameter the caller sized.
            unsafe { properties.write(values) };
        }
        SUCCESS
    } else if !properties.is_null() {
        // SAFETY: an out parameter the caller sized.
        unsafe { properties.write(std::ptr::null_mut()) };
        NO_DEVICE
    } else {
        NO_DEVICE
    }
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

exports::exports! {
    functions {
        "IOServiceMatching" => IOServiceMatching,
        "IORegistryEntryIDMatching" => IORegistryEntryIDMatching,
        "IOServiceGetMatchingServices" => IOServiceGetMatchingServices,
        "IOServiceGetMatchingService" => IOServiceGetMatchingService,
        "IOIteratorNext" => IOIteratorNext,
        "IOIteratorIsValid" => IOIteratorIsValid,
        "IOObjectRelease" => IOObjectRelease,
        "IOObjectRetain" => IOObjectRetain,
        "IOObjectIsEqualTo" => IOObjectIsEqualTo,
        "IORegistryEntryFromPath" => IORegistryEntryFromPath,
        "IORegistryEntryGetParentEntry" => IORegistryEntryGetParentEntry,
        "IORegistryEntryGetRegistryEntryID" => IORegistryEntryGetRegistryEntryID,
        "IORegistryEntryCreateCFProperty" => IORegistryEntryCreateCFProperty,
        "IORegistryEntryCreateCFProperties" => IORegistryEntryCreateCFProperties,
        "IONotificationPortCreate" => IONotificationPortCreate,
        "IONotificationPortDestroy" => IONotificationPortDestroy,
        "IONotificationPortGetMachPort" => IONotificationPortGetMachPort,
        "IOServiceAddMatchingNotification" => IOServiceAddMatchingNotification,
        "IODispatchCalloutFromMessage" => IODispatchCalloutFromMessage,
    }
}
