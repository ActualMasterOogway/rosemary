use std::ffi::c_void;
use std::sync::OnceLock;

use corefoundation::base::{CFTypeID, CFTypeRef};
use objc::class::Class;

use crate::hid::Device;

pub fn type_id() -> CFTypeID {
    corefoundation::base::type_id(class())
}

fn class() -> &'static Class {
    static CLASS: OnceLock<usize> = OnceLock::new();
    let pointer = *CLASS.get_or_init(|| {
        let class = corefoundation::base::register(c"__IOHIDValue", None, size_of::<Value>(), true);
        objc::class::ptr(class) as usize
    });
    // SAFETY: a leaked class registered above.
    unsafe { &*(pointer as *const Class) }
}

/// What an `IOHIDValueRef` points at.
#[repr(C)]
pub struct Value {
    isa: *const Class,
    /// The device the element belongs to, so `IOHIDValueGetElement` can answer.
    pub device: usize,
    pub cookie: u32,
    pub integer: i64,
}

/// A value for one element's reading. Its owner decides whether to release or
/// autorelease it according to the API that produced it.
pub fn new(device: &Device, cookie: u32, integer: i64) -> CFTypeRef {
    // SAFETY: `Value` starts with an isa and is the layout registered by
    // `class`.
    let value = unsafe { corefoundation::base::allocate::<Value>(class()) };
    // SAFETY: a new instance; allocation initialized the isa word and the
    // remaining fields belong to this type.
    unsafe {
        (*value).device = std::ptr::from_ref(device) as usize;
        (*value).cookie = cookie;
        (*value).integer = integer;
    }
    value as CFTypeRef
}

/// Give one back.
pub fn release(value: CFTypeRef) {
    if value.is_null() {
        return;
    }
    objc::arc::release(value as objc::object::Id);
}

/// # Safety
///
/// `value` must be null or a pointer [`new`] produced and not yet released.
pub unsafe fn borrow<'a>(value: CFTypeRef) -> Option<&'a Value> {
    (!value.is_null()).then(|| unsafe { &*(value as *const Value) })
}

/// The device a value came from.
///
/// # Safety
///
/// As [`borrow`], and the device must still be open - which it is for the
/// duration of the callback the value was made for.
pub unsafe fn device<'a>(value: &Value) -> Option<&'a Device> {
    (value.device != 0).then(|| unsafe { &*(value.device as *const Device) })
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
    let reference = unsafe { &*(handle as *const ElementRef) };
    if reference.device.is_null() {
        return None;
    }
    // SAFETY: as above - the device outlives every handle it owns.
    Some((unsafe { &*reference.device }, reference.cookie))
}
