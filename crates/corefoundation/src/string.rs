//! `CFString`, and the layout the compiler already decided.
//!
//! A `@"literal"` in the guest's `__DATA` is four words:
//!
//! ```c
//! struct __builtin_CFString { const void *isa; long flags; const char *str; long length; };
//! ```
//!
//! `isa` is the `___CFConstantStringClassReference` the image imported. So the
//! layout of a string here is not a choice - it is that structure, and a
//! constant string becomes a live object of this runtime the moment that one
//! data symbol is bound. Nothing is copied and nothing is converted; see
//! [`crate::constant_string_class`].
//!
//! Dynamically created strings use the same four words with owned storage, so
//! every reader below works on either.

use std::ffi::{CStr, c_char, c_void};
use std::sync::OnceLock;

use objc::class::Class;

use crate::base::{CFIndex, CFTypeRef, type_id};

/// `flags` bit: the bytes are UTF-16 rather than 8-bit.
const WIDE: u64 = 0x10;
/// `flags` bit: `str` is storage this runtime allocated and must free.
///
/// One of this crate's own. Apple keeps the same information in the same word,
/// with different bits; nothing outside this file reads it either way.
const OWNED: u64 = 0x1000_0000;

/// The four words a `CFString` is.
#[repr(C)]
pub struct CFString {
    isa: *const Class,
    flags: u64,
    /// The bytes: 8-bit and NUL-terminated, or UTF-16 and not.
    str: *const u8,
    /// Characters, not bytes. For an 8-bit string the two are the same.
    length: i64,
}

/// `kCFStringEncoding*`, the few a guest actually passes.
pub mod encoding {
    pub const MAC_ROMAN: u32 = 0;
    pub const UTF16: u32 = 0x0100;
    pub const UTF16_BE: u32 = 0x1_0100;
    pub const UTF16_LE: u32 = 0x1_4000;
    pub const ASCII: u32 = 0x0600;
    pub const UTF8: u32 = 0x0800_0100;
    pub const ISO_LATIN_1: u32 = 0x0201;
    /// `kCFStringEncodingWindowsLatin1`, which is what a guest asking for
    /// "the system encoding" most often gets.
    pub const WINDOWS_LATIN1: u32 = 0x0500;
}

/// Whether `encoding` is one where a byte is a character.
///
/// The eight-bit encodings differ above 0x7F, and this runtime does not carry
/// their tables: a string that is pure ASCII reads back identically under all of
/// them, and one that is not is refused rather than mistranslated.
fn is_eight_bit(encoding: u32) -> bool {
    matches!(
        encoding,
        encoding::UTF8
            | encoding::ASCII
            | encoding::ISO_LATIN_1
            | encoding::MAC_ROMAN
            | encoding::WINDOWS_LATIN1
    )
}

fn is_utf16(encoding: u32) -> bool {
    matches!(
        encoding,
        encoding::UTF16 | encoding::UTF16_BE | encoding::UTF16_LE
    )
}

/// The class a dynamically created string has.
pub fn class() -> &'static Class {
    static CLASS: OnceLock<usize> = OnceLock::new();
    let p = *CLASS.get_or_init(|| {
        let class = crate::base::register(c"__NSCFString", None, size_of::<CFString>(), true);
        install_string_methods(class);
        objc::class::ptr(class) as usize
    });
    // SAFETY: a leaked `Class` this crate registered.
    unsafe { &*(p as *const Class) }
}

/// The class a *constant* string has, and the address
/// `___CFConstantStringClassReference` is bound to.
///
/// A separate class from [`class`] for one reason: its storage is in the
/// image's `__DATA` and must never be freed. `-release` on one does nothing,
/// which is what makes a guest's `CFRelease(@"x")` harmless.
pub fn constant_class() -> &'static Class {
    static CLASS: OnceLock<usize> = OnceLock::new();
    let p = *CLASS.get_or_init(|| {
        // A *subclass*, so that `CFGetTypeID` on a literal answers
        // `CFStringGetTypeID()` and every string method is inherited.
        let class = crate::base::register(
            c"__NSCFConstantString",
            Some(class()),
            size_of::<CFString>(),
            false,
        );
        install_constant_methods(class);
        objc::class::ptr(class) as usize
    });
    // SAFETY: a leaked `Class` this crate registered.
    unsafe { &*(p as *const Class) }
}

