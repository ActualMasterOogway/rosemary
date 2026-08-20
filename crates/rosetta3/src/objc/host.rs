//! The host's Objective-C runtime, as this crate calls it.

use std::ffi::c_char;

/// An opaque `Class`. A pointer, and never dereferenced here - the whole argument for
/// using the public API is that the layout stays the runtime's business.
pub type Class = *mut std::ffi::c_void;
/// An opaque `Protocol *`.
pub type Protocol = *mut std::ffi::c_void;
/// A uniqued selector.
pub type Sel = *const std::ffi::c_void;
/// An `Ivar` description.
pub type Ivar = *const std::ffi::c_void;
/// A `Method` description: an `IMP` and a type encoding, which is the pair
/// [`crate::objc::dispatch`] needs and the reason a message is callable at all.
pub type Method = *const std::ffi::c_void;
/// A method implementation. Always one of this runtime's arm64 thunks when we
/// are the ones installing it.
pub type Imp = *const std::ffi::c_void;

#[link(name = "objc")]
unsafe extern "C" {
    pub fn objc_getClass(name: *const c_char) -> Class;
    pub fn objc_lookUpClass(name: *const c_char) -> Class;
    pub fn objc_allocateClassPair(superclass: Class, name: *const c_char, extra: usize) -> Class;
    pub fn objc_registerClassPair(cls: Class);
    pub fn objc_disposeClassPair(cls: Class);

    pub fn object_getClass(obj: Class) -> Class;
    pub fn class_getName(cls: Class) -> *const c_char;
    pub fn class_getSuperclass(cls: Class) -> Class;
    pub fn class_getInstanceSize(cls: Class) -> usize;
    pub fn class_addMethod(cls: Class, name: Sel, imp: Imp, types: *const c_char) -> bool;
    pub fn class_addIvar(
        cls: Class,
        name: *const c_char,
        size: usize,
        alignment: u8,
        types: *const c_char,
    ) -> bool;
    pub fn class_addProtocol(cls: Class, protocol: Protocol) -> bool;
    pub fn class_getInstanceVariable(cls: Class, name: *const c_char) -> Ivar;
    pub fn class_getInstanceMethod(cls: Class, name: Sel) -> Method;
    /// Allocate and **zero** an instance of `cls`.
    pub fn class_createInstance(cls: Class, extra: usize) -> Class;
    pub fn class_getClassMethod(cls: Class, name: Sel) -> Method;
    pub fn class_getMethodImplementation(cls: Class, name: Sel) -> Imp;

    /// The `IMP` and the type encoding of a method, which together are the whole of
    /// what a message send needs to know.
    pub fn method_getImplementation(method: Method) -> Imp;
    pub fn method_getTypeEncoding(method: Method) -> *const c_char;
    pub fn method_setImplementation(method: Method, imp: Imp) -> Imp;

    pub fn ivar_getOffset(ivar: Ivar) -> isize;

    pub fn sel_registerName(name: *const c_char) -> Sel;
    pub fn sel_getName(sel: Sel) -> *const c_char;

    /// Declared with no parameters on purpose.
    pub fn objc_msgSend();

    pub fn objc_getProtocol(name: *const c_char) -> Protocol;
    pub fn objc_allocateProtocol(name: *const c_char) -> Protocol;
    pub fn objc_registerProtocol(protocol: Protocol);
    pub fn protocol_addProtocol(protocol: Protocol, addition: Protocol);
    pub fn protocol_addMethodDescription(
        protocol: Protocol,
        name: Sel,
        types: *const c_char,
        is_required: bool,
        is_instance: bool,
    );
}

/// A `&str` as a NUL-terminated C string, for a call that borrows it.
pub fn c_string(s: &str) -> Option<std::ffi::CString> {
    std::ffi::CString::new(s).ok()
}
