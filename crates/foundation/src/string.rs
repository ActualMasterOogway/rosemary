//! `NSString`, on the storage [`corefoundation::string`] already holds.
//!
//! Every method here is the Objective-C spelling of a CoreFoundation call that
//! already works, which is the whole of what toll-free bridging is: one object,
//! two vocabularies.

use std::ffi::{CStr, c_char};

use corefoundation::api;
use corefoundation::base::CFTypeRef;
use corefoundation::string::encoding;
use objc::object::Id;
use objc::sel::Sel;

use crate::define::{class_methods, methods};

/// `NSStringEncoding`, which is *not* `CFStringEncoding`: Foundation numbers its
/// encodings separately and the two are converted at the boundary.
pub mod ns_encoding {
    pub const ASCII: usize = 1;
    pub const NEXTSTEP: usize = 2;
    pub const UTF8: usize = 4;
    pub const ISO_LATIN1: usize = 5;
    pub const UNICODE: usize = 10;
    pub const WINDOWS_CP1252: usize = 12;
    pub const UTF16_BE: usize = 0x9000_0100;
    pub const UTF16_LE: usize = 0x9400_0100;
}

/// An `NSStringEncoding` as the CoreFoundation one.
pub fn cf_encoding(ns: usize) -> u32 {
    match ns {
        ns_encoding::ASCII => encoding::ASCII,
        ns_encoding::ISO_LATIN1 => encoding::ISO_LATIN_1,
        ns_encoding::WINDOWS_CP1252 => encoding::WINDOWS_LATIN1,
        ns_encoding::UNICODE | ns_encoding::UTF16_LE => encoding::UTF16_LE,
        ns_encoding::UTF16_BE => encoding::UTF16_BE,
        // UTF-8 for everything else, including `NSNEXTSTEPStringEncoding`: it
        // is the encoding every caller in this tree has actually asked for, and
        // a wrong guess here is a string that reads back empty rather than one
        // that reads back wrong.
        _ => encoding::UTF8,
    }
}

/// The receiver's characters as UTF-8, or empty for something that is not a
/// string.
///
/// # Safety
///
/// `receiver` must be null or an object this runtime allocated.
pub unsafe fn bytes(receiver: Id) -> Vec<u8> {
    // SAFETY: the caller's obligation.
    unsafe { corefoundation::string::as_string(receiver as CFTypeRef) }
        .map(|s| s.utf8())
        .unwrap_or_default()
}

unsafe extern "C" fn length(receiver: Id, _sel: Sel) -> usize {
    // SAFETY: an `NSString`.
    unsafe { api::CFStringGetLength(receiver as CFTypeRef) as usize }
}

unsafe extern "C" fn utf8_string(receiver: Id, _sel: Sel) -> *const c_char {
    // SAFETY: an `NSString`.
    unsafe { api::CFStringGetCStringPtr(receiver as CFTypeRef, encoding::UTF8) }
}

unsafe extern "C" fn c_string_using_encoding(
    receiver: Id,
    _sel: Sel,
    ns: usize,
) -> *const c_char {
    // SAFETY: an `NSString`.
    unsafe { api::CFStringGetCStringPtr(receiver as CFTypeRef, cf_encoding(ns)) }
}

unsafe extern "C" fn get_c_string(
    receiver: Id,
    _sel: Sel,
    buffer: *mut c_char,
    max: usize,
    ns: usize,
) -> bool {
    // SAFETY: an `NSString` and the caller's buffer.
    unsafe {
        api::CFStringGetCString(receiver as CFTypeRef, buffer, max as isize, cf_encoding(ns)) != 0
    }
}

unsafe extern "C" fn length_of_bytes(receiver: Id, _sel: Sel, _ns: usize) -> usize {
    // SAFETY: an `NSString`. Bytes rather than characters, which for an
    // eight-bit encoding is the same number and for UTF-8 is the one the caller
    // is about to allocate.
    unsafe { bytes(receiver) }.len()
}

unsafe extern "C" fn is_equal_to_string(receiver: Id, _sel: Sel, other: Id) -> bool {
    // SAFETY: two objects; `CFEqual` checks both.
    unsafe { api::CFEqual(receiver as CFTypeRef, other as CFTypeRef) != 0 }
}

unsafe extern "C" fn character_at_index(receiver: Id, _sel: Sel, index: usize) -> u16 {
    // SAFETY: an `NSString`.
    let bytes = unsafe { bytes(receiver) };
    // The strings this runtime builds are UTF-8 whose characters are its bytes
    // for everything ASCII, which is what a guest indexing into one expects.
    bytes.get(index).map_or(0, |b| *b as u16)
}

unsafe extern "C" fn description(receiver: Id, _sel: Sel) -> Id {
    receiver
}

