use std::collections::BTreeMap;
use std::ffi::c_void;
use std::sync::{Arc, Mutex};

use corefoundation::base::CFTypeRef;

use crate::hid::Device;

/// `IOHIDDeviceCallback`: `(context, result, sender, device)`.
pub type DeviceCallback = unsafe extern "C" fn(*mut c_void, i32, *mut c_void, CFTypeRef);
/// `IOHIDValueCallback`: `(context, result, sender, value)`.
pub type ValueCallback = unsafe extern "C" fn(*mut c_void, i32, *mut c_void, CFTypeRef);

/// One matching criterion, as a dictionary a guest built.
///
/// Only the two keys anything uses: a page and a usage. A criterion naming
/// something else matches nothing rather than everything - a manager that
/// reported every device to a guest asking for gamepads would have it treat a
/// keyboard as one.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Criterion {
    pub page: Option<u32>,
    pub usage: Option<u32>,
}

impl Criterion {
    /// Whether a device whose root collection is `(page, usage)` matches.
    pub fn matches(&self, page: u32, usage: u32) -> bool {
        self.page.is_none_or(|p| p == page) && self.usage.is_none_or(|u| u == usage)
    }
}

/// A registered callback and the context to hand it back.
#[derive(Clone, Copy)]
struct Registered<T> {
    callback: Option<T>,
    context: usize,
}

impl<T> Default for Registered<T> {
    fn default() -> Self {
        Registered {
            callback: None,
            context: 0,
        }
    }
}

/// An `IOHIDManagerRef`.
pub struct Manager {
    criteria: Mutex<Vec<Criterion>>,
    devices: Mutex<BTreeMap<sdl3::JoystickId, Arc<Device>>>,
    matched: Mutex<Registered<DeviceCallback>>,
    removed: Mutex<Registered<DeviceCallback>>,
    values: Mutex<Registered<ValueCallback>>,
    open: Mutex<bool>,
}

// SAFETY: every field is behind a lock, and the contexts are opaque numbers this
// crate hands back rather than dereferences.
unsafe impl Send for Manager {}
unsafe impl Sync for Manager {}

impl Manager {
    pub fn new() -> Manager {
        Manager {
            criteria: Mutex::new(Vec::new()),
            devices: Mutex::new(BTreeMap::new()),
            matched: Mutex::new(Registered::default()),
            removed: Mutex::new(Registered::default()),
            values: Mutex::new(Registered::default()),
            open: Mutex::new(false),
        }
    }

    pub fn set_criteria(&self, criteria: Vec<Criterion>) {
        *self.criteria.lock().unwrap_or_else(|e| e.into_inner()) = criteria;
    }

    pub fn set_matched(&self, callback: Option<DeviceCallback>, context: usize) {
        *self.matched.lock().unwrap_or_else(|e| e.into_inner()) = Registered { callback, context };
    }

    pub fn set_removed(&self, callback: Option<DeviceCallback>, context: usize) {
        *self.removed.lock().unwrap_or_else(|e| e.into_inner()) = Registered { callback, context };
    }

    pub fn set_values(&self, callback: Option<ValueCallback>, context: usize) {
        *self.values.lock().unwrap_or_else(|e| e.into_inner()) = Registered { callback, context };
    }

    pub fn open(&self) -> bool {
        if sdl3::init(sdl3::init::JOYSTICK).is_err() {
            return false;
        }
        *self.open.lock().unwrap_or_else(|e| e.into_inner()) = true;
        true
    }

    pub fn close(&self) {
        *self.open.lock().unwrap_or_else(|e| e.into_inner()) = false;
    }

    /// Whether `device` matches any criterion, or all of them if there are none.
    fn accepts(&self, device: &Device) -> bool {
        let criteria = self.criteria.lock().unwrap_or_else(|e| e.into_inner());
        if criteria.is_empty() {
            return true;
        }
        let root = &device.elements[0];
        criteria.iter().any(|c| c.matches(root.page, root.usage))
    }

