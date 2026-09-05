//! The small toll-free `CFTimeZone`/`NSTimeZone` object used during startup.

use std::ffi::c_void;

use objc::class::Class;
use objc::object::Id;

use crate::base::CFTypeRef;

#[repr(C)]
struct Body {
    isa: *const Class,
    name: CFTypeRef,
}

fn class() -> &'static Class {
    static CLASS: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    let pointer = *CLASS.get_or_init(|| {
        let base = crate::base::abstract_class(c"NSTimeZone", None);
        crate::base::note_type_class(base);
        let class = crate::base::register(c"__NSCFTimeZone", Some(base), size_of::<Body>(), false);
        class.replace_method(
            objc::sel::get("dealloc"),
            dealloc as *const c_void,
            c"v16@0:8",
        );
        objc::class::ptr(class) as usize
    });
    // SAFETY: a class registered above and kept for the process.
    unsafe { objc::class::from_ptr(pointer as *const Class).expect("a timezone class") }
}

unsafe extern "C" fn dealloc(receiver: Id, _sel: objc::sel::Sel) {
    // SAFETY: this class's object layout and its owned name.
    unsafe {
        let body = receiver as *mut Body;
        objc::arc::release((*body).name as Id);
        objc::arc::finalize(receiver);
    }
}

fn new(name: &str) -> CFTypeRef {
    // SAFETY: `Body` is the registered layout.
    let body = unsafe { crate::base::allocate::<Body>(class()) };
    if body.is_null() {
        return std::ptr::null();
    }
    // SAFETY: a freshly allocated body.
    unsafe { (*body).name = crate::string::from_bytes(name.as_bytes(), false) };
    body as CFTypeRef
}

fn system_name() -> String {
    std::fs::read_link("/etc/localtime")
        .ok()
        .and_then(|path| {
            let text = path.to_string_lossy();
            text.split("/zoneinfo/").nth(1).map(str::to_owned)
        })
        .or_else(|| std::env::var("TZ").ok().filter(|name| !name.is_empty()))
        .unwrap_or_else(|| "UTC".to_string())
}

pub unsafe extern "C" fn copy_default() -> CFTypeRef {
    new(&system_name())
}

pub unsafe extern "C" fn copy_system() -> CFTypeRef {
    new(&system_name())
}

pub unsafe extern "C" fn create_with_name(
    _allocator: CFTypeRef,
    name: CFTypeRef,
    _try_abbreviation: u8,
) -> CFTypeRef {
    // SAFETY: the caller's obligation - a string or null.
    let Some(name) = (unsafe { crate::string::as_string(name) }) else {
        return std::ptr::null();
    };
    new(&String::from_utf8_lossy(&name.utf8()))
}

pub unsafe extern "C" fn get_name(timezone: CFTypeRef) -> CFTypeRef {
    // SAFETY: the caller's obligation - a timezone from this runtime.
    if !unsafe { crate::base::is_kind_of(timezone, class()) } {
        return std::ptr::null();
    }
    // SAFETY: an instance of `class()` has this layout.
    unsafe { (*(timezone as *const Body)).name }
}
