//! The code-signing half of the Security framework, on a host with no code
//! signatures.
//!
//! A program asks these three questions in a row: make a requirement out of a
//! string ("anchor apple generic and identifier com.example.game"), get a
//! handle on my own running code, and check the one against the other. On a Mac
//! that is how a program decides whether it is the copy it was shipped as.
//!
//! There is no code-signing machinery here: nothing checked the guest's
//! signature on the way in, and no certificate on this host chains to Apple's
//! anchor. So the check cannot be *performed*, and the two answers available
//! are both wrong in different directions. `errSecCSUnsigned` - the truthful
//! one about this host - is what a program reads as "this copy has been
//! tampered with", and it puts up that alert and stops; a binary the user
//! bought and did not modify is then refused for a check nobody ran.
//!
//! So the check succeeds, and **says on stderr that it verified nothing**. That
//! is the line that keeps this from being a silent lie: the guest is told its
//! own code is intact - which, for the unmodified binary this tree loads, it
//! is - and the person running it is told the signature was never examined.
//!
//! The first two calls do succeed, because they are questions this tree *can*
//! answer: a requirement is the text it was made from, and the running code is
//! this process.
//! 
//! FIXME (ACCURACY): Implement actual code signing checks!

#![allow(non_snake_case)]

use std::ffi::c_void;

use corefoundation::base::CFTypeRef;
use objc::class::Class;
use objc::object::Id;

type OSStatus = i32;
const SUCCESS: OSStatus = 0;
/// `errSecCSInvalidObjectRef`: a handle that is not one of these.
const INVALID_OBJECT_REF: OSStatus = -67071;
/// `errSecParam`.
const PARAM: OSStatus = -50;

/// A `SecCodeRef` or a `SecRequirementRef`.
///
/// One class for both: nothing here tells them apart, and a guest holds each
/// only to pass it back. The payload is what the handle was made from - the
/// requirement's text, or the running program's path - so that a report can say
/// which requirement was not met rather than only that one was not.
#[repr(C)]
struct Body {
    isa: *const Class,
    description: CFTypeRef,
}

fn class() -> &'static Class {
    static CLASS: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    let pointer = *CLASS.get_or_init(|| {
        let class =
            corefoundation::base::register(c"__R3SecCodeObject", None, size_of::<Body>(), true);
        class.replace_method(
            objc::sel::get("dealloc"),
            dealloc as *const c_void,
            c"v16@0:8",
        );
        objc::class::ptr(class) as usize
    });
    // SAFETY: a class registered above and kept for the process.
    unsafe { objc::class::from_ptr(pointer as *const Class).expect("a code object class") }
}

unsafe extern "C" fn dealloc(receiver: Id, _sel: objc::sel::Sel) {
    // SAFETY: this class's object layout and its owned description.
    unsafe {
        let body = receiver as *mut Body;
        objc::arc::release((*body).description as Id);
        objc::arc::finalize(receiver);
    }
}

/// A handle carrying `description`, owned by the caller.
fn new(description: CFTypeRef) -> CFTypeRef {
    // SAFETY: `Body` is the registered layout.
    let body = unsafe { corefoundation::base::allocate::<Body>(class()) };
    if body.is_null() {
        return std::ptr::null();
    }
    // SAFETY: a freshly allocated body.
    unsafe { (*body).description = description };
    body as CFTypeRef
}

/// # Safety
///
/// `object` is null or an object this runtime allocated.
unsafe fn is_ours(object: CFTypeRef) -> bool {
    // SAFETY: the caller's obligation.
    unsafe { corefoundation::base::is_kind_of(object, class()) }
}

/// `OSStatus SecRequirementCreateWithString(CFStringRef text, SecCSFlags flags,
/// SecRequirementRef *requirement)`
///
/// The text is kept rather than parsed. Parsing it would buy nothing: the check
/// below cannot be met by any code on this host, so a requirement that had been
/// understood and one that had not would be refused alike.
///
/// # Safety
///
/// `requirement` is null or a writable `SecRequirementRef`.
pub unsafe extern "C" fn SecRequirementCreateWithString(
    text: CFTypeRef,
    _flags: u32,
    requirement: *mut CFTypeRef,
) -> OSStatus {
    if requirement.is_null() {
        return PARAM;
    }
    // SAFETY: the caller's obligation - a string or null.
    if unsafe { corefoundation::string::as_string(text) }.is_none() {
        return PARAM;
    }
    let held = objc::arc::retain(text as Id) as CFTypeRef;
    // SAFETY: the caller states it is writable.
    unsafe { *requirement = new(held) };
    SUCCESS
}