/// The methods a string answers: equality by characters, and the length and
/// bytes an `NSString` caller asks for.
///
/// Without `-isEqual:` two strings spelled the same are unequal, which is the
/// first thing toll-free bridging has to get right - a dictionary keyed by
/// string would otherwise never find anything.
fn install_string_methods(class: &'static Class) {
    unsafe extern "C" fn is_equal(
        receiver: objc::object::Id,
        _sel: objc::sel::Sel,
        other: objc::object::Id,
    ) -> bool {
        if receiver == other {
            return true;
        }
        // SAFETY: the receiver is a string; `other` is checked before it is read.
        unsafe {
            match (as_string(receiver as CFTypeRef), as_string(other as CFTypeRef)) {
                (Some(a), Some(b)) => a.utf8() == b.utf8(),
                _ => false,
            }
        }
    }

    unsafe extern "C" fn hash(receiver: objc::object::Id, _sel: objc::sel::Sel) -> usize {
        // SAFETY: the receiver is a string.
        let Some(s) = (unsafe { as_string(receiver as CFTypeRef) }) else {
            return 0;
        };
        // FNV-1a over the characters, so that two strings spelled the same hash
        // the same however they were built. Which function it is does not
        // matter; that it is a function of the *characters* does.
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for byte in s.utf8() {
            h ^= byte as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        h as usize
    }

    unsafe extern "C" fn length(receiver: objc::object::Id, _sel: objc::sel::Sel) -> usize {
        // SAFETY: the receiver is a string.
        unsafe { as_string(receiver as CFTypeRef) }.map_or(0, |s| s.length() as usize)
    }

    unsafe extern "C" fn utf8_string(
        receiver: objc::object::Id,
        _sel: objc::sel::Sel,
    ) -> *const c_char {
        // SAFETY: the receiver is a string.
        unsafe { as_string(receiver as CFTypeRef) }
            .map_or(std::ptr::null(), |s| s.c_string(encoding::UTF8))
    }

    class.add_method(objc::sel::get("isEqual:"), is_equal as *const c_void, c"B24@0:8@16");
    class.add_method(objc::sel::get("hash"), hash as *const c_void, c"Q16@0:8");
    class.add_method(objc::sel::get("length"), length as *const c_void, c"Q16@0:8");
    class.add_method(
        objc::sel::get("UTF8String"),
        utf8_string as *const c_void,
        c"*16@0:8",
    );
    install_dealloc(class);
}

/// `-dealloc` that gives back the bytes before the object.
///
/// Without it the four words are freed and the storage they point at is not,
/// which is a leak per string and so a leak per frame in anything that builds
/// one.
fn install_dealloc(class: &'static Class) {
    unsafe extern "C" fn dealloc(receiver: objc::object::Id, _sel: objc::sel::Sel) {
        // SAFETY: `-dealloc` is only reached with the last reference gone, and
        // the receiver is a `__NSCFString`.
        unsafe {
            release_storage(receiver as *mut CFString);
            objc::arc::finalize(receiver);
        }
    }
    class.replace_method(
        objc::sel::get("dealloc"),
        dealloc as *const c_void,
        c"v16@0:8",
    );
}

/// `-retain`, `-release` and `-dealloc` that do nothing, for a string that lives
/// in an image rather than on the heap.
fn install_constant_methods(class: &'static Class) {
    unsafe extern "C" fn retain(receiver: objc::object::Id, _sel: objc::sel::Sel) -> objc::object::Id {
        receiver
    }
    unsafe extern "C" fn release(_receiver: objc::object::Id, _sel: objc::sel::Sel) {}
    unsafe extern "C" fn retain_count(_receiver: objc::object::Id, _sel: objc::sel::Sel) -> usize {
        // What Apple reports for an object that cannot be deallocated.
        usize::MAX
    }
    class.replace_method(objc::sel::get("retain"), retain as *const c_void, c"@16@0:8");
    class.replace_method(objc::sel::get("release"), release as *const c_void, c"v16@0:8");
    class.replace_method(
        objc::sel::get("autorelease"),
        retain as *const c_void,
        c"@16@0:8",
    );
    class.replace_method(
        objc::sel::get("retainCount"),
        retain_count as *const c_void,
        c"Q16@0:8",
    );
    class.replace_method(objc::sel::get("dealloc"), release as *const c_void, c"v16@0:8");
}

/// `CFStringGetTypeID`.
pub fn type_id_of_strings() -> crate::base::CFTypeID {
    type_id(class())
}

/// `string` as a `CFString`, or `None` if it is not one.
///
/// # Safety
///
/// `string` must be null or an object this runtime allocated.
pub unsafe fn as_string<'a>(string: CFTypeRef) -> Option<&'a CFString> {
    // SAFETY: the caller's obligation.
    let is_string = unsafe {
        crate::base::is_kind_of(string, class()) || crate::base::is_kind_of(string, constant_class())
    };
    // SAFETY: a `__NSCFString` or `__NSCFConstantString` is a `CFString`.
    is_string.then(|| unsafe { &*(string as *const CFString) })
}

impl CFString {
    /// The characters, as UTF-8 bytes.
    ///
    /// A copy for a wide string and a borrow for an 8-bit one, which is why the
    /// return is owned: the two cases have to look the same to a caller.
    pub fn utf8(&self) -> Vec<u8> {
        if self.str.is_null() || self.length <= 0 {
            return Vec::new();
        }
        let count = self.length as usize;
        if self.flags & WIDE == 0 {
            // SAFETY: `str` points at `length` bytes, by the layout above.
            return unsafe { std::slice::from_raw_parts(self.str, count) }.to_vec();
        }
        // SAFETY: `str` points at `length` UTF-16 code units.
        let units = unsafe { std::slice::from_raw_parts(self.str as *const u16, count) };
        String::from_utf16_lossy(units).into_bytes()
    }

    /// Characters, which is what `CFStringGetLength` reports.
    pub fn length(&self) -> CFIndex {
        self.length.max(0) as CFIndex
    }

    /// The bytes as a NUL-terminated C string, if they already are one.
    ///
    /// `CFStringGetCStringPtr`'s contract: null unless the answer is free. A
    /// wide string is never free, and neither is one whose storage this runtime
    /// did not NUL-terminate.
    pub fn c_string(&self, encoding: u32) -> *const c_char {
        match self.flags & WIDE == 0 && is_eight_bit(encoding) && !self.str.is_null() {
            true => self.str as *const c_char,
            false => std::ptr::null(),
        }
    }
}

/// Build a string from 8-bit bytes this runtime will own.
///
/// The storage is NUL-terminated even though the length is carried separately,
/// so that `CFStringGetCStringPtr` can answer without copying - which is the one
/// thing a caller of it is entitled to expect.
pub fn from_bytes(bytes: &[u8], wide: bool) -> CFTypeRef {
    // SAFETY: `CFString` begins with `isa` and is the layout `class` was
    // registered with.
    let object: *mut CFString = unsafe { crate::base::allocate(class()) };
    if object.is_null() {
        return std::ptr::null();
    }
    let (storage, length) = match wide {
        false => {
            let mut owned = bytes.to_vec();
            owned.push(0);
            let length = bytes.len() as i64;
            (owned.into_boxed_slice(), length)
        }
        true => {
            let length = (bytes.len() / 2) as i64;
            let mut owned = bytes.to_vec();
            owned.extend_from_slice(&[0, 0]);
            (owned.into_boxed_slice(), length)
        }
    };
    let str = Box::into_raw(storage) as *const u8;
    // SAFETY: `object` is a live instance of `class()`, so its four words are
    // ours to fill in. The `isa` is already set.
    unsafe {
        (*object).flags = OWNED | if wide { WIDE } else { 0 };
        (*object).str = str;
        (*object).length = length;
    }
    object as CFTypeRef
}

/// Build a string from bytes in `encoding`, or null for an encoding this
/// runtime does not carry a table for.
pub fn from_encoded(bytes: &[u8], encoding: u32) -> CFTypeRef {
    if is_eight_bit(encoding) {
        // Above 0x7F the eight-bit encodings disagree and this runtime has no
        // tables, so a non-ASCII byte under a non-UTF-8 encoding is refused
        // rather than reinterpreted. UTF-8 is passed through whole: its bytes
        // are its bytes, and `length` then counts bytes rather than characters,
        // which is what an all-ASCII string makes true and what every caller in
        // this tree so far relies on.
        if encoding != encoding::UTF8 && bytes.iter().any(|b| *b >= 0x80) {
            return std::ptr::null();
        }
        return from_bytes(bytes, false);
    }
    if is_utf16(encoding) {
        return from_bytes(bytes, true);
    }
    std::ptr::null()
}

/// Free the storage of a string this runtime allocated.
///
/// # Safety
///
/// `object` must be a `__NSCFString` with no remaining references.
pub unsafe fn release_storage(object: *mut CFString) {
    // SAFETY: the caller's obligation.
    let flags = unsafe { (*object).flags };
    if flags & OWNED == 0 {
        return;
    }
    // SAFETY: `str` came from `Box::into_raw` in `from_bytes`, and the length
    // recorded there covers the NUL this frees along with it.
    unsafe {
        let count = match flags & WIDE == 0 {
            true => (*object).length as usize + 1,
            false => (*object).length as usize * 2 + 2,
        };
        drop(Box::from_raw(std::slice::from_raw_parts_mut(
            (*object).str as *mut u8,
            count,
        )));
        (*object).str = std::ptr::null();
        (*object).flags &= !OWNED;
    }
}

/// The bytes of a NUL-terminated C string, or `None` for null.
///
/// # Safety
///
/// `p` must be null or NUL-terminated.
pub unsafe fn cstr_bytes<'a>(p: *const c_char) -> Option<&'a [u8]> {
    // SAFETY: the caller's obligation.
    (!p.is_null()).then(|| unsafe { CStr::from_ptr(p) }.to_bytes())
}
