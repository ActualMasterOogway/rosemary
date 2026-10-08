//! `IOHIDQueue`: a device's element changes, read one at a time by the caller.
//!
//! The queue is the polling half of HID input, where a value callback is the
//! pushing half; Unity's input backend reads its game controllers through one.
//! The device is sampled when the queue is read, and its elements' changes
//! since the last read are what come out, which is all a joystick's SDL state
//! can say.

#![allow(non_snake_case)]

use std::collections::VecDeque;
use std::ffi::c_void;
use std::sync::{Arc, Mutex};

use corefoundation::base::{CFIndex, CFTypeRef};

use crate::hid::Device;

struct State {
    /// The elements asked for, by cookie.
    cookies: Vec<u32>,
    started: bool,
    /// Each sampled element's last reading.
    last: Vec<i64>,
    pending: VecDeque<(u32, i64)>,
    depth: usize,
}

pub struct Queue {
    device: Arc<Device>,
    state: Mutex<State>,
}

/// The device behind a handle the guest got from this crate.
///
/// # Safety
///
/// `device` is null or an `IOHIDDeviceRef` this crate handed out, which live
/// for the process.
unsafe fn device_arc(device: CFTypeRef) -> Option<Arc<Device>> {
    if device.is_null() {
        return None;
    }
    let pointer = device as *const Device;
    // Every device is kept in an `Arc` for the process; take a new strong
    // count from the one held.
    Device::discover()
        .into_iter()
        .find(|d| std::ptr::eq(Arc::as_ptr(d), pointer))
}

/// # Safety
///
/// `queue` is null or a handle [`IOHIDQueueCreate`] returned.
unsafe fn queue<'a>(queue: CFTypeRef) -> Option<&'a Queue> {
    (!queue.is_null()).then(|| unsafe { &*(queue as *const Queue) })
}

/// `IOHIDQueueRef IOHIDQueueCreate(CFAllocatorRef, IOHIDDeviceRef, CFIndex depth,
/// IOOptionBits)`. Never freed: a queue lives as long as its device does here.
///
/// # Safety
///
/// `device` is null or a device this crate handed out.
pub unsafe extern "C" fn IOHIDQueueCreate(
    _allocator: CFTypeRef,
    device: CFTypeRef,
    depth: CFIndex,
    _options: u32,
) -> CFTypeRef {
    // SAFETY: the caller's guarantee.
    let Some(device) = (unsafe { device_arc(device) }) else {
        return std::ptr::null();
    };
    let elements = device.elements.len();
    let queue = Box::new(Queue {
        device,
        state: Mutex::new(State {
            cookies: Vec::new(),
            started: false,
            last: vec![i64::MIN; elements],
            pending: VecDeque::new(),
            depth: depth.max(1) as usize,
        }),
    });
    let handle = Box::into_raw(queue) as CFTypeRef;
    corefoundation::base::note_opaque(handle);
    handle
}

/// The cookie of an element handle, if it belongs to the queue's device.
///
/// # Safety
///
/// `element` is null or an element handle this crate handed out.
unsafe fn cookie_in(queue: &Queue, element: *const c_void) -> Option<u32> {
    // SAFETY: the caller's guarantee.
    let (device, cookie) = unsafe { crate::value::element(element) }?;
    std::ptr::eq(device, Arc::as_ptr(&queue.device)).then_some(cookie)
}

/// `void IOHIDQueueAddElement(IOHIDQueueRef, IOHIDElementRef)`
///
/// # Safety
///
/// Both are null or handles this crate handed out.
pub unsafe extern "C" fn IOHIDQueueAddElement(queue: CFTypeRef, element: *const c_void) {
    // SAFETY: the caller's guarantee.
    let Some(queue) = (unsafe { self::queue(queue) }) else {
        return;
    };
    // SAFETY: as above.
    let Some(cookie) = (unsafe { cookie_in(queue, element) }) else {
        return;
    };
    let mut state = queue.state.lock().unwrap_or_else(|e| e.into_inner());
    if !state.cookies.contains(&cookie) {
        state.cookies.push(cookie);
    }
}

/// `void IOHIDQueueRemoveElement(IOHIDQueueRef, IOHIDElementRef)`
///
/// # Safety
///
/// As [`IOHIDQueueAddElement`].
pub unsafe extern "C" fn IOHIDQueueRemoveElement(queue: CFTypeRef, element: *const c_void) {
    // SAFETY: the caller's guarantee.
    let Some(queue) = (unsafe { self::queue(queue) }) else {
        return;
    };
    // SAFETY: as above.
    let Some(cookie) = (unsafe { cookie_in(queue, element) }) else {
        return;
    };
    let mut state = queue.state.lock().unwrap_or_else(|e| e.into_inner());
    state.cookies.retain(|c| *c != cookie);
    state.pending.retain(|(c, _)| *c != cookie);
}

