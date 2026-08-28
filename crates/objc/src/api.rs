//! The public C API, as the guest and the rest of this tree call it.
//!
//! None of these is `#[no_mangle]`, deliberately. This runtime must be loadable
//! into a process that already has Apple's libobjc - that is the whole of how it
//! gets tested - so the C spellings exist only as strings in [`crate::symbol`],
//! and the addresses reach a caller through the loader rather than the linker.

// The C spellings are the interface; renaming them would only hide it.
#![allow(non_snake_case)]

use std::ffi::{CStr, c_char, c_void};

use crate::class::{Class, Imp, Ivar, Method};
use crate::object::Id;
use crate::protocol::{Description, Protocol};
use crate::sel::Sel;

/// A borrowed C string, or `None` for null.
///
/// # Safety
///
/// `p` must be null or a NUL-terminated string that outlives the call.
unsafe fn cstr<'a>(p: *const c_char) -> Option<&'a CStr> {
    // SAFETY: the caller's obligation.
    (!p.is_null()).then(|| unsafe { CStr::from_ptr(p) })
}

/// A class pointer as a class.
///
/// # Safety
///
/// `p` must be null or a class pointer this runtime produced.
unsafe fn class<'a>(p: *const c_void) -> Option<&'static Class> {
    // SAFETY: the caller's obligation.
    unsafe { crate::class::from_ptr(p as *const Class) }
}

/// A `Method` handle.
///
/// # Safety
///
/// `p` must be null or a handle this runtime produced.
unsafe fn method<'a>(p: *const c_void) -> Option<&'static Method> {
    // SAFETY: the caller's obligation.
    (!p.is_null()).then(|| unsafe { &*(p as *const Method) })
}

/// A `Protocol *`.
///
/// # Safety
///
/// `p` must be null or a protocol this runtime produced.
unsafe fn protocol<'a>(p: *const c_void) -> Option<&'static Protocol> {
    // SAFETY: the caller's obligation.
    (!p.is_null()).then(|| unsafe { &*(p as *const Protocol) })
}

fn as_ptr<T>(value: Option<&'static T>) -> *mut c_void {
    value.map_or(std::ptr::null_mut(), |v| v as *const T as *mut c_void)
}

pub unsafe extern "C" fn sel_registerName(name: *const c_char) -> Sel {
    // SAFETY: the caller's obligation.
    match unsafe { cstr(name) } {
        Some(name) => crate::sel::register(name),
        None => std::ptr::null(),
    }
}

pub unsafe extern "C" fn sel_getName(sel: Sel) -> *const c_char {
    match sel.is_null() {
        true => c"<null selector>".as_ptr(),
        false => crate::sel::name(sel),
    }
}

pub unsafe extern "C" fn sel_isEqual(a: Sel, b: Sel) -> bool {
    crate::sel::is_equal(a, b)
}

pub unsafe extern "C" fn objc_getClass(name: *const c_char) -> *mut c_void {
    // SAFETY: the caller's obligation.
    as_ptr(unsafe { cstr(name) }.and_then(crate::class::lookup))
}

/// The same as [`objc_getClass`] here.
///
/// Apple's two differ in whether the class handler runs, and this runtime has no
/// class handler: nothing here loads a class on demand.
pub unsafe extern "C" fn objc_lookUpClass(name: *const c_char) -> *mut c_void {
    // SAFETY: the caller's obligation.
    unsafe { objc_getClass(name) }
}

pub unsafe extern "C" fn objc_getRequiredClass(name: *const c_char) -> *mut c_void {
    // SAFETY: the caller's obligation.
    let found = unsafe { objc_getClass(name) };
    if found.is_null() {
        // SAFETY: as above.
        let name = unsafe { cstr(name) }.map_or("<null>".into(), |n| n.to_string_lossy());
        crate::fail!("objc_getRequiredClass: no class named {name}");
    }
    found
}

pub unsafe extern "C" fn objc_getMetaClass(name: *const c_char) -> *mut c_void {
    // SAFETY: the caller's obligation.
    as_ptr(unsafe { cstr(name) }.and_then(crate::class::lookup).map(|c| c.metaclass()))
}

