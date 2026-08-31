//! `NSURL`'s methods. The object itself is `corefoundation::url`, because
//! `CFURLRef` is `NSURL *` and the storage cannot be two things.

use objc::object::Id;
use objc::sel::Sel;

use crate::define::{class_methods, methods};

/// The URL a receiver holds, as text.
///
/// # Safety
///
/// `receiver` must be null or an object this runtime allocated.
pub unsafe fn text_of(receiver: Id) -> Option<String> {
    // SAFETY: the caller's obligation.
    unsafe { corefoundation::url::as_url(receiver as corefoundation::base::CFTypeRef) }
        .map(|u| u.text.clone())
}

/// `+URLWithString:`, autoreleased.
///
/// nil for text that is not a URL, which is what Apple's answers and what a
/// caller checks for. "Not a URL" is only the empty string here: a scheme this
/// runtime does not recognise is still a URL, and refusing it would be this
/// module deciding what the transport is allowed to be asked for.
unsafe extern "C" fn url_with_string(_class: Id, _sel: Sel, text: Id) -> Id {
    // SAFETY: an `NSString`, or nil.
    let bytes = unsafe { crate::string::bytes(text) };
    let Ok(text) = std::str::from_utf8(&bytes) else {
        return std::ptr::null_mut();
    };
    if text.is_empty() {
        return std::ptr::null_mut();
    }
    // SAFETY: a URL this call just made.
    unsafe { corefoundation::api::CFAutorelease(corefoundation::url::new(text)) as Id }
}

/// `+fileURLWithPath:` and `+fileURLWithPath:isDirectory:`.
unsafe extern "C" fn file_url_with_path(_class: Id, _sel: Sel, path: Id) -> Id {
    // SAFETY: an `NSString`, or nil.
    let bytes = unsafe { crate::string::bytes(path) };
    let Ok(path) = std::str::from_utf8(&bytes) else {
        return std::ptr::null_mut();
    };
    // Whether it is a directory is asked of the file system, which is what
    // Apple's single-argument form does too.
    let directory = std::path::Path::new(path).is_dir();
    // SAFETY: a URL this call just made.
    unsafe {
        corefoundation::api::CFAutorelease(corefoundation::url::new_file_url(path, directory)) as Id
    }
}

unsafe extern "C" fn file_url_with_path_is_directory(
    _class: Id,
    _sel: Sel,
    path: Id,
    directory: u8,
) -> Id {
    // SAFETY: an `NSString`, or nil.
    let bytes = unsafe { crate::string::bytes(path) };
    let Ok(path) = std::str::from_utf8(&bytes) else {
        return std::ptr::null_mut();
    };
    // SAFETY: a URL this call just made.
    unsafe {
        corefoundation::api::CFAutorelease(corefoundation::url::new_file_url(path, directory != 0))
            as Id
    }
}

/// One accessor, given what it reads off the contents.
macro_rules! accessor {
    ($name:ident, $read:expr) => {
        unsafe extern "C" fn $name(receiver: Id, _sel: Sel) -> Id {
            // SAFETY: the receiver is an `NSURL`.
            let Some(url) = (unsafe {
                corefoundation::url::as_url(receiver as corefoundation::base::CFTypeRef)
            }) else {
                return std::ptr::null_mut();
            };
            let read: fn(&corefoundation::url::Contents) -> Option<String> = $read;
            match read(url) {
                Some(text) => autoreleased(&text),
                None => std::ptr::null_mut(),
            }
        }
    };
}

accessor!(absolute_string, |u| Some(u.text.clone()));
accessor!(path, |u| Some(u.path()).filter(|p| !p.is_empty()));
accessor!(scheme, |u| Some(u.scheme()).filter(|s| !s.is_empty()));

/// `-fileSystemRepresentation`, which is a `const char *` into storage the URL
/// keeps alive - so it is autoreleased as an `NSString` and its bytes handed
/// back, which is where the guest reads them from until the pool drains.
unsafe extern "C" fn file_system_representation(
    receiver: Id,
    _sel: Sel,
) -> *const std::ffi::c_char {
    // SAFETY: the receiver is an `NSURL`.
    let Some(url) =
        (unsafe { corefoundation::url::as_url(receiver as corefoundation::base::CFTypeRef) })
    else {
        return std::ptr::null();
    };
    let Some(path) = url.file_path() else {
        return std::ptr::null();
    };
    let string = crate::string::from_str(&path);
    // SAFETY: a fresh string, autoreleased so its bytes outlive this call by as
    // long as the pool does; `-UTF8String` is `const char * (id, SEL)`.
    unsafe {
        corefoundation::api::CFAutorelease(string as corefoundation::base::CFTypeRef);
        let sel = objc::sel::get("UTF8String");
        match objc::object::class_of(string).and_then(|c| c.method(sel)) {
            Some(method) => {
                let f: unsafe extern "C" fn(Id, Sel) -> *const std::ffi::c_char =
                    std::mem::transmute(method.implementation());
                f(string, sel)
            }
            None => std::ptr::null(),
        }
    }
}

unsafe extern "C" fn is_file_url(receiver: Id, _sel: Sel) -> u8 {
    // SAFETY: the receiver is an `NSURL`.
    let is_file =
        unsafe { corefoundation::url::as_url(receiver as corefoundation::base::CFTypeRef) }
            .is_some_and(|u| u.is_file_url());
    u8::from(is_file)
}

fn autoreleased(text: &str) -> Id {
    let string = crate::string::from_str(text);
    // SAFETY: a string this call just created.
    unsafe { corefoundation::api::CFAutorelease(string as corefoundation::base::CFTypeRef) as Id }
}

pub(crate) fn install() {
    let class = corefoundation::url::class();
    // On `NSURL` rather than on the concrete class, so `+URLWithString:` reaches
    // the spelling a guest actually sends it to.
    let abstract_class = objc::class::lookup(c"NSURL").expect("registered by corefoundation");
    class_methods!(abstract_class, {
        "URLWithString:" => url_with_string, c"@24@0:8@16";
        "fileURLWithPath:" => file_url_with_path, c"@24@0:8@16";
        "fileURLWithPath:isDirectory:" => file_url_with_path_is_directory, c"@28@0:8@16c24";
    });
    methods!(class, {
        "absoluteString" => absolute_string, c"@16@0:8";
        "description" => absolute_string, c"@16@0:8";
        "path" => path, c"@16@0:8";
        "scheme" => scheme, c"@16@0:8";
        "isFileURL" => is_file_url, c"c16@0:8";
        "fileSystemRepresentation" => file_system_representation, c"*16@0:8";
    });
}
