use std::ffi::c_void;
use std::sync::Arc;

use corefoundation::base::CFTypeRef;

use crate::hid::Device;

/// What an `IOHIDValueRef` points at.
#[repr(C)]
pub struct Value {
    /// The device the element belongs to, so `IOHIDValueGetElement` can answer.
    pub device: usize,
    pub cookie: u32,
    pub integer: i64,
}

/// A value for one element's reading, which the caller releases.
pub fn new(device: &Arc<Device>, cookie: u32, integer: i64) -> CFTypeRef {
    let value = Box::into_raw(Box::new(Value {
        device: Arc::as_ptr(device) as usize,
        cookie,
        integer,
    })) as CFTypeRef;
    // A value is the one handle here that is freed, so it is forgotten again in
    // [`release`] rather than left in the table for an unrelated allocation to
    // land on. See `corefoundation::base::note_opaque`.
    corefoundation::base::note_opaque(value);
    value
}

/// Give one back.
pub fn release(value: CFTypeRef) {
    if value.is_null() {
        return;
    }
    corefoundation::base::forget_opaque(value);
    // SAFETY: a pointer [`new`] produced, released once - which is what the one
    // caller, the manager's poll, does immediately after the callback returns.
    drop(unsafe { Box::from_raw(value as *mut Value) });
}

/// # Safety
///
/// `value` must be null or a pointer [`new`] produced and not yet released.
pub unsafe fn borrow<'a>(value: CFTypeRef) -> Option<&'a Value> {
    // SAFETY: the caller's obligation.
    (!value.is_null()).then(|| unsafe { &*(value as *const Value) })
}

/// The device a value came from.
///
/// # Safety
///
/// As [`borrow`], and the device must still be open - which it is for the
/// duration of the callback the value was made for.
pub unsafe fn device<'a>(value: &Value) -> Option<&'a Device> {
    (value.device != 0).then(|| {
        // SAFETY: the caller's obligation.
        unsafe { &*(value.device as *const Device) }
    })
}

/// An `IOHIDElementRef`: which device, and which of its elements.
///
/// A real allocation rather than a packed pointer. An element is not separately
/// allocated - it is an index into its device's vector - but `IOHIDElementRef`
/// has to be a pointer a guest can store and hand back later, and packing the
/// index into the low bits of the device pointer would need an alignment a
/// `Box` does not give.
///
/// One per element, made when the device is opened and living as long as it.
#[derive(Clone, Copy)]
pub struct ElementRef {
    pub device: *const Device,
    pub cookie: u32,
}

/// # Safety
///
/// `handle` must be null or a pointer to an [`ElementRef`] a live device owns.
pub unsafe fn element<'a>(handle: *const c_void) -> Option<(&'a Device, u32)> {
    if handle.is_null() {
        return None;
    }
    // SAFETY: the caller's obligation.
    let reference = unsafe { &*(handle as *const ElementRef) };
    if reference.device.is_null() {
        return None;
    }
    // SAFETY: as above - the device outlives every handle it owns.
    Some((unsafe { &*reference.device }, reference.cookie))
}