unsafe extern "C" fn copy(receiver: Id, _sel: Sel) -> Id {
    // A copy of an immutable string is the string, which is what Apple's does
    // and why `-copy` on one is free.
    objc::arc::retain(receiver)
}

// ------------------------------------------------------------ class methods

unsafe extern "C" fn string_with_utf8(_class: Id, _sel: Sel, cstr: *const c_char) -> Id {
    // SAFETY: a NUL-terminated string or null.
    unsafe {
        let s = api::CFStringCreateWithCString(std::ptr::null(), cstr, encoding::UTF8);
        api::CFAutorelease(s) as Id
    }
}

unsafe extern "C" fn string_with_cstring_encoding(
    _class: Id,
    _sel: Sel,
    cstr: *const c_char,
    ns: usize,
) -> Id {
    // SAFETY: as above.
    unsafe {
        let s = api::CFStringCreateWithCString(std::ptr::null(), cstr, cf_encoding(ns));
        api::CFAutorelease(s) as Id
    }
}

unsafe extern "C" fn empty_string(_class: Id, _sel: Sel) -> Id {
    // SAFETY: a literal.
    unsafe {
        let s = api::CFStringCreateWithCString(std::ptr::null(), c"".as_ptr(), encoding::UTF8);
        api::CFAutorelease(s) as Id
    }
}

/// `-initWithBytes:length:encoding:` and its `-initWithCString:` relatives.
///
/// The awkward shape Objective-C allocation has: `+alloc` already made an
/// object, and `-init...` may hand back a *different* one. Here it always does -
/// the allocated shell is released and a properly built string returned - which
/// is allowed, expected, and the only way to reuse the CoreFoundation
/// constructor.
unsafe extern "C" fn init_with_bytes(
    receiver: Id,
    _sel: Sel,
    bytes: *const u8,
    length: usize,
    ns: usize,
) -> Id {
    // SAFETY: the caller says `bytes` covers `length` bytes.
    unsafe {
        objc::arc::release(receiver);
        api::CFStringCreateWithBytes(
            std::ptr::null(),
            bytes,
            length as isize,
            cf_encoding(ns),
            0,
        ) as Id
    }
}

unsafe extern "C" fn init_with_utf8(receiver: Id, _sel: Sel, cstr: *const c_char) -> Id {
    // SAFETY: a NUL-terminated string or null.
    unsafe {
        objc::arc::release(receiver);
        api::CFStringCreateWithCString(std::ptr::null(), cstr, encoding::UTF8) as Id
    }
}

unsafe extern "C" fn init_with_cstring_encoding(
    receiver: Id,
    _sel: Sel,
    cstr: *const c_char,
    ns: usize,
) -> Id {
    // SAFETY: as above.
    unsafe {
        objc::arc::release(receiver);
        api::CFStringCreateWithCString(std::ptr::null(), cstr, cf_encoding(ns)) as Id
    }
}

/// A Rust string as an autoreleased `NSString`, for this crate's own use.
pub fn from_str(text: &str) -> Id {
    corefoundation::string::from_bytes(text.as_bytes(), false) as Id
}

/// A `&CStr` as an `NSString` the caller owns.
pub fn from_cstr(text: &CStr) -> Id {
    corefoundation::string::from_bytes(text.to_bytes(), false) as Id
}

pub(crate) fn install() {
    let class = objc::class::lookup(c"NSString").expect("CoreFoundation registered NSString");
    methods!(class, {
        "length" => length, c"Q16@0:8";
        "UTF8String" => utf8_string, c"*16@0:8";
        "cStringUsingEncoding:" => c_string_using_encoding, c"*24@0:8Q16";
        "getCString:maxLength:encoding:" => get_c_string, c"B40@0:8*16Q24Q32";
        "lengthOfBytesUsingEncoding:" => length_of_bytes, c"Q24@0:8Q16";
        "maximumLengthOfBytesUsingEncoding:" => length_of_bytes, c"Q24@0:8Q16";
        "isEqualToString:" => is_equal_to_string, c"B24@0:8@16";
        "characterAtIndex:" => character_at_index, c"S24@0:8Q16";
        "description" => description, c"@16@0:8";
        "copy" => copy, c"@16@0:8";
        "copyWithZone:" => copy, c"@24@0:8^v16";
        "initWithBytes:length:encoding:" => init_with_bytes, c"@40@0:8^v16Q24Q32";
        "initWithUTF8String:" => init_with_utf8, c"@24@0:8*16";
        "initWithCString:encoding:" => init_with_cstring_encoding, c"@32@0:8*16Q24";
    });
    class_methods!(class, {
        "stringWithUTF8String:" => string_with_utf8, c"@24@0:8*16";
        "stringWithCString:encoding:" => string_with_cstring_encoding, c"@32@0:8*16Q24";
        "string" => empty_string, c"@16@0:8";
    });
}