pub unsafe extern "C" fn objc_allocateClassPair(
    superclass: *const c_void,
    name: *const c_char,
    extra: usize,
) -> *mut c_void {
    // SAFETY: the caller's obligation.
    let Some(name) = (unsafe { cstr(name) }) else {
        return std::ptr::null_mut();
    };
    // SAFETY: as above. Null is a root class, which is what `nil` means here.
    let superclass = unsafe { class(superclass) };
    as_ptr(crate::class::allocate_pair(superclass, name, extra))
}

pub unsafe extern "C" fn objc_registerClassPair(cls: *const c_void) {
    // SAFETY: the caller's obligation.
    if let Some(cls) = unsafe { class(cls) } {
        crate::class::register_pair(cls);
    }
}

pub unsafe extern "C" fn objc_disposeClassPair(cls: *const c_void) {
    // SAFETY: the caller's obligation.
    if let Some(cls) = unsafe { class(cls) } {
        crate::class::dispose_pair(cls);
    }
}

pub unsafe extern "C" fn class_getName(cls: *const c_void) -> *const c_char {
    // SAFETY: the caller's obligation.
    unsafe { class(cls) }.map_or(c"nil".as_ptr(), |c| c.name())
}

pub unsafe extern "C" fn class_getSuperclass(cls: *const c_void) -> *mut c_void {
    // SAFETY: the caller's obligation.
    as_ptr(unsafe { class(cls) }.and_then(|c| c.superclass()))
}

pub unsafe extern "C" fn class_isMetaClass(cls: *const c_void) -> bool {
    // SAFETY: the caller's obligation.
    unsafe { class(cls) }.is_some_and(|c| c.is_meta())
}

pub unsafe extern "C" fn class_getInstanceSize(cls: *const c_void) -> usize {
    // SAFETY: the caller's obligation.
    unsafe { class(cls) }.map_or(0, |c| c.instance_size())
}

pub unsafe extern "C" fn class_addMethod(
    cls: *const c_void,
    name: Sel,
    imp: Imp,
    types: *const c_char,
) -> bool {
    // SAFETY: the caller's obligation.
    let (Some(cls), Some(types)) = (unsafe { class(cls) }, unsafe { cstr(types) }) else {
        return false;
    };
    cls.add_method(name, imp, types)
}

pub unsafe extern "C" fn class_replaceMethod(
    cls: *const c_void,
    name: Sel,
    imp: Imp,
    types: *const c_char,
) -> Imp {
    // SAFETY: the caller's obligation.
    let (Some(cls), Some(types)) = (unsafe { class(cls) }, unsafe { cstr(types) }) else {
        return std::ptr::null();
    };
    cls.replace_method(name, imp, types)
}

pub unsafe extern "C" fn class_addIvar(
    cls: *const c_void,
    name: *const c_char,
    size: usize,
    alignment: u8,
    types: *const c_char,
) -> bool {
    // SAFETY: the caller's obligation.
    let (Some(cls), Some(name), Some(types)) = (
        unsafe { class(cls) },
        unsafe { cstr(name) },
        unsafe { cstr(types) },
    ) else {
        return false;
    };
    cls.add_ivar(name, size, alignment, types)
}

pub unsafe extern "C" fn class_getInstanceVariable(
    cls: *const c_void,
    name: *const c_char,
) -> *const c_void {
    // SAFETY: the caller's obligation.
    let (Some(cls), Some(name)) = (unsafe { class(cls) }, unsafe { cstr(name) }) else {
        return std::ptr::null();
    };
    cls.ivar(name)
        .map_or(std::ptr::null(), |i| i as *const Ivar as *const c_void)
}

pub unsafe extern "C" fn class_getInstanceMethod(cls: *const c_void, name: Sel) -> *const c_void {
    // SAFETY: the caller's obligation.
    unsafe { class(cls) }
        .and_then(|c| c.method(name))
        .map_or(std::ptr::null(), |m| m as *const Method as *const c_void)
}

pub unsafe extern "C" fn class_getClassMethod(cls: *const c_void, name: Sel) -> *const c_void {
    // SAFETY: the caller's obligation.
    unsafe { class(cls) }
        .and_then(|c| c.metaclass().method(name))
        .map_or(std::ptr::null(), |m| m as *const Method as *const c_void)
}

