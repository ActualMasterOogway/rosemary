#![allow(non_snake_case)]

use std::ffi::c_void;
use std::sync::{Arc, Mutex};

use corefoundation::api as cf;
use corefoundation::base::{CFIndex, CFTypeRef};
use corefoundation::collections::{self, Number};

use crate::hid::Device;
use crate::manager::{Criterion, DeviceCallback, Manager, ValueCallback};
use crate::value;

type IOReturn = i32;
const SUCCESS: IOReturn = 0;
const ERROR: IOReturn = -536_870_212;

static MANAGERS: Mutex<Vec<Arc<Manager>>> = Mutex::new(Vec::new());

/// # Safety
///
/// `manager` must be null or a pointer this module handed out.
unsafe fn manager<'a>(manager: CFTypeRef) -> Option<&'a Manager> {
    // SAFETY: the caller's obligation; managers are never freed.
    (!manager.is_null()).then(|| unsafe { &*(manager as *const Manager) })
}

/// # Safety
///
/// `device` must be null or an `IOHIDDeviceRef` this module handed out.
unsafe fn device<'a>(device: CFTypeRef) -> Option<&'a Device> {
    // SAFETY: the caller's obligation.
    (!device.is_null()).then(|| unsafe { &*(device as *const Device) })
}

pub unsafe extern "C" fn IOHIDManagerCreate(_allocator: CFTypeRef, _options: u32) -> CFTypeRef {
    let manager = Arc::new(Manager::new());
    let handle = Arc::as_ptr(&manager) as CFTypeRef;
    // A guest may `CFRetain` it, and this handle has no isa to retain through.
    corefoundation::base::note_opaque(handle);
    MANAGERS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(manager);
    handle
}

pub unsafe extern "C" fn IOHIDManagerOpen(manager: CFTypeRef, _options: u32) -> IOReturn {
    // SAFETY: the caller's obligation.
    match unsafe { self::manager(manager) }.is_some_and(|m| m.open()) {
        true => SUCCESS,
        false => ERROR,
    }
}

pub unsafe extern "C" fn IOHIDManagerClose(manager: CFTypeRef, _options: u32) -> IOReturn {
    // SAFETY: the caller's obligation.
    if let Some(m) = unsafe { self::manager(manager) } {
        m.close();
    }
    SUCCESS
}

/// Read one matching dictionary into a [`Criterion`].
///
/// # Safety
///
/// `dictionary` must be null or a `CFDictionaryRef`.
unsafe fn criterion_of(dictionary: CFTypeRef) -> Criterion {
    // SAFETY: the caller's obligation.
    unsafe fn number_for(dictionary: CFTypeRef, key: &std::ffi::CStr) -> Option<u32> {
        let name = corefoundation::string::from_bytes(key.to_bytes(), false);
        // SAFETY: the caller's obligation, and a string this call just made.
        let found = unsafe { cf::CFDictionaryGetValue(dictionary, name) };
        // SAFETY: as above.
        unsafe { cf::CFRelease(name) };
        // SAFETY: as above.
        unsafe { collections::number(found) }.map(|n| n.as_i64() as u32)
    }
    // SAFETY: the caller's obligation.
    unsafe {
        Criterion {
            page: number_for(dictionary, c"DeviceUsagePage"),
            usage: number_for(dictionary, c"DeviceUsage"),
        }
    }
}

pub unsafe extern "C" fn IOHIDManagerSetDeviceMatching(manager: CFTypeRef, matching: CFTypeRef) {
    // SAFETY: the caller's obligation.
    let Some(m) = (unsafe { self::manager(manager) }) else {
        return;
    };
    // Null means "match everything", which is what an empty criteria list is.
    let criteria = match matching.is_null() {
        true => Vec::new(),
        // SAFETY: as above.
        false => vec![unsafe { criterion_of(matching) }],
    };
    m.set_criteria(criteria);
}

