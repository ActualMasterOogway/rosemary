//! Host frameworks, resolved at the moment the overlay first needs them.
//!
//! Everything this module reaches for — CoreGraphics, CoreText, OpenGL,
//! HIToolbox — is a framework present on every macOS system, and every one of
//! them is found with `dlsym` rather than linked. That is a deliberate
//! difference from [`crate::objc::host`], which links `libobjc` because this
//! crate's Objective-C support is not optional and the linker should check it.
//!
//! The overlay *is* optional, and more than that it is **inert until a hotkey
//! is pressed**. Linking CoreText would pull CoreGraphics, ApplicationServices
//! and their initializers into the address space of every run, including the
//! console programs and the test fixtures that will never draw anything, and
//! this runtime's whole posture is that the process it builds should look like
//! the guest's process and not like a toolkit's. So the frameworks load on
//! first use and a run that never shows the overlay never sees them.
//!
//! [`symbol`] tries `RTLD_DEFAULT` first for the same reason: in a guest that
//! draws, CoreGraphics is already loaded — its window brought it in — and the
//! `dlopen` is only for the case where it is not.

use std::ffi::{CString, c_void};

/// The frameworks the overlay may have to bring in itself, by absolute path.
///
/// A path rather than a leaf name, because `dlopen("CoreText.framework")` is
/// answered by the loader's search paths and this must not depend on what the
/// guest has set in its environment — the guest sets `DYLD_*` variables and it
/// is not this module's business to be affected by them.
const FRAMEWORKS: &[&str] = &[
    "/System/Library/Frameworks/CoreFoundation.framework/CoreFoundation",
    "/System/Library/Frameworks/CoreGraphics.framework/CoreGraphics",
    "/System/Library/Frameworks/CoreText.framework/CoreText",
    "/System/Library/Frameworks/OpenGL.framework/OpenGL",
    "/System/Library/Frameworks/Carbon.framework/Carbon",
];

/// Bring the frameworks in, once.
///
/// Failures are silent and are meant to be: a system without OpenGL is a system
/// where the OpenGL backend will find no entry points and say so through
/// [`symbol`] returning `None`, which is a better diagnostic than a message at
/// load time about a framework nothing has asked for yet.
fn load_frameworks() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        for path in FRAMEWORKS {
            let Ok(c) = CString::new(*path) else { continue };
            // SAFETY: a NUL-terminated path and a valid mode. The handle is
            // deliberately dropped — these are process-lifetime frameworks and
            // nothing here ever closes one.
            unsafe { libc::dlopen(c.as_ptr(), libc::RTLD_LAZY | libc::RTLD_LOCAL) };
        }
    });
}

/// A host function by name, or `None` if this system does not export it.
///
/// Two attempts, in this order: the images already loaded, then the frameworks
/// above. The first is what answers in a guest that draws — a program with a
/// Metal layer has CoreGraphics mapped long before it presents a frame — and
/// the second is the console case, where nothing has needed a font yet.
pub fn symbol(name: &str) -> Option<*mut c_void> {
    let c = CString::new(name).ok()?;
    // SAFETY: `RTLD_DEFAULT` is a valid pseudo-handle, `c` outlives the call,
    // and a name the process does not export comes back null, which is checked.
    let found = unsafe { libc::dlsym(libc::RTLD_DEFAULT, c.as_ptr()) };
    if !found.is_null() {
        return Some(found);
    }
    load_frameworks();
    // SAFETY: as above.
    let found = unsafe { libc::dlsym(libc::RTLD_DEFAULT, c.as_ptr()) };
    (!found.is_null()).then_some(found)
}

/// A host function as a typed pointer, or `None`.
///
/// # Safety
///
/// The caller states that `name` has the signature `F`. Nothing checks it and
/// nothing can: a `dlsym` result is an address, and the only record of what it
/// expects is the framework's header. Every call site here names the header
/// declaration it was checked against.
pub unsafe fn function<F: Copy>(name: &str) -> Option<F> {
    const {
        assert!(
            size_of::<F>() == size_of::<*mut c_void>(),
            "F must be a bare function pointer, not a fat one",
        );
    }
    let raw = symbol(name)?;
    // SAFETY: `F` is the caller's claim about the symbol, and the size
    // assertion above is what rules out transmuting an address into something
    // wider than a pointer.
    Some(unsafe { std::mem::transmute_copy::<*mut c_void, F>(&raw) })
}

// -------------------------------------------------- CoreFoundation shapes ---

/// An opaque `CFTypeRef`. Never dereferenced here.
pub type CfRef = *mut c_void;

/// `CGFloat`, which on every 64-bit Apple platform is a `double`.
pub type CgFloat = f64;

/// `CGPoint`, `CGSize` and `CGRect` from `<CoreGraphics/CGGeometry.h>`.
///
/// Declared again rather than reused from [`crate::shims::coregraphics`]
/// because that module's copies exist to describe what the *guest* passes, and
/// these describe what this runtime passes to the host. The two are the same
/// shape today and are answers to different questions; joining them would make
/// a change to the guest's ABI silently a change to the overlay's.
#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct Point {
    pub x: CgFloat,
    pub y: CgFloat,
}

#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct Size {
    pub width: CgFloat,
    pub height: CgFloat,
}

#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct Rect {
    pub origin: Point,
    pub size: Size,
}

/// `kCFStringEncodingUTF8`.
pub const CF_UTF8: u32 = 0x0800_0100;

/// A `CFStringRef` for `s`, or null.
///
/// The caller owns it and must `CFRelease` it — a `Create` function is +1 by
/// the Core Foundation naming rule, and the overlay creates a handful of these
/// exactly once, so they are kept rather than released.
pub fn cf_string(s: &str) -> CfRef {
    // `CFStringCreateWithBytes(CFAllocatorRef, const UInt8*, CFIndex,
    //  CFStringEncoding, Boolean)` — bytes rather than a C string so a `&str`
    // does not have to be copied into a `CString` first.
    type Create = unsafe extern "C" fn(CfRef, *const u8, isize, u32, u8) -> CfRef;
    // SAFETY: the declaration above is `<CoreFoundation/CFString.h>`'s.
    let Some(create) = (unsafe { function::<Create>("CFStringCreateWithBytes") }) else {
        return std::ptr::null_mut();
    };
    // SAFETY: `s` is a live UTF-8 slice for the length of the call, the
    // encoding is a real one, and a null allocator means the default.
    unsafe {
        create(
            std::ptr::null_mut(),
            s.as_ptr(),
            s.len() as isize,
            CF_UTF8,
            0,
        )
    }
}
