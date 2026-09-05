//! A device this process invents, read the way a guest reads one.
//!
//! What this would falsify: that `libYoYoGamepad`'s sequence works at all -
//! make a manager, hand it matching criteria, schedule it on a run loop, and be
//! called back per device and per element. Every step here is the guest's, done
//! from Rust so a failure lands in one place rather than four.
//!
//! Deterministic and headless: the device is `SDL_AttachVirtualJoystick`'s, so
//! this needs no hardware and runs in a container.

use std::ffi::c_void;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use corefoundation::api as cf;
use corefoundation::base::CFTypeRef;
use iokit::api;
use iokit::hid::{element_type, page, usage};

static MATCHED: AtomicUsize = AtomicUsize::new(0);
static REMOVED: AtomicUsize = AtomicUsize::new(0);
static DEVICE: Mutex<usize> = Mutex::new(0);
static VALUES: Mutex<Vec<(u32, i64)>> = Mutex::new(Vec::new());

unsafe extern "C" fn matched(
    _context: *mut c_void,
    _result: i32,
    _sender: *mut c_void,
    device: CFTypeRef,
) {
    MATCHED.fetch_add(1, Ordering::SeqCst);
    *DEVICE.lock().unwrap_or_else(|e| e.into_inner()) = device as usize;
}

unsafe extern "C" fn changed(
    _context: *mut c_void,
    _result: i32,
    _sender: *mut c_void,
    value: CFTypeRef,
) {
    // SAFETY: a value the manager made for this call and releases after it.
    unsafe {
        let element = api::IOHIDValueGetElement(value);
        let cookie = api::IOHIDElementGetCookie(element);
        let integer = api::IOHIDValueGetIntegerValue(value) as i64;
        VALUES
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((cookie, integer));
    }
}

unsafe extern "C" fn removed(_context: *mut c_void, _result: i32, _sender: *mut c_void) {
    REMOVED.fetch_add(1, Ordering::SeqCst);
}

fn string(text: &[u8]) -> CFTypeRef {
    corefoundation::string::from_bytes(text, false)
}

/// Turn the run loop until `done`, or give up.
fn pump_until(done: impl Fn() -> bool) {
    for _ in 0..100 {
        if done() {
            return;
        }
        // SAFETY: a mode name and a timeout.
        unsafe { cf::CFRunLoopRunInMode(string(b"kCFRunLoopDefaultMode"), 0.02, 0) };
    }
}

/// "A gamepad on the generic-desktop page", built the way a guest builds one.
fn gamepad_criteria() -> CFTypeRef {
    let number = |v: i64| {
        corefoundation::collections::new_number(corefoundation::collections::Number::Integer(v))
    };
    let keys = [string(b"DeviceUsagePage"), string(b"DeviceUsage")];
    let values = [
        number(page::GENERIC_DESKTOP as i64),
        number(usage::GAMEPAD as i64),
    ];
    // SAFETY: real objects and this tree's own type callbacks.
    unsafe {
        let dictionary = cf::CFDictionaryCreate(
            std::ptr::null(),
            keys.as_ptr(),
            values.as_ptr(),
            2,
            &corefoundation::callbacks::TYPE_KEY_CALLBACKS,
            &corefoundation::callbacks::TYPE_VALUE_CALLBACKS,
        );
        cf::CFArrayCreate(
            std::ptr::null(),
            [dictionary].as_ptr(),
            1,
            &corefoundation::callbacks::TYPE_VALUE_CALLBACKS,
        )
    }
}