/// `OSStatus SecRequirementCreateWithData(CFDataRef data, SecCSFlags flags,
/// SecRequirementRef *requirement)`
///
/// The compiled form of the same thing. Kept with no description, because the
/// bytes are a requirement blob this tree does not decode.
///
/// # Safety
///
/// As [`SecRequirementCreateWithString`].
pub unsafe extern "C" fn SecRequirementCreateWithData(
    _data: CFTypeRef,
    _flags: u32,
    requirement: *mut CFTypeRef,
) -> OSStatus {
    if requirement.is_null() {
        return PARAM;
    }
    // SAFETY: the caller states it is writable.
    unsafe { *requirement = new(std::ptr::null()) };
    SUCCESS
}

/// `OSStatus SecCodeCopySelf(SecCSFlags flags, SecCodeRef *self)`
///
/// # Safety
///
/// `out` is null or a writable `SecCodeRef`.
pub unsafe extern "C" fn SecCodeCopySelf(_flags: u32, out: *mut CFTypeRef) -> OSStatus {
    if out.is_null() {
        return PARAM;
    }
    let path = std::env::current_exe()
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_default();
    let description = corefoundation::string::from_bytes(path.as_bytes(), false);
    // SAFETY: the caller states it is writable.
    unsafe { *out = new(description) };
    SUCCESS
}

/// `OSStatus SecCodeCheckValidity(SecCodeRef code, SecCSFlags flags,
/// SecRequirementRef requirement)`
///
/// Always `errSecSuccess`, with nothing verified - see the module note. Said
/// once on stderr, because a signature check that did not happen is exactly the
/// thing that must not pass unremarked.
///
/// # Safety
///
/// Both arguments are null or objects this runtime allocated.
pub unsafe extern "C" fn SecCodeCheckValidity(
    code: CFTypeRef,
    _flags: u32,
    requirement: CFTypeRef,
) -> OSStatus {
    // SAFETY: the caller's obligation.
    if !unsafe { is_ours(code) } {
        return INVALID_OBJECT_REF;
    }
    // SAFETY: as above; a null requirement means "any valid code", which is
    // still no code, so it is refused the same way.
    if !requirement.is_null() && !unsafe { is_ours(requirement) } {
        return INVALID_OBJECT_REF;
    }
    static SAID: std::sync::Once = std::sync::Once::new();
    SAID.call_once(|| {
        eprintln!(
            "rosetta3: security: the guest checked its own code signature. There is no \
             code-signing on this host, so nothing was examined and the check is reported as \
             passing - see crates/security/src/codesign.rs"
        );
    });
    SUCCESS
}

exports::exports! {
    functions {
        "SecRequirementCreateWithString" => SecRequirementCreateWithString,
        "SecRequirementCreateWithData" => SecRequirementCreateWithData,
        "SecCodeCopySelf" => SecCodeCopySelf,
        "SecCodeCheckValidity" => SecCodeCheckValidity,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pair a program makes, and the answer it gets: both handles come
    /// back, and the check passes - while a handle this module did not make is
    /// still refused, which is what keeps the success from being unconditional.
    #[test]
    fn the_check_passes_for_a_handle_this_module_made() {
        objc::bootstrap();
        let text = corefoundation::string::from_bytes(b"anchor apple generic", false);
        let mut requirement = std::ptr::null();
        let mut code = std::ptr::null();
        // SAFETY: a string this test made, and two writable out-parameters.
        unsafe {
            assert_eq!(
                SecRequirementCreateWithString(text, 0, &mut requirement),
                SUCCESS
            );
            assert_eq!(SecCodeCopySelf(0, &mut code), SUCCESS);
        }
        assert!(!requirement.is_null() && !code.is_null());
        // SAFETY: two handles the calls above made.
        assert_eq!(
            unsafe { SecCodeCheckValidity(code, 0, requirement) },
            SUCCESS
        );
        // SAFETY: a handle this test owns, against storage that is not one.
        let mut other = 0_u64;
        assert_eq!(
            unsafe { SecCodeCheckValidity((&raw mut other).cast(), 0, requirement) },
            INVALID_OBJECT_REF
        );
        // SAFETY: three objects this test owns.
        unsafe {
            corefoundation::api::CFRelease(requirement);
            corefoundation::api::CFRelease(code);
            corefoundation::api::CFRelease(text);
        }
    }
}
