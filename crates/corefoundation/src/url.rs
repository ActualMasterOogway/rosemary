//! `CFURL`, which is `NSURL *`.
//!
//! Only file URLs are built here, because that is what a bundle answers with and
//! what a guest turns straight back into a path. A URL is stored as the *path*
//! rather than as text: every caller in this tree either asks for the file
//! system representation or hands it to another call that does, and round
//! tripping through percent-encoded text would only add a place to lose a
//! character.
//!
//! The `file://` spelling is still what `-absoluteString` reports, so a guest
//! that logs one sees what Apple's would print.

use std::ffi::c_void;

use objc::class::Class;
use objc::object::Id;

use crate::base::CFTypeRef;

/// A URL object: the `isa`, and the path hanging off it.
#[repr(C)]
struct Body {
    isa: *const Class,
    contents: *mut Contents,
}

pub struct Contents {
    /// An absolute file system path, with no trailing slash except at the root.
    pub path: String,
    /// Whether the URL names a directory, which is the one bit
    /// `CFURLCreateWithFileSystemPath` takes and `-absoluteString` shows.
    pub directory: bool,
}

impl Contents {
    /// `file:///Applications/Foo.app/Contents/Resources/`, which is what
    /// `-absoluteString` answers.
    pub fn text(&self) -> String {
        let mut out = String::from("file://");
        out.push_str(&percent_encode(&self.path));
        if self.directory && !out.ends_with('/') {
            out.push('/');
        }
        out
    }
}

/// The characters a path may carry into a URL unescaped.
///
/// RFC 3986's unreserved set plus the sub-delimiters a file path routinely
/// contains, and `/` itself. Anything else is escaped, which is what stops a
/// bundle under a directory with a space in its name from producing a URL that
/// re-parses as two.
fn unreserved(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || b"-._~/!$&'()*+,;=:@".contains(&byte)
}

fn percent_encode(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for byte in path.bytes() {
        match unreserved(byte) {
            true => out.push(byte as char),
            false => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// The concrete class, under `NSURL` so that a guest's `isKindOfClass:` and its
/// `_OBJC_CLASS_$_NSURL` binding agree about the object.
pub fn class() -> &'static Class {
    static CLASS: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    let ptr = *CLASS.get_or_init(|| {
        let base = crate::base::abstract_class(c"NSURL", None);
        crate::base::note_type_class(base);
        let class = crate::base::register(c"__NSCFURL", Some(base), size_of::<Body>(), false);
        class.replace_method(objc::sel::get("dealloc"), dealloc as *const c_void, c"v16@0:8");
        objc::class::ptr(class) as usize
    });
    // SAFETY: the cell holds a pointer to a class this function registered.
    unsafe { objc::class::from_ptr(ptr as *const Class).expect("a class") }
}

unsafe extern "C" fn dealloc(receiver: Id, _sel: objc::sel::Sel) {
    // SAFETY: `-dealloc` is reached with the last reference gone, and the
    // receiver is a `Body` whose `contents` came from `Box::into_raw`.
    unsafe {
        let body = receiver as *mut Body;
        if !(*body).contents.is_null() {
            drop(Box::from_raw((*body).contents));
            (*body).contents = std::ptr::null_mut();
        }
        objc::arc::finalize(receiver);
    }
}

/// A file URL naming `path`, owned by the caller.
pub fn new_file_url(path: &str, directory: bool) -> CFTypeRef {
    // SAFETY: `Body` begins with `isa` and is the layout `class` was registered
    // with.
    let body: *mut Body = unsafe { crate::base::allocate(class()) };
    if body.is_null() {
        return std::ptr::null();
    }
    let contents = Contents {
        path: path.trim_end_matches('/').to_string(),
        directory,
    };
    // A path that trimmed to nothing was the root, which is the one path whose
    // trailing slash is the whole path.
    let contents = match contents.path.is_empty() {
        true => Contents {
            path: "/".to_string(),
            ..contents
        },
        false => contents,
    };
    // SAFETY: a live instance whose second word is ours to fill in.
    unsafe { (*body).contents = Box::into_raw(Box::new(contents)) };
    body as CFTypeRef
}

/// The contents of `url`, if it is one.
///
/// # Safety
///
/// `url` must be null or an object this runtime allocated.
pub unsafe fn as_url<'a>(url: CFTypeRef) -> Option<&'a Contents> {
    // SAFETY: the caller's obligation.
    if !unsafe { crate::base::is_kind_of(url, class()) } {
        return None;
    }
    // SAFETY: an instance of `class()` is a `Body` with a live `contents`.
    unsafe {
        let body = url as *const Body;
        (!(*body).contents.is_null()).then(|| &*(*body).contents)
    }
}