    pub fn device(&self, id: sdl3::JoystickId) -> Option<Arc<Device>> {
        self.devices
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&id)
            .cloned()
    }

    /// One turn: notice what appeared and went, and report what moved.
    ///
    /// Called from a run loop source, which is where a guest expects its
    /// callbacks. Every lock is released before a callback runs - a callback is
    /// entitled to ask the manager a question, and holding a lock across one is
    /// a deadlock the first time it does.
    pub fn poll(&self) {
        if !*self.open.lock().unwrap_or_else(|e| e.into_inner()) {
            return;
        }
        sdl3::pump();

        let attached = sdl3::joysticks();
        let mut appeared = Vec::new();
        let mut vanished = Vec::new();
        {
            let mut devices = self.devices.lock().unwrap_or_else(|e| e.into_inner());
            for id in &attached {
                if devices.contains_key(id) {
                    continue;
                }
                let Some(device) = Device::open(*id) else {
                    continue;
                };
                if !self.accepts(&device) {
                    continue;
                }
                devices.insert(*id, Arc::clone(&device));
                appeared.push(device);
            }
            let gone: Vec<sdl3::JoystickId> = devices
                .keys()
                .copied()
                .filter(|id| !attached.contains(id))
                .collect();
            for id in gone {
                if let Some(device) = devices.remove(&id) {
                    vanished.push(device);
                }
            }
        }

        for device in appeared {
            let registered = *self.matched.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(callback) = registered.callback {
                // SAFETY: the callback is a `void (*)(void *, IOReturn, void *,
                // IOHIDDeviceRef)` the guest registered - a trampoline, by the
                // time it reaches here - and the device pointer is the one every
                // other call in this crate takes.
                unsafe {
                    callback(
                        registered.context as *mut c_void,
                        0,
                        std::ptr::null_mut(),
                        Arc::as_ptr(&device) as CFTypeRef,
                    )
                };
            }
        }
        for device in vanished {
            let registered = *self.removed.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(callback) = registered.callback {
                // SAFETY: as above.
                unsafe {
                    callback(
                        registered.context as *mut c_void,
                        0,
                        std::ptr::null_mut(),
                        Arc::as_ptr(&device) as CFTypeRef,
                    )
                };
            }
        }

        // And what moved, one callback per element per change
        let devices: Vec<Arc<Device>> = self
            .devices
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .cloned()
            .collect();
        let registered = *self.values.lock().unwrap_or_else(|e| e.into_inner());
        let Some(callback) = registered.callback else {
            return;
        };
        for device in devices {
            for (cookie, value) in device.changed() {
                let boxed = crate::value::new(&device, cookie, value);
                // SAFETY: the callback is the guest's, and `boxed` is an
                // `IOHIDValueRef` this crate made and keeps alive for the call.
                unsafe {
                    callback(
                        registered.context as *mut c_void,
                        0,
                        Arc::as_ptr(&device) as *mut c_void,
                        boxed,
                    )
                };
                crate::value::release(boxed);
            }
        }
    }
}

impl Default for Manager {
    fn default() -> Self {
        Manager::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hid::{page, usage};

    #[test]
    fn a_criterion_matches_on_the_keys_it_names_and_ignores_the_rest() {
        let gamepad = Criterion {
            page: Some(page::GENERIC_DESKTOP),
            usage: Some(usage::GAMEPAD),
        };
        assert!(gamepad.matches(page::GENERIC_DESKTOP, usage::GAMEPAD));
        assert!(!gamepad.matches(page::GENERIC_DESKTOP, usage::JOYSTICK));
        assert!(!gamepad.matches(page::BUTTON, usage::GAMEPAD));

        // A criterion that names only a page matches every usage on it, which
        // is what a guest asking for "anything on the desktop page" means.
        let any_desktop = Criterion {
            page: Some(page::GENERIC_DESKTOP),
            usage: None,
        };
        assert!(any_desktop.matches(page::GENERIC_DESKTOP, usage::JOYSTICK));
        assert!(!any_desktop.matches(page::BUTTON, 1));
    }
}