pub unsafe extern "C" fn IOHIDManagerSetDeviceMatchingMultiple(
    manager: CFTypeRef,
    matching: CFTypeRef,
) {
    // SAFETY: the caller's obligation.
    let Some(m) = (unsafe { self::manager(manager) }) else {
        return;
    };
    // SAFETY: as above - a `CFArrayRef` of dictionaries, or null.
    let count = unsafe { cf::CFArrayGetCount(matching) };
    let mut criteria = Vec::new();
    for i in 0..count {
        // SAFETY: as above.
        let entry = unsafe { cf::CFArrayGetValueAtIndex(matching, i) };
        // SAFETY: as above.
        criteria.push(unsafe { criterion_of(entry) });
    }
    m.set_criteria(criteria);
}

pub unsafe extern "C" fn IOHIDManagerRegisterDeviceMatchingCallback(
    manager: CFTypeRef,
    callback: Option<DeviceCallback>,
    context: *mut c_void,
) {
    // SAFETY: the caller's obligation.
    if let Some(m) = unsafe { self::manager(manager) } {
        m.set_matched(callback, context as usize);
    }
}

pub unsafe extern "C" fn IOHIDManagerRegisterDeviceRemovalCallback(
    manager: CFTypeRef,
    callback: Option<DeviceCallback>,
    context: *mut c_void,
) {
    // SAFETY: the caller's obligation.
    if let Some(m) = unsafe { self::manager(manager) } {
        m.set_removed(callback, context as usize);
    }
}

pub unsafe extern "C" fn IOHIDDeviceRegisterInputValueCallback(
    device: CFTypeRef,
    callback: Option<ValueCallback>,
    context: *mut c_void,
) {
    let _ = device;
    for m in MANAGERS.lock().unwrap_or_else(|e| e.into_inner()).iter() {
        m.set_values(callback, context as usize);
    }
}

pub unsafe extern "C" fn IOHIDManagerScheduleWithRunLoop(
    manager: CFTypeRef,
    run_loop: CFTypeRef,
    mode: CFTypeRef,
) {
    // SAFETY: the caller's obligation.
    let Some(m) = (unsafe { self::manager(manager) }) else {
        return;
    };
    schedule(m as *const Manager as usize, run_loop, mode);
}

pub unsafe extern "C" fn IOHIDManagerUnscheduleFromRunLoop(
    _manager: CFTypeRef,
    _run_loop: CFTypeRef,
    _mode: CFTypeRef,
) {
    // no-op: The source stays and finds no manager to poll once the
    // manager is closed, which is the same observable behaviour and avoids a
    // registry of sources keyed on a pair.
}

fn schedule(manager: usize, run_loop: CFTypeRef, mode: CFTypeRef) {
    unsafe extern "C" fn poll_one(info: *mut c_void) {
        // SAFETY: `info` is a `&Manager`'s address, and managers are never freed.
        if let Some(m) = unsafe { (info as *const Manager).as_ref() } {
            m.poll();
        }
    }
    unsafe extern "C" fn wake(_timer: CFTypeRef, info: *mut c_void) {
        // SAFETY: as above.
        if let Some(m) = unsafe { (info as *const Manager).as_ref() } {
            m.poll();
        }
    }

    let context = cf::CFRunLoopSourceContext {
        version: 0,
        info: manager as *mut c_void,
        retain: std::ptr::null(),
        release: std::ptr::null(),
        copy_description: std::ptr::null(),
        equal: std::ptr::null(),
        hash: std::ptr::null(),
        schedule: std::ptr::null(),
        cancel: std::ptr::null(),
        perform: Some(poll_one),
    };
    let mut timer_context = cf::CFRunLoopTimerContext {
        version: 0,
        info: manager as *mut c_void,
        retain: std::ptr::null(),
        release: std::ptr::null(),
        copy_description: std::ptr::null(),
    };
    // SAFETY: both contexts are this function's own and outlive the calls, which
    // copy what they need.
    unsafe {
        let source = cf::CFRunLoopSourceCreate(std::ptr::null(), 0, &context);
        cf::CFRunLoopAddSource(run_loop, source, mode);
        cf::CFRunLoopSourceSignal(source);

        let timer = cf::CFRunLoopTimerCreate(
            std::ptr::null(),
            cf::CFAbsoluteTimeGetCurrent(),
            1.0 / 60.0,
            0,
            0,
            Some(wake),
            &mut timer_context,
        );
        cf::CFRunLoopAddTimer(run_loop, timer, mode);
    }
}

