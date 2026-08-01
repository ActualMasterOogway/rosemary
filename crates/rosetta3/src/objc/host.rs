//! The host's Objective-C runtime, as this crate calls it.
//!
//! Every function here is **public API** — `objc/runtime.h`, documented and
//! stable since Mac OS X 10.5. That is the whole point of the posture M6
//! settled on: the alternative was synthesizing the image registration dyld
//! performs, which means private entry points, a `mach_header` the host runtime
//! would have to accept, and a metadata format that
//! [differs between the two architectures](../../../docs/ROADMAP.md). Replaying
//! through the public API asks the host runtime to do only things it already
//! does for `objc_allocateClassPair` callers, of which there are many.
//!
//! ## Why linking, rather than `dlsym`
//!
//! `machish`'s [`NativeDylibProvider`] resolves the *guest's* imports with
//! `dlsym` because the guest's needs are discovered at load time. This crate's
//! own needs are known at compile time, so they are a link dependency and the
//! linker checks them. `libobjc.A.dylib` is present on every macOS system and
//! is already in this process whenever anything has touched Foundation.
//!
//! [`NativeDylibProvider`]: machish::resolve::NativeDylibProvider

use std::ffi::c_char;

/// An opaque `Class`. A pointer, and never dereferenced here — the whole
/// argument for using the public API is that the layout stays the runtime's
/// business.
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

    /// The `IMP` and the type encoding of a method, which together are the whole
    /// of what a message send needs to know.
    ///
    /// Asked of the *class of the receiver*, so one function answers for both
    /// kinds of method: a class method is an instance method of the metaclass,
    /// which is what `object_getClass` of a class returns.
    pub fn method_getImplementation(method: Method) -> Imp;
    pub fn method_getTypeEncoding(method: Method) -> *const c_char;

    pub fn ivar_getOffset(ivar: Ivar) -> isize;

    pub fn sel_registerName(name: *const c_char) -> Sel;
    pub fn sel_getName(sel: Sel) -> *const c_char;

    /// Declared with no parameters on purpose.
    ///
    /// `objc_msgSend` has no signature of its own — it has whichever signature
    /// the method has, which is the fact all of M6.3 turns on. Rust cannot spell
    /// that, so this exists to be taken as an *address* and transmuted to the
    /// prototype the selector's type encoding describes. Calling it through this
    /// declaration would pass no arguments at all.
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
///
/// Returns `None` for a name with an interior NUL rather than truncating.
/// Truncation would register a class under a *different* name than the guest
/// declared, which the guest would then fail to find — a silent wrong answer in
/// a place with no diagnostic.
pub fn c_string(s: &str) -> Option<std::ffi::CString> {
    std::ffi::CString::new(s).ok()
}