/// The resolved `IMP` itself, never a forwarding trampoline.
///
/// `rosetta3`'s dispatch caches this value and re-probes it to notice a swizzle,
/// so it must change exactly when the resolution does. See the crate doc.
pub unsafe extern "C" fn class_getMethodImplementation(cls: *const c_void, name: Sel) -> Imp {
    // SAFETY: the caller's obligation.
    unsafe { class(cls) }.map_or(std::ptr::null(), |c| {
        crate::class::method_implementation(c, name)
    })
}

pub unsafe extern "C" fn class_respondsToSelector(cls: *const c_void, name: Sel) -> bool {
    // SAFETY: the caller's obligation.
    unsafe { class(cls) }.is_some_and(|c| c.method(name).is_some())
}

pub unsafe extern "C" fn class_addProtocol(cls: *const c_void, proto: *const c_void) -> bool {
    // SAFETY: the caller's obligation.
    let (Some(cls), Some(proto)) = (unsafe { class(cls) }, unsafe { protocol(proto) }) else {
        return false;
    };
    cls.add_protocol(proto)
}

pub unsafe extern "C" fn class_conformsToProtocol(cls: *const c_void, proto: *const c_void) -> bool {
    // SAFETY: the caller's obligation.
    let (Some(cls), Some(proto)) = (unsafe { class(cls) }, unsafe { protocol(proto) }) else {
        return false;
    };
    cls.conforms_to(proto)
}

pub unsafe extern "C" fn class_createInstance(cls: *const c_void, extra: usize) -> Id {
    // SAFETY: the caller's obligation.
    unsafe { class(cls) }.map_or(std::ptr::null_mut(), |c| {
        crate::object::create_instance(c, extra)
    })
}

/// The `IMP` exactly as it was installed.
///
/// Never a wrapper: `rosetta3` compares this against its own thunk table to tell
/// a guest method from a host one. See the crate doc.
pub unsafe extern "C" fn method_getImplementation(m: *const c_void) -> Imp {
    // SAFETY: the caller's obligation.
    unsafe { method(m) }.map_or(std::ptr::null(), |m| m.implementation())
}

pub unsafe extern "C" fn method_getTypeEncoding(m: *const c_void) -> *const c_char {
    // SAFETY: the caller's obligation.
    unsafe { method(m) }.map_or(std::ptr::null(), |m| m.types())
}

pub unsafe extern "C" fn method_getName(m: *const c_void) -> Sel {
    // SAFETY: the caller's obligation.
    unsafe { method(m) }.map_or(std::ptr::null(), |m| m.sel)
}

pub unsafe extern "C" fn method_setImplementation(m: *const c_void, imp: Imp) -> Imp {
    // SAFETY: the caller's obligation.
    unsafe { method(m) }.map_or(std::ptr::null(), |m| m.set_implementation(imp))
}

pub unsafe extern "C" fn ivar_getOffset(ivar: *const c_void) -> isize {
    // SAFETY: an `Ivar` this runtime produced, or null.
    match ivar.is_null() {
        true => 0,
        false => unsafe { &*(ivar as *const Ivar) }.offset(),
    }
}

pub unsafe extern "C" fn ivar_getName(ivar: *const c_void) -> *const c_char {
    // SAFETY: as above.
    match ivar.is_null() {
        true => std::ptr::null(),
        false => unsafe { &*(ivar as *const Ivar) }.name(),
    }
}

pub unsafe extern "C" fn object_getClass(object: Id) -> *mut c_void {
    // SAFETY: the caller's obligation.
    as_ptr(unsafe { crate::object::class_of(object) })
}

pub unsafe extern "C" fn object_setClass(object: Id, cls: *const c_void) -> *mut c_void {
    // SAFETY: the caller's obligation.
    let Some(cls) = (unsafe { class(cls) }) else {
        return std::ptr::null_mut();
    };
    // SAFETY: as above.
    as_ptr(unsafe { crate::object::set_class(object, cls) })
}