/// `IOHIDDeviceGetProperty`, which is where a guest's GUID comes from.
///
/// The runner builds the GUID `SGamepadMapping::FindFromGUID` matches on out of
/// the vendor, product and version keys - so these are answered from SDL's own
/// USB identifiers rather than from anything SDL derived. Feeding it
/// `SDL_GetJoystickGUID` would be the wrong direction: that string is *made
/// from* these numbers.
pub unsafe extern "C" fn IOHIDDeviceGetProperty(device: CFTypeRef, key: CFTypeRef) -> CFTypeRef {
    // SAFETY: the caller's obligation.
    let Some(d) = (unsafe { self::device(device) }) else {
        return std::ptr::null();
    };
    // SAFETY: as above - a `CFStringRef` or null.
    let key = unsafe { corefoundation::string::as_string(key) }
        .map(|s| s.utf8())
        .unwrap_or_default();
    let number = |value: i64| {
        let n = collections::new_number(Number::Integer(value));
        // SAFETY: a fresh object handed straight back; the caller owns nothing.
        unsafe { cf::CFAutorelease(n) }
    };
    match key.as_slice() {
        b"VendorID" => number(d.vendor as i64),
        b"ProductID" => number(d.product as i64),
        b"VersionNumber" => number(d.version as i64),
        b"PrimaryUsagePage" => number(d.elements[0].page as i64),
        b"PrimaryUsage" => number(d.elements[0].usage as i64),
        b"Product" | b"Manufacturer" => {
            let s = corefoundation::string::from_bytes(d.name.as_bytes(), false);
            // SAFETY: as above.
            unsafe { cf::CFAutorelease(s) }
        }
        // A key this runtime does not answer is null, which is what a real
        // device that does not report it gives - and what a caller checks for.
        _ => std::ptr::null(),
    }
}

/// `IOHIDDeviceCopyMatchingElements`, which a guest walks to build its mapping.
///
/// The matching dictionary is ignored: every guest in this tree passes null,
/// which means "all of them", and a filter nobody exercises is a filter nobody
/// has checked.
pub unsafe extern "C" fn IOHIDDeviceCopyMatchingElements(
    device: CFTypeRef,
    _matching: CFTypeRef,
    _options: u32,
) -> CFTypeRef {
    // SAFETY: the caller's obligation.
    let Some(d) = (unsafe { self::device(device) }) else {
        return std::ptr::null();
    };
    let handles: Vec<CFTypeRef> = d
        .elements
        .iter()
        .map(|e| d.handle(e.cookie) as CFTypeRef)
        .collect();
    // The elements are not CoreFoundation objects, so the array holds them with
    // no callbacks: nothing to retain, nothing to release, and identity by
    // pointer - which is what a guest comparing two element references wants.
    collections::new_array(&handles, corefoundation::callbacks::Policy::NONE, false)
}

pub unsafe extern "C" fn IOHIDDeviceOpen(_device: CFTypeRef, _options: u32) -> IOReturn {
    // The device was opened when it was matched; there is nothing left to do.
    SUCCESS
}

pub unsafe extern "C" fn IOHIDDeviceClose(_device: CFTypeRef, _options: u32) -> IOReturn {
    SUCCESS
}