/// `Boolean IOHIDQueueContainsElement(IOHIDQueueRef, IOHIDElementRef)`
///
/// # Safety
///
/// As [`IOHIDQueueAddElement`].
pub unsafe extern "C" fn IOHIDQueueContainsElement(queue: CFTypeRef, element: *const c_void) -> u8 {
    // SAFETY: the caller's guarantee.
    let Some(queue) = (unsafe { self::queue(queue) }) else {
        return 0;
    };
    // SAFETY: as above.
    let Some(cookie) = (unsafe { cookie_in(queue, element) }) else {
        return 0;
    };
    let state = queue.state.lock().unwrap_or_else(|e| e.into_inner());
    state.cookies.contains(&cookie) as u8
}

/// `void IOHIDQueueStart(IOHIDQueueRef)`: changes from now on are queued.
///
/// # Safety
///
/// `queue` is null or a handle [`IOHIDQueueCreate`] returned.
pub unsafe extern "C" fn IOHIDQueueStart(queue: CFTypeRef) {
    // SAFETY: the caller's guarantee.
    let Some(queue) = (unsafe { self::queue(queue) }) else {
        return;
    };
    let mut state = queue.state.lock().unwrap_or_else(|e| e.into_inner());
    if state.started {
        return;
    }
    state.started = true;
    // What every element reads now is the baseline, not a change.
    let device = Arc::clone(&queue.device);
    for element in &device.elements {
        state.last[element.cookie as usize] = device.value(element);
    }
}

/// `void IOHIDQueueStop(IOHIDQueueRef)`
///
/// # Safety
///
/// `queue` is null or a handle [`IOHIDQueueCreate`] returned.
pub unsafe extern "C" fn IOHIDQueueStop(queue: CFTypeRef) {
    // SAFETY: the caller's guarantee.
    if let Some(queue) = unsafe { self::queue(queue) } {
        let mut state = queue.state.lock().unwrap_or_else(|e| e.into_inner());
        state.started = false;
        state.pending.clear();
    }
}

/// Move whatever changed since the last read into `pending`.
fn collect(device: &Device, state: &mut State) {
    for &cookie in &state.cookies.clone() {
        let Some(element) = device.element(cookie) else {
            continue;
        };
        let now = device.value(element);
        let slot = &mut state.last[cookie as usize];
        if *slot != now {
            *slot = now;
            state.pending.push_back((cookie, now));
        }
    }
    // A full queue drops its oldest, as the kernel's does.
    while state.pending.len() > state.depth.max(64) {
        state.pending.pop_front();
    }
}

/// `IOHIDValueRef IOHIDQueueCopyNextValueWithTimeout(IOHIDQueueRef, CFTimeInterval)`
///
/// The oldest queued change, owned by the caller, or null when there is none
/// within `timeout` seconds.
///
/// # Safety
///
/// `queue` is null or a handle [`IOHIDQueueCreate`] returned.
pub unsafe extern "C" fn IOHIDQueueCopyNextValueWithTimeout(
    queue: CFTypeRef,
    timeout: f64,
) -> CFTypeRef {
    // SAFETY: the caller's guarantee.
    let Some(queue) = (unsafe { self::queue(queue) }) else {
        return std::ptr::null();
    };
    let deadline =
        std::time::Instant::now() + std::time::Duration::from_secs_f64(timeout.clamp(0.0, 60.0));
    loop {
        {
            let mut state = queue.state.lock().unwrap_or_else(|e| e.into_inner());
            if !state.started {
                return std::ptr::null();
            }
            if state.pending.is_empty() {
                collect(&queue.device, &mut state);
            }
            if let Some((cookie, integer)) = state.pending.pop_front() {
                return crate::value::new(&queue.device, cookie, integer);
            }
        }
        if std::time::Instant::now() >= deadline {
            return std::ptr::null();
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}

/// `IOHIDValueRef IOHIDQueueCopyNextValue(IOHIDQueueRef)`, which does not wait.
///
/// # Safety
///
/// As [`IOHIDQueueCopyNextValueWithTimeout`].
pub unsafe extern "C" fn IOHIDQueueCopyNextValue(queue: CFTypeRef) -> CFTypeRef {
    // SAFETY: the caller's guarantee.
    unsafe { IOHIDQueueCopyNextValueWithTimeout(queue, 0.0) }
}

/// `Boolean IOHIDDeviceSetProperty(IOHIDDeviceRef, CFStringRef, CFTypeRef)`
///
/// No property a guest sets changes what a device here reports, so none is
/// accepted - which is what a real device answers for a key it does not take.
pub unsafe extern "C" fn IOHIDDeviceSetProperty(
    _device: CFTypeRef,
    _key: CFTypeRef,
    _value: CFTypeRef,
) -> u8 {
    0
}
