use std::collections::BTreeMap;
use std::ffi::c_void;
use std::sync::atomic::AtomicU8;
use std::sync::{Arc, Mutex};

use corefoundation::base::{CFTypeID, CFTypeRef};

use crate::hid::Device;

static MANAGER_TYPE: AtomicU8 = AtomicU8::new(0);

pub fn type_id() -> CFTypeID {
    std::ptr::addr_of!(MANAGER_TYPE) as CFTypeID
}

/// `IOHIDDeviceCallback`: `(context, result, sender, device)`.
pub type DeviceCallback = unsafe extern "C" fn(*mut c_void, i32, *mut c_void, CFTypeRef);
/// `IOHIDValueCallback`: `(context, result, sender, value)`.
pub type ValueCallback = unsafe extern "C" fn(*mut c_void, i32, *mut c_void, CFTypeRef);

/// One matching criterion, as a dictionary a guest built.
///
/// The usage pair selects a device class; vendor and product support the
/// narrower enumeration filter used by HIDAPI.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Criterion {
    pub page: Option<u32>,
    pub usage: Option<u32>,
    pub vendor: Option<u32>,
    pub product: Option<u32>,
}

impl Criterion {
    /// Whether a device whose root collection is `(page, usage)` matches.
    pub fn matches(&self, page: u32, usage: u32, vendor: u32, product: u32) -> bool {
        self.page.is_none_or(|p| p == page)
            && self.usage.is_none_or(|u| u == usage)
            && self.vendor.is_none_or(|v| v == vendor)
            && self.product.is_none_or(|p| p == product)
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

/// An `IOHIDManagerRef` with independent matching and delivery history.
pub struct Manager {
    criteria: Mutex<Vec<Criterion>>,
    devices: Mutex<BTreeMap<sdl3::JoystickId, Arc<Device>>>,
    last: Mutex<BTreeMap<sdl3::JoystickId, Vec<i64>>>,
    matched: Mutex<Registered<DeviceCallback>>,
    removed: Mutex<Registered<DeviceCallback>>,
    values: Mutex<Registered<ValueCallback>>,
    open: std::sync::atomic::AtomicBool,
    polling: std::sync::atomic::AtomicBool,
    pub(crate) schedules: crate::schedule::Schedules,
}

impl Manager {
    pub fn new() -> Manager {
        Manager {
            criteria: Mutex::new(Vec::new()),
            devices: Mutex::new(BTreeMap::new()),
            last: Mutex::new(BTreeMap::new()),
            matched: Mutex::new(Registered::default()),
            removed: Mutex::new(Registered::default()),
            values: Mutex::new(Registered::default()),
            open: std::sync::atomic::AtomicBool::new(false),
            polling: std::sync::atomic::AtomicBool::new(false),
            schedules: Default::default(),
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
        self.last.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }
    pub fn open(&self) -> bool {
        if sdl3::init_controllers().is_err() {
            return false;
        }
        self.open.store(true, std::sync::atomic::Ordering::Release);
        true
    }
    pub fn close(&self) {
        self.open.store(false, std::sync::atomic::Ordering::Release);
        self.schedules.clear();
        self.devices
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        self.last.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }
    fn accepts(&self, device: &Device) -> bool {
        let criteria = self.criteria.lock().unwrap_or_else(|e| e.into_inner());
        let root = &device.elements[0];
        criteria.is_empty()
            || criteria.iter().any(|c| {
                c.matches(
                    root.page,
                    root.usage,
                    device.vendor as u32,
                    device.product as u32,
                )
            })
    }
    pub fn devices(&self) -> Vec<Arc<Device>> {
        if !self.open.load(std::sync::atomic::Ordering::Acquire) {
            return Vec::new();
        }
        Device::discover()
            .into_iter()
            .filter(|d| self.accepts(d))
            .collect()
    }
    fn delivering(&self) -> bool {
        self.open.load(std::sync::atomic::Ordering::Acquire) && self.schedules.current()
    }
    pub fn poll(&self) {
        use std::sync::atomic::Ordering;
        if !self.delivering() || self.polling.swap(true, Ordering::AcqRel) {
            return;
        }
        struct Guard<'a>(&'a std::sync::atomic::AtomicBool);
        impl Drop for Guard<'_> {
            fn drop(&mut self) {
                self.0.store(false, Ordering::Release);
            }
        }
        let _guard = Guard(&self.polling);
        let devices = self.devices();
        let (appeared, vanished) = {
            let mut previous = self.devices.lock().unwrap_or_else(|e| e.into_inner());
            let appeared = devices
                .iter()
                .filter(|d| !previous.contains_key(&d.id))
                .cloned()
                .collect::<Vec<_>>();
            let vanished = previous
                .values()
                .filter(|d| !devices.iter().any(|n| n.id == d.id))
                .cloned()
                .collect::<Vec<_>>();
            *previous = devices.iter().map(|d| (d.id, Arc::clone(d))).collect();
            (appeared, vanished)
        };
        for device in appeared {
            if !self.delivering() {
                return;
            }
            let r = *self.matched.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(callback) = r.callback {
                unsafe {
                    callback(
                        r.context as *mut c_void,
                        0,
                        std::ptr::from_ref(self).cast_mut().cast(),
                        Arc::as_ptr(&device).cast(),
                    );
                }
            }
        }
        for device in vanished {
            if !self.delivering() {
                return;
            }
            if !device.is_connected() {
                device.poll(true, || self.delivering());
            }
            self.last
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&device.id);
            let r = *self.removed.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(callback) = r.callback {
                unsafe {
                    callback(
                        r.context as *mut c_void,
                        0,
                        std::ptr::from_ref(self).cast_mut().cast(),
                        Arc::as_ptr(&device).cast(),
                    );
                }
            }
        }
        for device in devices {
            if !self.delivering() {
                return;
            }
            device.poll(true, || self.delivering());
            let changes = {
                let mut history = self.last.lock().unwrap_or_else(|e| e.into_inner());
                let last = history
                    .entry(device.id)
                    .or_insert_with(|| vec![i64::MIN; device.elements.len()]);
                let mut changes = Vec::new();
                for element in &device.elements {
                    if element.source == crate::hid::Source::Collection {
                        continue;
                    }
                    let value = device.value(element);
                    let old = &mut last[element.cookie as usize];
                    if *old != value {
                        *old = value;
                        changes.push((element.cookie, value));
                    }
                }
                changes
            };
            for (cookie, integer) in changes {
                if !self.delivering() || !self.accepts(&device) {
                    return;
                }
                let r = *self.values.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(callback) = r.callback {
                    let value = crate::value::new(&device, cookie, integer);
                    unsafe {
                        callback(
                            r.context as *mut c_void,
                            0,
                            std::ptr::from_ref(self).cast_mut().cast(),
                            value,
                        );
                    }
                    crate::value::release(value);
                }
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
            vendor: None,
            product: None,
        };
        assert!(gamepad.matches(page::GENERIC_DESKTOP, usage::GAMEPAD, 1, 2));
        assert!(!gamepad.matches(page::GENERIC_DESKTOP, usage::JOYSTICK, 1, 2));
        assert!(!gamepad.matches(page::BUTTON, usage::GAMEPAD, 1, 2));

        // A criterion that names only a page matches every usage on it, which
        // is what a guest asking for "anything on the desktop page" means.
        let any_desktop = Criterion {
            page: Some(page::GENERIC_DESKTOP),
            usage: None,
            vendor: None,
            product: None,
        };
        assert!(any_desktop.matches(page::GENERIC_DESKTOP, usage::JOYSTICK, 1, 2));
        assert!(!any_desktop.matches(page::BUTTON, 1, 1, 2));
    }
}
