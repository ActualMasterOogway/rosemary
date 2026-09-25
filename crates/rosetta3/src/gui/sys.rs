use std::ffi::{CString, c_void};

/// The frameworks the overlay may have to bring in itself, by absolute path.
///
/// A path rather than a leaf name, because `dlopen("CoreText.framework")` is
/// answered by the loader's search paths and this must not depend on what the
/// guest has set in its environment - the guest sets `DYLD_*` variables and it
/// is not this module's business to be affected by them.
const FRAMEWORKS: &[&str] = &[
    "/System/Library/Frameworks/CoreFoundation.framework/CoreFoundation",
    "/System/Library/Frameworks/CoreGraphics.framework/CoreGraphics",
    "/System/Library/Frameworks/CoreText.framework/CoreText",
    "/System/Library/Frameworks/OpenGL.framework/OpenGL",
    "/System/Library/Frameworks/Carbon.framework/Carbon",
];

/// Bring the frameworks in, once.
fn load_frameworks() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        for path in FRAMEWORKS {
            let Ok(c) = CString::new(*path) else { continue };
            // SAFETY: a NUL-terminated path and a valid mode. The handle is
            // deliberately dropped - these are process-lifetime frameworks and
            // nothing here ever closes one.
            unsafe { libc::dlopen(c.as_ptr(), libc::RTLD_LAZY | libc::RTLD_LOCAL) };
        }
    });
}

/// A host function by name, or `None` if this system does not export it.
pub fn symbol(name: &str) -> Option<*mut c_void> {
    let c = CString::new(name).ok()?;
    // SAFETY: `RTLD_DEFAULT` is a valid pseudo-handle, `c` outlives the call,
    // and a name the process does not export comes back null, which is checked.
    let found = unsafe { libc::dlsym(libc::RTLD_DEFAULT, c.as_ptr()) };
    if !found.is_null() {
        return Some(found);
    }
    load_frameworks();
    let found = unsafe { libc::dlsym(libc::RTLD_DEFAULT, c.as_ptr()) };
    (!found.is_null()).then_some(found)
}

/// A host function as a typed pointer, or `None`.
///
/// # Safety
///
/// The caller states that `name` has the signature `F`. Nothing checks it and nothing
/// can.
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

/// An opaque `CFTypeRef`. Never dereferenced here.
pub type CfRef = *mut c_void;

/// `CGFloat`, which on every 64-bit Apple platform is a `double`.
pub type CgFloat = f64;

/// `CGPoint`, `CGSize` and `CGRect` from `<CoreGraphics/CGGeometry.h>`.
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
/// The caller owns it and must `CFRelease` it - a `Create` function is +1 by
/// the Core Foundation naming rule, and the overlay creates a handful of these
/// exactly once, so they are kept rather than released.
pub fn cf_string(s: &str) -> CfRef {
    // `CFStringCreateWithBytes(CFAllocatorRef, const UInt8*, CFIndex,
    //  CFStringEncoding, Boolean)` - bytes rather than a C string so a `&str`
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