#[test]
fn a_guest_reads_a_gamepad_through_the_run_loop() {
    if !sdl3::available() || !sdl3::virtual_joystick::available() {
        eprintln!("skipped: this host has no SDL3 with a virtual joystick");
        return;
    }
    corefoundation::bootstrap();
    let pool = unsafe { objc::api::objc_autoreleasePoolPush() };

    let label = c"rosetta3 test pad";
    let desc = sdl3::virtual_joystick::Desc {
        // `SDL_JOYSTICK_TYPE_GAMEPAD`, so SDL reports it as one.
        kind: 1,
        vendor_id: 0x045e,
        product_id: 0x028e,
        naxes: 6,
        nbuttons: 15,
        nhats: 1,
        name: label.as_ptr(),
        ..Default::default()
    };
    sdl3::init(sdl3::init::JOYSTICK).expect("the joystick subsystem");
    let Some(id) = sdl3::virtual_joystick::attach(&desc) else {
        panic!("SDL declined a virtual joystick: {}", sdl3::error());
    };

    // The guest's sequence, in the guest's order.
    // SAFETY: every argument is an object built just above, and the callbacks
    // are this file's own.
    let manager;
    unsafe {
        manager = api::IOHIDManagerCreate(std::ptr::null(), 0);
        api::IOHIDManagerSetDeviceMatchingMultiple(manager, gamepad_criteria());
        api::IOHIDManagerRegisterDeviceMatchingCallback(
            manager,
            Some(matched),
            std::ptr::null_mut(),
        );
        api::IOHIDManagerRegisterInputValueCallback(manager, Some(changed), std::ptr::null_mut());
        assert_eq!(api::IOHIDManagerOpen(manager, 0), 0, "the manager opens");
        api::IOHIDManagerScheduleWithRunLoop(
            manager,
            cf::CFRunLoopGetCurrent(),
            string(b"kCFRunLoopDefaultMode"),
        );
    }

    pump_until(|| MATCHED.load(Ordering::SeqCst) > 0);
    assert_eq!(
        MATCHED.load(Ordering::SeqCst),
        1,
        "the matching callback did not see the device"
    );
    let device = *DEVICE.lock().unwrap_or_else(|e| e.into_inner()) as CFTypeRef;
    assert!(!device.is_null());
    unsafe {
        let devices = api::IOHIDManagerCopyDevices(manager);
        assert_eq!(cf::CFGetTypeID(devices), cf::CFSetGetTypeID());
        assert_eq!(cf::CFSetGetCount(devices), 1);
        let mut copied = std::ptr::null();
        cf::CFSetGetValues(devices, &mut copied);
        assert_eq!(copied, device);
        cf::CFRelease(devices);
    }
    unsafe {
        api::IOHIDDeviceRegisterRemovalCallback(device, Some(removed), std::ptr::null_mut());
    }

    // The properties the runner builds its GUID from, which is what
    // `SGamepadMapping::FindFromGUID` matches on.
    // SAFETY: a device the manager just reported.
    unsafe {
        let read = |key: &[u8]| {
            corefoundation::collections::number(api::IOHIDDeviceGetProperty(device, string(key)))
                .map(|v| v.as_i64())
                .unwrap_or(-1)
        };
        assert_eq!(read(b"VendorID"), 0x045e, "the vendor id is SDL's own");
        assert_eq!(read(b"ProductID"), 0x028e);
    }

    // The element tree, walked the way a guest walks it.
    // SAFETY: as above.
    unsafe {
        let elements = api::IOHIDDeviceCopyMatchingElements(device, std::ptr::null(), 0);
        let count = cf::CFArrayGetCount(elements);
        assert_eq!(count, 1 + 6 + 1 + 15, "the element tree is the wrong shape");

        // Apps can reject every array member whose general CF type does not
        // equal IOKit's element type before it reads any HID fields.
        for i in 0..count {
            let element = cf::CFArrayGetValueAtIndex(elements, i);
            assert_eq!(cf::CFGetTypeID(element), api::IOHIDElementGetTypeID());
        }

        // The first axis is X on the generic-desktop page, over SDL's own range.
        let first_axis = cf::CFArrayGetValueAtIndex(elements, 1) as *const c_void;
        assert_eq!(
            api::IOHIDElementGetUsagePage(first_axis),
            page::GENERIC_DESKTOP
        );
        assert_eq!(api::IOHIDElementGetUsage(first_axis), usage::X);
        assert_eq!(
            api::IOHIDElementGetType(first_axis),
            element_type::INPUT_AXIS
        );
        assert_eq!(api::IOHIDElementGetLogicalMin(first_axis), -32768);
        assert_eq!(api::IOHIDElementGetLogicalMax(first_axis), 32767);

        // Apps may poll mapped controls with IOHIDDeviceGetValue rather than
        // registering the manager-level value callback.
        let mut value = std::ptr::null();
        assert_eq!(api::IOHIDDeviceGetValue(device, first_axis, &mut value), 0);
        assert!(!value.is_null());
        assert_eq!(cf::CFGetTypeID(value), api::IOHIDValueGetTypeID());
        assert_eq!(api::IOHIDValueGetElement(value), first_axis);

        // And the root collection's children are everything else.
        let root = cf::CFArrayGetValueAtIndex(elements, 0) as *const c_void;
        assert_eq!(
            cf::CFArrayGetCount(api::IOHIDElementGetChildren(root)),
            count - 1
        );
    }

    // Move it, and see the changes arrive as element callbacks.
    let joystick = sdl3::open(id).expect("the joystick opens");
    VALUES.lock().unwrap_or_else(|e| e.into_inner()).clear();
    // SAFETY: an open virtual joystick.
    unsafe {
        assert!(sdl3::virtual_joystick::set_axis(joystick, 0, 20_000));
        assert!(sdl3::virtual_joystick::set_button(joystick, 2, true));
    }
    pump_until(|| VALUES.lock().unwrap_or_else(|e| e.into_inner()).len() >= 2);

    let values = VALUES.lock().unwrap_or_else(|e| e.into_inner()).clone();
    assert!(
        values.contains(&(1, 20_000)),
        "the axis did not arrive as element 1: {values:?}"
    );
    // Element 0 is the collection, 1..=6 the axes, 7 the hat, 8.. the buttons -
    // so SDL's button 2 is element 10.
    assert!(
        values.contains(&(10, 1)),
        "the button did not arrive as element 10: {values:?}"
    );

    // SAFETY: a joystick this test opened.
    unsafe { sdl3::close(joystick) };
    sdl3::virtual_joystick::detach(id);
    pump_until(|| REMOVED.load(Ordering::SeqCst) > 0);
    assert_eq!(REMOVED.load(Ordering::SeqCst), 1);
    unsafe { objc::api::objc_autoreleasePoolPop(pool) };
}