/// Read one field of an element.
macro_rules! element_field {
    ($(#[$meta:meta])* $name:ident, $ty:ty, $read:expr, $default:expr) => {
        $(#[$meta])*
        pub unsafe extern "C" fn $name(element: *const c_void) -> $ty {
            // SAFETY: the caller's obligation - an `IOHIDElementRef` this
            // module handed out, whose device is still open.
            match unsafe { value::element(element) }
                .and_then(|(device, cookie)| device.element(cookie))
            {
                Some(e) => $read(e),
                None => $default,
            }
        }
    };
}

element_field!(/// The element's opaque identity, which a guest stores and looks up.
    IOHIDElementGetCookie, u32, |e: &crate::hid::Element| e.cookie, 0);
element_field!(/// `kHIDPage_GenericDesktop`, `kHIDPage_Button`, and so on.
    IOHIDElementGetUsagePage, u32, |e: &crate::hid::Element| e.page, 0);
element_field!(/// Which control on that page.
    IOHIDElementGetUsage, u32, |e: &crate::hid::Element| e.usage, 0);
element_field!(/// Axis, button, hat or collection.
    IOHIDElementGetType, u32, |e: &crate::hid::Element| e.kind, 0);
element_field!(/// The lowest value this element reports.
    IOHIDElementGetLogicalMin, CFIndex, |e: &crate::hid::Element| e.logical_min as CFIndex, 0);
element_field!(/// The highest.
    IOHIDElementGetLogicalMax, CFIndex, |e: &crate::hid::Element| e.logical_max as CFIndex, 0);

/// `IOHIDElementGetChildren`: everything under a collection.
///
/// Only the root collection has children here, and its children are every other
/// element - which is the shape a guest walking from the top expects, and the
/// shape a real gamepad's report descriptor mostly has.
pub unsafe extern "C" fn IOHIDElementGetChildren(element: *const c_void) -> CFTypeRef {
    // SAFETY: the caller's obligation.
    let Some((device, cookie)) = (unsafe { value::element(element) }) else {
        return std::ptr::null();
    };
    if cookie != 0 {
        return std::ptr::null();
    }
    let handles: Vec<CFTypeRef> = device
        .elements
        .iter()
        .skip(1)
        .map(|e| device.handle(e.cookie) as CFTypeRef)
        .collect();
    collections::new_array(&handles, corefoundation::callbacks::Policy::NONE, false)
}

pub unsafe extern "C" fn IOHIDValueGetElement(v: CFTypeRef) -> *const c_void {
    // SAFETY: the caller's obligation - a value handed to a callback, read
    // during that callback.
    let Some(v) = (unsafe { value::borrow(v) }) else {
        return std::ptr::null();
    };
    // SAFETY: as above.
    match unsafe { value::device(v) } {
        Some(device) => device.handle(v.cookie),
        None => std::ptr::null(),
    }
}

pub unsafe extern "C" fn IOHIDValueGetIntegerValue(v: CFTypeRef) -> CFIndex {
    // SAFETY: as above.
    unsafe { value::borrow(v) }.map_or(0, |v| v.integer as CFIndex)
}

/// The width of the value in bytes, which for everything here is one word.
pub unsafe extern "C" fn IOHIDValueGetLength(v: CFTypeRef) -> CFIndex {
    // SAFETY: as above.
    match unsafe { value::borrow(v) } {
        Some(_) => size_of::<CFIndex>() as CFIndex,
        None => 0,
    }
}

/// Unused, but part of the shape: a caller may ask for a scaled reading.
pub unsafe extern "C" fn IOHIDValueGetScaledValue(v: CFTypeRef, _kind: u32) -> f64 {
    // SAFETY: as above.
    unsafe { value::borrow(v) }.map_or(0.0, |v| v.integer as f64)
}

/// Keys a guest builds a matching dictionary from, exported as data.
///
/// `CFStringRef` globals, so the same object every time - a guest stores one and
/// uses it as a dictionary key, and two different objects would not match.
pub fn key_strings() -> Vec<(&'static str, &'static str)> {
    vec![
        ("_kIOHIDDeviceUsagePageKey", "DeviceUsagePage"),
        ("_kIOHIDDeviceUsageKey", "DeviceUsage"),
        ("_kIOHIDVendorIDKey", "VendorID"),
        ("_kIOHIDProductIDKey", "ProductID"),
        ("_kIOHIDVersionNumberKey", "VersionNumber"),
        ("_kIOHIDPrimaryUsagePageKey", "PrimaryUsagePage"),
        ("_kIOHIDPrimaryUsageKey", "PrimaryUsage"),
        ("_kIOHIDProductKey", "Product"),
        ("_kIOHIDManufacturerKey", "Manufacturer"),
    ]
}
