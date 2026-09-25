use objc::class::Class;
use objc::object::Id;
use objc::sel::Sel;

use crate::define::methods;

#[repr(C)]
struct Body {
    isa: *const Class,
    contents: *mut Contents,
}

struct Contents {
    domain: String,
    code: i64,
    description: String,
}

pub const NSURL_ERROR_DOMAIN: &str = "NSURLErrorDomain";

/// `NSURLErrorUnknown`. The URL error codes are negative, and this is the one
/// for a failure with no more specific code - which is every failure here, since
/// what this runtime learns from a transport error is a message rather than a
/// classification.
pub const NSURL_ERROR_UNKNOWN: i64 = -1;

fn class() -> &'static Class {
    corefoundation::base::register(c"NSError", None, size_of::<Body>(), false)
}

/// An autoreleased `NSError`.
pub fn make(domain: &str, code: i64, description: &str) -> Id {
    // SAFETY: `Body` begins with `isa` and is the layout `class` was registered
    // with.
    let body: *mut Body = unsafe { corefoundation::base::allocate(class()) };
    if body.is_null() {
        return std::ptr::null_mut();
    }
    let contents = Contents {
        domain: domain.to_string(),
        code,
        description: description.to_string(),
    };
    // SAFETY: a live instance whose second word is ours to fill in.
    unsafe { (*body).contents = Box::into_raw(Box::new(contents)) };
    objc::arc::autorelease(body as Id)
}

/// # Safety
///
/// `receiver` must be an `NSError` this module made.
unsafe fn contents<'a>(receiver: Id) -> Option<&'a Contents> {
    if receiver.is_null() {
        return None;
    }
    unsafe {
        let body = receiver as *const Body;
        (!(*body).contents.is_null()).then(|| &*(*body).contents)
    }
}

unsafe extern "C" fn domain(receiver: Id, _sel: Sel) -> Id {
    // SAFETY: the receiver is an `NSError` this module made.
    match unsafe { contents(receiver) } {
        Some(c) => autoreleased(&c.domain),
        None => std::ptr::null_mut(),
    }
}

unsafe extern "C" fn code(receiver: Id, _sel: Sel) -> i64 {
    unsafe { contents(receiver) }.map_or(0, |c| c.code)
}

unsafe extern "C" fn localized_description(receiver: Id, _sel: Sel) -> Id {
    match unsafe { contents(receiver) } {
        Some(c) => autoreleased(&c.description),
        None => std::ptr::null_mut(),
    }
}

/// `-userInfo`: the description under its documented key, and nothing else.
///
/// A dictionary rather than nil, because a caller reads
/// `userInfo[NSLocalizedDescriptionKey]` directly as often as it sends
/// `-localizedDescription`.
unsafe extern "C" fn user_info(receiver: Id, _sel: Sel) -> Id {
    // SAFETY: the receiver is an `NSError` this module made.
    let Some(c) = (unsafe { contents(receiver) }) else {
        return std::ptr::null_mut();
    };
    let key = crate::string::from_str("NSLocalizedDescription");
    let value = crate::string::from_str(&c.description);
    let dictionary = corefoundation::collections::new_dictionary(
        &[(
            key as corefoundation::base::CFTypeRef,
            value as corefoundation::base::CFTypeRef,
        )],
        corefoundation::callbacks::Policy::from_type_callbacks(),
        corefoundation::callbacks::Policy::from_type_callbacks(),
        false,
    );
    // SAFETY: the dictionary retained both, and this call owns the two it made.
    unsafe {
        corefoundation::api::CFRelease(key as corefoundation::base::CFTypeRef);
        corefoundation::api::CFRelease(value as corefoundation::base::CFTypeRef);
        corefoundation::api::CFAutorelease(dictionary) as Id
    }
}

unsafe extern "C" fn dealloc(receiver: Id, _sel: Sel) {
    // SAFETY: `-dealloc` is reached with the last reference gone, and the
    // receiver is a `Body` whose `contents` came from `Box::into_raw`.
    unsafe {
        let body = receiver as *mut Body;
        if !(*body).contents.is_null() {
            drop(Box::from_raw((*body).contents));
            (*body).contents = std::ptr::null_mut();
        }
    }
    objc::arc::finalize(receiver);
}

fn autoreleased(text: &str) -> Id {
    let string = crate::string::from_str(text);
    // SAFETY: a string this call just created.
    unsafe { corefoundation::api::CFAutorelease(string as corefoundation::base::CFTypeRef) as Id }
}

pub(crate) fn install() {
    let class = class();
    methods!(class, {
        "domain" => domain, c"@16@0:8";
        "code" => code, c"q16@0:8";
        "localizedDescription" => localized_description, c"@16@0:8";
        "description" => localized_description, c"@16@0:8";
        "userInfo" => user_info, c"@16@0:8";
        "dealloc" => dealloc, c"v16@0:8";
    });
}
