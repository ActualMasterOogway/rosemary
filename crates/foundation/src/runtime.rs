//! Foundation's spellings of four Objective-C runtime questions, and `-className`.
//!
//! `NSSelectorFromString` and `NSClassFromString` are what a program uses when
//! the name it is acting on is data - a selector read from a nib, a class
//! chosen by a feature flag - and their inverses are how it reports one. The
//! runtime underneath is `objc`; this crate owns the `NSString` at each end.
//!
//! `-className` is here for the same reason: it is an `NSObject` method that
//! answers with an `NSString`, and `objc` has no string to answer with.

#![allow(non_snake_case)]

use objc::object::Id;
use objc::sel::Sel;

use crate::define::methods;

pub const NAMES: &[&str] = &[
    "NSSelectorFromString",
    "NSStringFromSelector",
    "NSClassFromString",
    "NSStringFromClass",
];

pub fn symbol(name: &str) -> Option<u64> {
    let f: *const () = match name {
        "NSSelectorFromString" => NSSelectorFromString as *const (),
        "NSStringFromSelector" => NSStringFromSelector as *const (),
        "NSClassFromString" => NSClassFromString as *const (),
        "NSStringFromClass" => NSStringFromClass as *const (),
        _ => return None,
    };
    Some(f as usize as u64)
}

/// The text of an `NSString` argument, or `None` when it is nil or not one.
///
/// # Safety
///
/// `string` is nil or an object this runtime allocated.
unsafe fn text(string: Id) -> Option<String> {
    if string.is_null() {
        return None;
    }
    // SAFETY: the caller's obligation.
    let bytes = unsafe { crate::string::bytes(string) };
    String::from_utf8(bytes).ok()
}

/// An autoreleased `NSString`, as every `NSStringFrom...` returns.
fn autoreleased(text: &str) -> Id {
    let string = corefoundation::string::from_bytes(text.as_bytes(), false);
    // SAFETY: a fresh object this call owns.
    unsafe { corefoundation::api::CFAutorelease(string) as Id }
}

/// `SEL NSSelectorFromString(NSString *)`.
///
/// Registers the name rather than only finding it, which is Apple's behaviour:
/// a selector is its name, and one nothing has sent yet is still a selector.
unsafe extern "C" fn NSSelectorFromString(name: Id) -> Sel {
    // SAFETY: the caller passed an `NSString` or nil.
    let Some(name) = (unsafe { text(name) }) else {
        return std::ptr::null();
    };
    // A selector name with an interior NUL is not one; `objc::sel::get` would
    // rather panic than register half of it.
    if name.contains('\0') {
        return std::ptr::null();
    }
    objc::sel::get(&name)
}

/// `NSString *NSStringFromSelector(SEL)`.
unsafe extern "C" fn NSStringFromSelector(selector: Sel) -> Id {
    if selector.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: a registered selector, whose name is a live NUL-terminated string.
    let name = unsafe { std::ffi::CStr::from_ptr(objc::sel::name(selector)) };
    autoreleased(&name.to_string_lossy())
}

/// `Class NSClassFromString(NSString *)`, nil for a class this tree has not
/// registered - which is what a guest checks before using a class it can do
/// without.
unsafe extern "C" fn NSClassFromString(name: Id) -> *const std::ffi::c_void {
    // SAFETY: the caller passed an `NSString` or nil.
    let Some(name) = (unsafe { text(name) }) else {
        return std::ptr::null();
    };
    let Ok(name) = std::ffi::CString::new(name) else {
        return std::ptr::null();
    };
    objc::class::lookup(&name)
        .map(|class| class as *const objc::class::Class as *const std::ffi::c_void)
        .unwrap_or(std::ptr::null())
}

/// `NSString *NSStringFromClass(Class)`.
unsafe extern "C" fn NSStringFromClass(class: *const objc::class::Class) -> Id {
    // SAFETY: a live class object, as the caller's `Class` is.
    let Some(class) = (unsafe { class.as_ref() }) else {
        return std::ptr::null_mut();
    };
    autoreleased(class.name_str())
}

/// `-className`, the receiver's class name as an `NSString`.
///
/// Foundation declares it on `NSObject`, so every object answers it - which is
/// what a caller logging or dispatching on a type relies on. Autoreleased,
/// because the name it is built from is the class's and the string is not.
unsafe extern "C" fn class_name(receiver: Id, _sel: Sel) -> Id {
    // SAFETY: the receiver is a live object.
    let name = match unsafe { objc::object::class_of(receiver) } {
        Some(class) => class.name_str().to_string(),
        // Nil has no class, and `-className` on it would not have been dispatched
        // - this is the unreachable branch rather than a guess.
        None => String::new(),
    };
    let string = crate::string::from_str(&name);
    // SAFETY: a fresh object.
    unsafe { corefoundation::api::CFAutorelease(string as corefoundation::base::CFTypeRef) as Id }
}

pub(crate) fn install() {
    let nsobject = objc::class::lookup(c"NSObject").expect("the runtime's own NSObject");
    methods!(nsobject, {
        "className" => class_name, c"@16@0:8";
    });
}