pub unsafe extern "C" fn object_getClassName(object: Id) -> *const c_char {
    // SAFETY: the caller's obligation.
    unsafe { crate::object::class_of(object) }.map_or(c"nil".as_ptr(), |c| c.name())
}

pub unsafe extern "C" fn object_getIndexedIvars(object: Id) -> *mut c_void {
    // SAFETY: the caller's obligation.
    unsafe { crate::object::indexed_ivars(object) }
}

pub unsafe extern "C" fn object_getIvar(object: Id, ivar: *const c_void) -> Id {
    // SAFETY: the caller's obligation.
    let Some(name) = (unsafe { cstr(ivar_getName(ivar)) }) else {
        return std::ptr::null_mut();
    };
    // SAFETY: as above.
    unsafe { crate::object::get_ivar(object, name) }
}

pub unsafe extern "C" fn object_setIvar(object: Id, ivar: *const c_void, value: Id) {
    // SAFETY: the caller's obligation.
    let Some(name) = (unsafe { cstr(ivar_getName(ivar)) }) else {
        return;
    };
    // SAFETY: as above.
    unsafe { crate::object::set_ivar(object, name, value) }
}

pub unsafe extern "C" fn objc_constructInstance(cls: *const c_void, bytes: *mut c_void) -> Id {
    // SAFETY: the caller's obligation.
    let Some(cls) = (unsafe { class(cls) }) else {
        return std::ptr::null_mut();
    };
    if bytes.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: the caller says `bytes` is at least `instance_size` bytes.
    unsafe { *(bytes as *mut *mut Class) = crate::class::ptr(cls) };
    bytes
}

pub unsafe extern "C" fn objc_destructInstance(object: Id) -> Id {
    object
}

pub unsafe extern "C" fn objc_getProtocol(name: *const c_char) -> *mut c_void {
    // SAFETY: the caller's obligation.
    as_ptr(unsafe { cstr(name) }.and_then(crate::protocol::lookup))
}

pub unsafe extern "C" fn objc_allocateProtocol(name: *const c_char) -> *mut c_void {
    // SAFETY: the caller's obligation.
    as_ptr(unsafe { cstr(name) }.and_then(crate::protocol::allocate))
}

pub unsafe extern "C" fn objc_registerProtocol(proto: *const c_void) {
    // SAFETY: the caller's obligation.
    if let Some(proto) = unsafe { protocol(proto) } {
        crate::protocol::register(proto);
    }
}

pub unsafe extern "C" fn protocol_getName(proto: *const c_void) -> *const c_char {
    // SAFETY: the caller's obligation.
    unsafe { protocol(proto) }.map_or(c"nil".as_ptr(), |p| p.name())
}

pub unsafe extern "C" fn protocol_addProtocol(proto: *const c_void, addition: *const c_void) {
    // SAFETY: the caller's obligation.
    if let (Some(proto), Some(addition)) =
        (unsafe { protocol(proto) }, unsafe { protocol(addition) })
    {
        proto.add_protocol(addition);
    }
}

pub unsafe extern "C" fn protocol_addMethodDescription(
    proto: *const c_void,
    name: Sel,
    types: *const c_char,
    is_required: bool,
    is_instance: bool,
) {
    // SAFETY: the caller's obligation.
    if let (Some(proto), Some(types)) = (unsafe { protocol(proto) }, unsafe { cstr(types) }) {
        proto.add_method(name, types, is_required, is_instance);
    }
}

/// Returned by value: two words, a selector and a type encoding.
pub unsafe extern "C" fn protocol_getMethodDescription(
    proto: *const c_void,
    name: Sel,
    is_required: bool,
    is_instance: bool,
) -> Description {
    // SAFETY: the caller's obligation.
    unsafe { protocol(proto) }.map_or(
        Description {
            sel: std::ptr::null(),
            types: std::ptr::null(),
        },
        |p| p.method(name, is_required, is_instance),
    )
}

pub unsafe extern "C" fn objc_retain(object: Id) -> Id {
    crate::arc::retain(object)
}

pub unsafe extern "C" fn objc_release(object: Id) {
    crate::arc::release(object);
}

pub unsafe extern "C" fn objc_autorelease(object: Id) -> Id {
    crate::arc::autorelease(object)
}

