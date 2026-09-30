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

/// The directories a zoneinfo database is found in, in the order `tzset` itself
/// looks: `TZDIR` overrides, and the rest are where the distributions put it -
/// NixOS uses `/etc/zoneinfo`, most others `/usr/share/zoneinfo`.
pub fn database() -> Option<std::path::PathBuf> {
    std::env::var("TZDIR")
        .ok()
        .filter(|dir| !dir.is_empty())
        .map(std::path::PathBuf::from)
        .into_iter()
        .chain(
            ["/usr/share/zoneinfo", "/etc/zoneinfo", "/usr/lib/zoneinfo"]
                .into_iter()
                .map(std::path::PathBuf::from),
        )
        .find(|dir| dir.is_dir())
}

/// The zone called `name`, read from the host's database once and kept, or
/// `None` for a name the host does not have.
///
/// The system zone falls back to `/etc/localtime`, which is where the host's
/// zone lives even when its name is not in a database this can find; UTC and
/// GMT are answered without one.
pub fn zone(name: &str) -> Option<std::sync::Arc<crate::tzif::Zone>> {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};
    static ZONES: Mutex<Option<HashMap<String, Option<Arc<crate::tzif::Zone>>>>> = Mutex::new(None);

    let mut zones = ZONES.lock().unwrap_or_else(|e| e.into_inner());
    let zones = zones.get_or_insert_default();
    if let Some(found) = zones.get(name) {
        return found.clone();
    }
    // A name is a relative path inside the database and nothing else.
    let valid = !name.is_empty()
        && !name.starts_with('/')
        && !name
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..");
    let read = |path: std::path::PathBuf| {
        std::fs::read(path)
            .ok()
            .and_then(|bytes| crate::tzif::Zone::parse(&bytes))
    };
    let found = valid
        .then(|| database().and_then(|root| read(root.join(name))))
        .flatten()
        .or_else(|| {
            (name == system_name())
                .then(|| read("/etc/localtime".into()))
                .flatten()
        })
        .or_else(|| match name {
            "UTC" | "GMT" | "Etc/UTC" | "Etc/GMT" => Some(crate::tzif::Zone::fixed(0, name)),
            _ => None,
        })
        .map(Arc::new);
    zones.insert(name.to_owned(), found.clone());
    found
}

/// The zone this host is set to, as an IANA name.
pub fn system_name() -> String {
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
    // SAFETY: a string or null.
    let Some(name) = (unsafe { crate::string::as_string(name) }) else {
        return std::ptr::null();
    };
    let name = String::from_utf8_lossy(&name.utf8()).into_owned();
    // A name the host has no zone for is NULL, as it is on a Mac, rather than
    // an object every later question about would have to refuse.
    match zone(&name) {
        Some(_) => new(&name),
        None => std::ptr::null(),
    }
}

/// The zone's name as Rust text, or `None` for anything that is not one of
/// this tree's time zones - a null included, which the calendar reads as UTC.
///
/// # Safety
///
/// `timezone` is null or an object.
pub unsafe fn name_of(timezone: CFTypeRef) -> Option<String> {
    if !unsafe { crate::base::is_kind_of(timezone, class()) } {
        return None;
    }
    // SAFETY: an instance of `class()` has this layout.
    let name = unsafe { (*(timezone as *const Body)).name };
    // SAFETY: the name is a string this module made.
    let text = unsafe { crate::string::as_string(name) }?;
    Some(String::from_utf8_lossy(&text.utf8()).into_owned())
}

pub unsafe extern "C" fn get_name(timezone: CFTypeRef) -> CFTypeRef {
    // SAFETY: a timezone from this runtime.
    if !unsafe { crate::base::is_kind_of(timezone, class()) } {
        return std::ptr::null();
    }
    // SAFETY: an instance of `class()` has this layout.
    unsafe { (*(timezone as *const Body)).name }
}