pub unsafe extern "C" fn objc_autoreleasePoolPush() -> *mut c_void {
    crate::arc::pool_push()
}

pub unsafe extern "C" fn objc_autoreleasePoolPop(token: *mut c_void) {
    crate::arc::pool_pop(token);
}

/// The return-value optimisations, none of which this runtime elides.
///
/// Eliding them needs the caller and the callee to agree through a marker
/// instruction after the call site, which is a bargain between two compilations
/// this runtime is not part of. Doing the plain thing is always correct.
pub unsafe extern "C" fn objc_autoreleaseReturnValue(object: Id) -> Id {
    crate::arc::autorelease(object)
}

pub unsafe extern "C" fn objc_retainAutoreleaseReturnValue(object: Id) -> Id {
    crate::arc::autorelease(crate::arc::retain(object))
}

pub unsafe extern "C" fn objc_retainAutoreleasedReturnValue(object: Id) -> Id {
    crate::arc::retain(object)
}

pub unsafe extern "C" fn objc_unsafeClaimAutoreleasedReturnValue(object: Id) -> Id {
    object
}

pub unsafe extern "C" fn objc_retainAutorelease(object: Id) -> Id {
    crate::arc::autorelease(crate::arc::retain(object))
}

pub unsafe extern "C" fn objc_storeStrong(slot: *mut Id, value: Id) {
    if slot.is_null() {
        return;
    }
    // SAFETY: the caller owns `slot`.
    let previous = unsafe { *slot };
    if previous == value {
        return;
    }
    crate::arc::retain(value);
    // SAFETY: as above.
    unsafe { *slot = value };
    crate::arc::release(previous);
}

pub unsafe extern "C" fn objc_initWeak(slot: *mut Id, value: Id) -> Id {
    if slot.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: the caller owns `slot`, which is uninitialized storage.
    unsafe { *slot = std::ptr::null_mut() };
    crate::arc::store_weak(slot, value)
}

pub unsafe extern "C" fn objc_storeWeak(slot: *mut Id, value: Id) -> Id {
    match slot.is_null() {
        true => std::ptr::null_mut(),
        false => crate::arc::store_weak(slot, value),
    }
}

pub unsafe extern "C" fn objc_loadWeakRetained(slot: *mut Id) -> Id {
    match slot.is_null() {
        true => std::ptr::null_mut(),
        false => crate::arc::load_weak_retained(slot),
    }
}

pub unsafe extern "C" fn objc_loadWeak(slot: *mut Id) -> Id {
    // SAFETY: the caller owns `slot`.
    match slot.is_null() {
        true => std::ptr::null_mut(),
        false => crate::arc::autorelease(unsafe { objc_loadWeakRetained(slot) }),
    }
}

pub unsafe extern "C" fn objc_destroyWeak(slot: *mut Id) {
    if !slot.is_null() {
        crate::arc::destroy_weak(slot);
    }
}

pub unsafe extern "C" fn objc_copyWeak(to: *mut Id, from: *mut Id) {
    if to.is_null() || from.is_null() {
        return;
    }
    // SAFETY: the caller owns both slots; `to` is uninitialized.
    unsafe { *to = std::ptr::null_mut() };
    // SAFETY: as above.
    crate::arc::store_weak(to, unsafe { *from });
}

pub unsafe extern "C" fn objc_moveWeak(to: *mut Id, from: *mut Id) {
    // SAFETY: the caller owns both slots.
    unsafe { objc_copyWeak(to, from) };
    // SAFETY: as above.
    unsafe { objc_destroyWeak(from) };
}

pub unsafe extern "C" fn objc_sync_enter(object: Id) -> i32 {
    crate::arc::sync_enter(object);
    0
}

pub unsafe extern "C" fn objc_sync_exit(object: Id) -> i32 {
    crate::arc::sync_exit(object);
    0
}

pub unsafe extern "C" fn objc_setAssociatedObject(
    object: Id,
    key: *const c_void,
    value: Id,
    _policy: usize,
) {
    crate::arc::set_associated(object, key, value);
}

pub unsafe extern "C" fn objc_getAssociatedObject(object: Id, key: *const c_void) -> Id {
    crate::arc::associated(object, key)
}
