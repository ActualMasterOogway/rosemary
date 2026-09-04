// The C spellings are the interface; renaming them would only hide it.
#![allow(non_snake_case)]

use std::ffi::{c_char, c_void};

use objc::object::Id;

use crate::base::{CFIndex, CFTypeID, CFTypeRef};
use crate::string;

/// `Boolean`, which is one byte.
pub type Boolean = u8;

/// Everything a `CFCreate` call ignores about its allocator.
///
/// Apple's allocators exist so a caller can account for, or place, the memory a
/// framework takes. Nothing in this tree does either, and a guest that passes
/// `kCFAllocatorDefault` - which is null - is asking for the default one, which
/// is the only one here.
pub type CFAllocatorRef = *const c_void;

/// Hand a foreign object back to the CoreFoundation that made it.
///
/// Returns whether it did. See [`crate::foreign`]: the alternative is reading
/// Apple's object as one of this runtime's, which walks a class structure that
/// is not there.
#[cfg(target_os = "macos")]
unsafe fn forwarded(name: &std::ffi::CStr, object: CFTypeRef) -> bool {
    // SAFETY: the caller's obligation - null or an object.
    if !unsafe { crate::foreign::is(object) } {
        return false;
    }
    // A handle of this tree's that has no isa to read. Nothing to forward and
    // nothing to count: see [`crate::base::is_opaque`].
    if crate::base::is_opaque(object) {
        return true;
    }
    if !crate::foreign::forwarding() {
        objc::fail!(
            "{}: the object is not this runtime's, and ROSETTA3_NO_FOREIGN forbids passing it to \
             the CoreFoundation that made it",
            name.to_string_lossy()
        );
    }
    let Some(f) = crate::foreign::entry(name) else {
        objc::fail!(
            "{}: the object is not this runtime's and there is no host CoreFoundation to pass it \
             to",
            name.to_string_lossy()
        );
    };
    // SAFETY: every entry point forwarded here is `void (*)(CFTypeRef)` in the
    // one respect that matters - one pointer argument - and a return value the
    // caller does not read.
    unsafe {
        let f: unsafe extern "C" fn(CFTypeRef) -> CFTypeRef = std::mem::transmute(f);
        f(object);
    }
    true
}

/// Off Darwin every object the guest holds is this runtime's, so there is
/// nothing to forward and nothing to check for.
#[cfg(not(target_os = "macos"))]
unsafe fn forwarded(_name: &std::ffi::CStr, object: CFTypeRef) -> bool {
    // The opaque handles are here too - `IOHIDDeviceRef` is one on every host -
    // even though a foreign object is not.
    crate::base::is_opaque(object)
}

pub unsafe extern "C" fn CFRetain(object: CFTypeRef) -> CFTypeRef {
    // SAFETY: the caller's obligation.
    if unsafe { forwarded(c"CFRetain", object) } {
        return object;
    }
    objc::arc::retain(object as Id) as CFTypeRef
}

pub unsafe extern "C" fn CFRelease(object: CFTypeRef) {
    // SAFETY: the caller's obligation.
    if unsafe { forwarded(c"CFRelease", object) } {
        return;
    }
    objc::arc::release(object as Id);
}

pub unsafe extern "C" fn CFAutorelease(object: CFTypeRef) -> CFTypeRef {
    // SAFETY: the caller's obligation.
    if unsafe { forwarded(c"CFAutorelease", object) } {
        return object;
    }
    objc::arc::autorelease(object as Id) as CFTypeRef
}

pub unsafe extern "C" fn CFGetRetainCount(object: CFTypeRef) -> CFIndex {
    objc::arc::retain_count(object as Id) as CFIndex
}

pub unsafe extern "C" fn CFGetTypeID(object: CFTypeRef) -> CFTypeID {
    // SAFETY: the caller's obligation - an object this runtime allocated.
    unsafe { crate::base::class_of(object) }.map_or(0, crate::base::type_id_of)
}

/// `CFEqual`, which is `-isEqual:` and nothing more.
///
/// Two strings with the same characters are equal, because `__NSCFString`
/// overrides `-isEqual:`; two objects of any other type are equal when they are
/// the same object, because `NSObject`'s does.
pub unsafe extern "C" fn CFEqual(a: CFTypeRef, b: CFTypeRef) -> Boolean {
    if a == b {
        return 1;
    }
    if a.is_null() || b.is_null() {
        return 0;
    }
    // SAFETY: two live objects; `-isEqual:` is `BOOL (*)(id, SEL, id)`.
    unsafe { crate::send::send1_bool(a as Id, objc::sel::get("isEqual:"), b as usize) as Boolean }
}

pub unsafe extern "C" fn CFHash(object: CFTypeRef) -> usize {
    if object.is_null() {
        return 0;
    }
    // SAFETY: a live object; `-hash` is `NSUInteger (*)(id, SEL)`.
    unsafe { crate::send::send0_usize(object as Id, objc::sel::get("hash")) }
}

//

pub unsafe extern "C" fn CFStringGetTypeID() -> CFTypeID {
    string::type_id_of_strings()
}

pub unsafe extern "C" fn CFStringCreateWithCString(
    _allocator: CFAllocatorRef,
    cstr: *const c_char,
    encoding: u32,
) -> CFTypeRef {
    // SAFETY: the caller's obligation - a NUL-terminated string or null.
    let Some(bytes) = (unsafe { string::cstr_bytes(cstr) }) else {
        return std::ptr::null();
    };
    string::from_encoded(bytes, encoding)
}

/// `CFStringCreateWithCStringNoCopy`, which copies here.
///
/// The "no copy" contract is about the *allocator* being told who frees the
/// buffer, and a guest that passes `kCFAllocatorNull` means "never free it".
/// Copying is correct under every one of those spellings and wrong under none;
/// it only costs the bytes.
pub unsafe extern "C" fn CFStringCreateWithCStringNoCopy(
    allocator: CFAllocatorRef,
    cstr: *const c_char,
    encoding: u32,
    _contents_deallocator: CFAllocatorRef,
) -> CFTypeRef {
    // SAFETY: the caller's obligation.
    unsafe { CFStringCreateWithCString(allocator, cstr, encoding) }
}

pub unsafe extern "C" fn CFStringCreateWithBytes(
    _allocator: CFAllocatorRef,
    bytes: *const u8,
    length: CFIndex,
    encoding: u32,
    _external_representation: Boolean,
) -> CFTypeRef {
    if bytes.is_null() || length < 0 {
        return std::ptr::null();
    }
    // SAFETY: the caller says `bytes` covers `length` bytes.
    let slice = unsafe { std::slice::from_raw_parts(bytes, length as usize) };
    string::from_encoded(slice, encoding)
}

/// `CFStringRef CFStringCreateWithCharacters(CFAllocatorRef, const UniChar *,
/// CFIndex numChars)`.
///
/// The characters are UTF-16 in this machine's order, and are kept that way: a
/// string built from code units is asked for its code units back, and going
/// through UTF-8 would lose an unpaired surrogate that a caller put there.
///
/// No characters is the empty string rather than null - `numChars` of zero is
/// how a caller spells one, and there is nothing to read at the pointer.
pub unsafe extern "C" fn CFStringCreateWithCharacters(
    _allocator: CFAllocatorRef,
    chars: *const u16,
    count: CFIndex,
) -> CFTypeRef {
    if count < 0 || (chars.is_null() && count > 0) {
        return std::ptr::null();
    }
    // SAFETY: the caller says `chars` covers `count` code units.
    let units = match count {
        0 => &[][..],
        _ => unsafe { std::slice::from_raw_parts(chars, count as usize) },
    };
    string::from_utf16(units)
}

/// `CFStringCreateCopy`, which keeps the original's representation.
///
/// A wide string copied to 8-bit would have its `length` become a count of
/// UTF-8 bytes rather than of characters, so a caller that copies a string and
/// asks its length would be told something different about the same characters.
pub unsafe extern "C" fn CFStringCreateCopy(
    _allocator: CFAllocatorRef,
    original: CFTypeRef,
) -> CFTypeRef {
    // SAFETY: the caller's obligation.
    let Some(s) = (unsafe { string::as_string(original) }) else {
        return std::ptr::null();
    };
    match s.is_wide() {
        true => string::from_utf16(&s.utf16()),
        false => string::from_bytes(&s.utf8(), false),
    }
}

pub unsafe extern "C" fn CFStringGetLength(s: CFTypeRef) -> CFIndex {
    // SAFETY: the caller's obligation.
    unsafe { string::as_string(s) }.map_or(0, |s| s.length())
}

/// `CFStringGetMaximumSizeForEncoding`.
///
/// A bound, not an answer: the caller adds one for the NUL and allocates that
/// much. `length` is in UTF-16 code units, so **three** bytes per unit is the
/// UTF-8 bound and not four - a code point that needs four bytes is a surrogate
/// pair and so costs two units. Apple's answers exactly this, and a caller that
/// sizes a buffer here and checks it against a length elsewhere would notice
/// the difference.
pub unsafe extern "C" fn CFStringGetMaximumSizeForEncoding(
    length: CFIndex,
    encoding: u32,
) -> CFIndex {
    if length < 0 {
        return 0;
    }
    match encoding {
        string::encoding::UTF8 => length * 3,
        // Two bytes a unit, and room for a byte-order mark.
        string::encoding::UTF16 | string::encoding::UTF16_BE | string::encoding::UTF16_LE => {
            length * 2 + 2
        }
        _ => length,
    }
}

/// `CFStringGetCString`: copy the characters into the caller's buffer.
///
/// `false` when they do not fit, having written nothing - which is the contract,
/// and the reason a caller that ignores the return prints an empty string rather
/// than a truncated one.
pub unsafe extern "C" fn CFStringGetCString(
    s: CFTypeRef,
    buffer: *mut c_char,
    buffer_size: CFIndex,
    encoding: u32,
) -> Boolean {
    // SAFETY: the caller's obligation.
    let Some(s) = (unsafe { string::as_string(s) }) else {
        return 0;
    };
    if buffer.is_null() || buffer_size <= 0 {
        return 0;
    }
    let bytes = match encoding {
        string::encoding::UTF8 => s.utf8(),
        // Every other encoding this runtime answers for is one where an ASCII
        // byte is itself, and a non-ASCII one has no defined answer here.
        e if e == string::encoding::UTF16
            || e == string::encoding::UTF16_BE
            || e == string::encoding::UTF16_LE =>
        {
            return 0;
        }
        _ => {
            let utf8 = s.utf8();
            if utf8.iter().any(|b| *b >= 0x80) {
                return 0;
            }
            utf8
        }
    };
    if bytes.len() + 1 > buffer_size as usize {
        return 0;
    }
    // SAFETY: the caller says `buffer` covers `buffer_size` bytes, and the copy
    // plus its NUL fits inside that, as just checked.
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), buffer as *mut u8, bytes.len());
        *(buffer as *mut u8).add(bytes.len()) = 0;
    }
    1
}

/// `CFStringGetCStringPtr`: the bytes if they are already there, and null
/// otherwise.
///
/// Null is a legitimate answer and a caller must handle it; one that does not is
/// broken on Apple's CoreFoundation too.
pub unsafe extern "C" fn CFStringGetCStringPtr(s: CFTypeRef, encoding: u32) -> *const c_char {
    // SAFETY: the caller's obligation.
    unsafe { string::as_string(s) }.map_or(std::ptr::null(), |s| s.c_string(encoding))
}

/// `CFStringGetBytes`: the general form, which reports how much it moved.
/// `CFStringGetBytes`: the range's characters in an encoding, and how many of
/// them fit.
///
/// The return is *characters converted*, which is not the byte count a caller
/// gets in `usedBufLen` and is not the whole range either when the buffer runs
/// out - a caller that trusts one for the other reads past what it was given.
pub unsafe extern "C" fn CFStringGetBytes(
    s: CFTypeRef,
    range_location: CFIndex,
    range_length: CFIndex,
    encoding: u32,
    _loss_byte: u8,
    external_representation: Boolean,
    buffer: *mut u8,
    max_buffer_length: CFIndex,
    used_buffer_length: *mut CFIndex,
) -> CFIndex {
    // SAFETY: the caller's obligation.
    let Some(s) = (unsafe { string::as_string(s) }) else {
        return 0;
    };
    let start = range_location.max(0) as usize;
    let length = range_length.max(0) as usize;

    // Bytes to write, and how many bytes one character of them takes: the two
    // together are what turns a truncated buffer back into a character count.
    let (wanted, per_character, prefix) = match encoding {
        string::encoding::UTF8
        | string::encoding::ASCII
        | string::encoding::ISO_LATIN_1
        | string::encoding::MAC_ROMAN
        | string::encoding::WINDOWS_LATIN1 => {
            let utf8 = s.utf8();
            if start > utf8.len() {
                return 0;
            }
            let end = (start + length).min(utf8.len());
            (utf8[start..end].to_vec(), 1, 0)
        }
        string::encoding::UTF16 | string::encoding::UTF16_BE | string::encoding::UTF16_LE => {
            let units = s.units();
            if start > units.len() {
                return 0;
            }
            let end = (start + length).min(units.len());
            // A byte-order mark, and only where Apple writes one: the plain
            // UTF-16 encoding asked for as an external representation. The two
            // explicit orders say their order in their name.
            let mark = external_representation != 0 && encoding == string::encoding::UTF16;
            let big_endian = encoding == string::encoding::UTF16_BE;
            let mut bytes = Vec::with_capacity((end - start) * 2 + 2 * mark as usize);
            for unit in mark
                .then_some(0xFEFF_u16)
                .iter()
                .chain(&units[start..end])
            {
                match big_endian {
                    true => bytes.extend_from_slice(&unit.to_be_bytes()),
                    false => bytes.extend_from_slice(&unit.to_le_bytes()),
                }
            }
            (bytes, 2, 2 * mark as usize)
        }
        // An encoding with no table here converts nothing, which is what a
        // caller checking the return against zero already handles.
        _ => return 0,
    };

    // A null buffer is the caller asking how much room it needs.
    let moved = match buffer.is_null() {
        true => wanted.len(),
        false => {
            let room = max_buffer_length.max(0) as usize;
            // Whole characters only: half of one is not a character, and the
            // count returned below has to name what the caller can read.
            let moved = wanted.len().min(room);
            let moved = match moved >= prefix {
                true => prefix + (moved - prefix) / per_character * per_character,
                false => 0,
            };
            // SAFETY: the caller says `buffer` covers `max_buffer_length`
            // bytes, and `moved` is no more than that.
            unsafe { std::ptr::copy_nonoverlapping(wanted.as_ptr(), buffer, moved) };
            moved
        }
    };
    if !used_buffer_length.is_null() {
        // SAFETY: the caller's out-parameter.
        unsafe { *used_buffer_length = moved as CFIndex };
    }
    (moved.saturating_sub(prefix) / per_character) as CFIndex
}

pub unsafe extern "C" fn CFStringCreateMutable(
    _allocator: CFAllocatorRef,
    _max_length: CFIndex,
) -> CFTypeRef {
    // The capacity is a hint Apple's uses to preallocate and this one ignores;
    // a `CFStringRef` with a maximum length is a thing a guest may then exceed,
    // and truncating silently is worse than reallocating.
    string::new_mutable()
}

/// `CFStringCreateMutableCopy(CFAllocatorRef, CFIndex maxLength, CFStringRef)`.
///
/// The maximum length is ignored for the reason [`CFStringCreateMutable`]
/// ignores it. The characters keep the representation the original had, so that
/// the length of the copy is the length of what it was copied from.
pub unsafe extern "C" fn CFStringCreateMutableCopy(
    _allocator: CFAllocatorRef,
    _max_length: CFIndex,
    original: CFTypeRef,
) -> CFTypeRef {
    // SAFETY: the caller's obligation.
    let Some(s) = (unsafe { string::as_string(original) }) else {
        return std::ptr::null();
    };
    string::new_mutable_copy(s)
}

/// Add to a mutable string in the representation it already has.
///
/// An 8-bit string appended to in code units - or the reverse - would have its
/// `length` start counting something different from what it counted before.
///
/// # Safety
///
/// `target` is null or an object this runtime allocated.
unsafe fn append(target: CFTypeRef, tail_utf8: &[u8]) {
    // SAFETY: the caller's obligation.
    let Some(a) = (unsafe { string::as_string(target) }) else {
        return;
    };
    if a.is_wide() {
        let mut units = a.utf16();
        units.extend(String::from_utf8_lossy(tail_utf8).encode_utf16());
        // SAFETY: as above.
        unsafe { string::set_utf16(target, &units) };
        return;
    }
    let mut joined = a.utf8();
    joined.extend_from_slice(tail_utf8);
    // SAFETY: as above.
    unsafe { string::set_bytes(target, &joined) };
}

pub unsafe extern "C" fn CFStringAppend(target: CFTypeRef, appended: CFTypeRef) {
    // SAFETY: the caller's obligation.
    let (Some(a), Some(b)) = (unsafe { string::as_string(target) }, unsafe {
        string::as_string(appended)
    }) else {
        return;
    };
    // Both wide: the code units go across untouched, so an unpaired surrogate
    // survives the append rather than becoming a replacement character.
    if a.is_wide() && b.is_wide() {
        let mut units = a.utf16();
        units.extend_from_slice(&b.utf16());
        // SAFETY: as above.
        unsafe { string::set_utf16(target, &units) };
        return;
    }
    // SAFETY: as above.
    unsafe { append(target, &b.utf8()) };
}

pub unsafe extern "C" fn CFStringAppendCString(
    target: CFTypeRef,
    cstr: *const c_char,
    _encoding: u32,
) {
    // SAFETY: the caller's obligation.
    // SAFETY: the caller's obligation - a NUL-terminated string or null.
    let Some(bytes) = (unsafe { string::cstr_bytes(cstr) }) else {
        return;
    };
    // SAFETY: as above.
    unsafe { append(target, bytes) };
}

/// `void CFStringUppercase(CFMutableStringRef, CFLocaleRef locale)`.
///
/// The locale is not consulted: this runtime has no case-mapping tables of its
/// own, so the mapping is Unicode's default one. What that costs is the
/// language-specific pairs - a Turkish locale uppercases `i` to `I` here where
/// Apple's gives `U+0130`. Everything else, including the multi-character
/// mappings such as `ss` to `SS`, is the same answer.
pub unsafe extern "C" fn CFStringUppercase(target: CFTypeRef, _locale: CFTypeRef) {
    // SAFETY: the caller's obligation.
    unsafe { recase(target, str::to_uppercase) }
}

/// `void CFStringLowercase(CFMutableStringRef, CFLocaleRef locale)`, with the
/// same treatment of the locale as [`CFStringUppercase`].
pub unsafe extern "C" fn CFStringLowercase(target: CFTypeRef, _locale: CFTypeRef) {
    // SAFETY: the caller's obligation.
    unsafe { recase(target, str::to_lowercase) }
}

/// Rewrite a mutable string through a case mapping, keeping the representation.
///
/// An immutable string is left alone rather than written through: Apple's would
/// be a crash, and there is nothing this can return to say so.
///
/// # Safety
///
/// `target` is null or an object this runtime allocated.
unsafe fn recase(target: CFTypeRef, map: fn(&str) -> String) {
    // SAFETY: the caller's obligation.
    let Some(s) = (unsafe { string::as_string(target) }) else {
        return;
    };
    if s.is_wide() {
        let mapped = map(&String::from_utf16_lossy(&s.utf16()));
        let units: Vec<u16> = mapped.encode_utf16().collect();
        // SAFETY: as above.
        unsafe { string::set_utf16(target, &units) };
        return;
    }
    let mapped = map(&String::from_utf8_lossy(&s.utf8()));
    // SAFETY: as above.
    unsafe { string::set_bytes(target, mapped.as_bytes()) };
}

/// `UniChar CFStringGetCharacterAtIndex(CFStringRef, CFIndex)`.
///
/// Zero for an index outside the string, which Apple's would fault on: there is
/// no character to answer with, and a caller reading past the length it was
/// given has already lost track of the string.
pub unsafe extern "C" fn CFStringGetCharacterAtIndex(s: CFTypeRef, index: CFIndex) -> u16 {
    // SAFETY: the caller's obligation.
    let Some(s) = (unsafe { string::as_string(s) }) else {
        return 0;
    };
    match usize::try_from(index) {
        Ok(index) => s.units().get(index).copied().unwrap_or(0),
        Err(_) => 0,
    }
}

/// `void CFStringGetCharacters(CFStringRef, CFRange, UniChar *buffer)`, with the
/// range flattened into the two words it is passed in.
///
/// A range that is not inside the string writes nothing: the caller sized the
/// buffer from the range, so writing a shorter answer would leave it reading
/// characters it never received, and writing a longer one is the overflow.
pub unsafe extern "C" fn CFStringGetCharacters(
    s: CFTypeRef,
    range_location: CFIndex,
    range_length: CFIndex,
    buffer: *mut u16,
) {
    // SAFETY: the caller's obligation.
    let Some(s) = (unsafe { string::as_string(s) }) else {
        return;
    };
    let (Ok(start), Ok(count)) = (usize::try_from(range_location), usize::try_from(range_length))
    else {
        return;
    };
    let units = s.units();
    if buffer.is_null() || start + count > units.len() {
        return;
    }
    // SAFETY: the caller states `buffer` holds the range's characters, and the
    // range has just been checked against the string.
    unsafe { std::ptr::copy_nonoverlapping(units[start..].as_ptr(), buffer, count) };
}

/// `const UniChar *CFStringGetCharactersPtr(CFStringRef)`.
///
/// The same contract as `CFStringGetCStringPtr`: the characters if they are
/// already there, and null rather than a conversion. An 8-bit string's bytes
/// are not `UniChar`s, so it has none to point at.
pub unsafe extern "C" fn CFStringGetCharactersPtr(s: CFTypeRef) -> *const u16 {
    // SAFETY: the caller's obligation.
    unsafe { string::as_string(s) }.map_or(std::ptr::null(), |s| s.characters_ptr())
}

/// `void CFStringAppendCharacters(CFMutableStringRef, const UniChar *, CFIndex)`.
pub unsafe extern "C" fn CFStringAppendCharacters(
    target: CFTypeRef,
    chars: *const u16,
    count: CFIndex,
) {
    let Ok(count) = usize::try_from(count) else {
        return;
    };
    if chars.is_null() && count > 0 {
        return;
    }
    // SAFETY: the caller says `chars` covers `count` code units.
    let tail = match count {
        0 => &[][..],
        _ => unsafe { std::slice::from_raw_parts(chars, count) },
    };
    // SAFETY: the caller's obligation.
    let Some(a) = (unsafe { string::as_string(target) }) else {
        return;
    };
    if a.is_wide() {
        let mut units = a.utf16();
        units.extend_from_slice(tail);
        // SAFETY: as above.
        unsafe { string::set_utf16(target, &units) };
        return;
    }
    // SAFETY: as above.
    unsafe { append(target, String::from_utf16_lossy(tail).as_bytes()) };
}

pub unsafe extern "C" fn CFStringCompare(a: CFTypeRef, b: CFTypeRef, options: u64) -> CFIndex {
    // SAFETY: the caller's obligation.
    let (Some(a), Some(b)) = (unsafe { string::as_string(a) }, unsafe {
        string::as_string(b)
    }) else {
        return 0;
    };
    let (mut x, mut y) = (a.utf8(), b.utf8());
    // `kCFCompareCaseInsensitive`.
    if options & 1 != 0 {
        x.make_ascii_lowercase();
        y.make_ascii_lowercase();
    }
    match x.cmp(&y) {
        std::cmp::Ordering::Less => -1,
        std::cmp::Ordering::Equal => 0,
        std::cmp::Ordering::Greater => 1,
    }
}

//

use crate::callbacks::{KeyCallBacks, Policy, ValueCallBacks};
use crate::collections;

pub unsafe extern "C" fn CFArrayGetTypeID() -> CFTypeID {
    crate::base::type_id_of(collections::array_class())
}

pub unsafe extern "C" fn CFArrayCreate(
    _allocator: CFAllocatorRef,
    values: *const CFTypeRef,
    count: CFIndex,
    callbacks: *const ValueCallBacks,
) -> CFTypeRef {
    if count < 0 || (values.is_null() && count > 0) {
        return std::ptr::null();
    }
    // SAFETY: the caller says `values` covers `count` entries.
    let slice = match count {
        0 => &[][..],
        _ => unsafe { std::slice::from_raw_parts(values, count as usize) },
    };
    // SAFETY: the caller's obligation - null, or a live callbacks structure.
    collections::new_array(slice, unsafe { Policy::from_value(callbacks) }, false)
}

/// `CFArrayCreateMutable`. The capacity is a hint, and zero means unbounded;
/// this array grows either way, so it is only reserved for.
pub unsafe extern "C" fn CFArrayCreateMutable(
    _allocator: CFAllocatorRef,
    _capacity: CFIndex,
    callbacks: *const ValueCallBacks,
) -> CFTypeRef {
    // SAFETY: the caller's obligation - null, or a live callbacks structure.
    collections::new_array(&[], unsafe { Policy::from_value(callbacks) }, true)
}

pub unsafe extern "C" fn CFArrayAppendValue(array: CFTypeRef, value: CFTypeRef) {
    // SAFETY: the caller's obligation.
    let Some(a) = (unsafe { collections::array(array) }) else {
        return;
    };
    if !a.mutable {
        return;
    }
    a.values.push(a.policy.retain(value));
}

/// The function called by [`CFArrayApplyFunction`] for each value in its range.
pub type ArrayApplierFunction = unsafe extern "C" fn(CFTypeRef, *mut c_void);

/// Apply `callback` to every value in the requested array range.
///
/// `CFRange` is passed as its two `CFIndex` fields so that this Rust entry point
/// has the same System V ABI as the C function taking the aggregate by value.
/// The values are copied before invoking guest code: callbacks must not mutate
/// the array, but this keeps an accidental mutation from invalidating the walk.
pub unsafe extern "C" fn CFArrayApplyFunction(
    array: CFTypeRef,
    range_location: CFIndex,
    range_length: CFIndex,
    callback: Option<ArrayApplierFunction>,
    context: *mut c_void,
) {
    let Some(callback) = callback else {
        return;
    };
    let (Ok(start), Some(end)) = (
        usize::try_from(range_location),
        range_location
            .checked_add(range_length)
            .and_then(|end| usize::try_from(end).ok()),
    ) else {
        return;
    };
    // SAFETY: the caller's obligation.
    let Some(values) = (unsafe { collections::array(array) })
        .and_then(|a| a.values.get(start..end).map(ToOwned::to_owned))
    else {
        return;
    };
    for value in values {
        // SAFETY: `callback` is supplied by the caller with this signature.
        unsafe { callback(value, context) };
    }
}

pub unsafe extern "C" fn CFArrayGetCount(array: CFTypeRef) -> CFIndex {
    // SAFETY: the caller's obligation.
    unsafe { collections::array(array) }.map_or(0, |a| a.values.len() as CFIndex)
}

pub unsafe extern "C" fn CFArrayGetValueAtIndex(array: CFTypeRef, index: CFIndex) -> CFTypeRef {
    // SAFETY: the caller's obligation.
    let Some(a) = (unsafe { collections::array(array) }) else {
        return std::ptr::null();
    };
    match collections::at(index, a.values.len()) {
        Some(at) => a.values[at],
        // Apple's traps here. Answering null is the more useful failure: a guest
        // that walks past the end gets a nil it can see rather than a crash in a
        // frame that says nothing about its own loop.
        None => std::ptr::null(),
    }
}

//

pub unsafe extern "C" fn CFDictionaryGetTypeID() -> CFTypeID {
    crate::base::type_id_of(collections::dictionary_class())
}

pub unsafe extern "C" fn CFDictionaryCreate(
    _allocator: CFAllocatorRef,
    keys: *const CFTypeRef,
    values: *const CFTypeRef,
    count: CFIndex,
    key_callbacks: *const KeyCallBacks,
    value_callbacks: *const ValueCallBacks,
) -> CFTypeRef {
    if count < 0 || ((keys.is_null() || values.is_null()) && count > 0) {
        return std::ptr::null();
    }
    // SAFETY: the caller says both arrays cover `count` entries.
    let pairs: Vec<(CFTypeRef, CFTypeRef)> = match count {
        0 => Vec::new(),
        _ => unsafe {
            std::slice::from_raw_parts(keys, count as usize)
                .iter()
                .copied()
                .zip(
                    std::slice::from_raw_parts(values, count as usize)
                        .iter()
                        .copied(),
                )
                .collect()
        },
    };
    // SAFETY: the caller's obligation - null, or live callbacks structures.
    unsafe {
        collections::new_dictionary(
            &pairs,
            Policy::from_key(key_callbacks),
            Policy::from_value(value_callbacks),
            false,
        )
    }
}

/// `CFDictionaryCreateMutable`, whose capacity is a hint the same way
/// [`CFArrayCreateMutable`]'s is.
pub unsafe extern "C" fn CFDictionaryCreateMutable(
    _allocator: CFAllocatorRef,
    _capacity: CFIndex,
    key_callbacks: *const KeyCallBacks,
    value_callbacks: *const ValueCallBacks,
) -> CFTypeRef {
    // SAFETY: the caller's obligation - null, or live callbacks structures.
    unsafe {
        collections::new_dictionary(
            &[],
            Policy::from_key(key_callbacks),
            Policy::from_value(value_callbacks),
            true,
        )
    }
}

pub unsafe extern "C" fn CFDictionarySetValue(
    dictionary: CFTypeRef,
    key: CFTypeRef,
    value: CFTypeRef,
) {
    // SAFETY: the caller's obligation.
    let Some(d) = (unsafe { collections::dictionary(dictionary) }) else {
        return;
    };
    if !d.mutable {
        return;
    }
    d.set(key, value);
}

pub unsafe extern "C" fn CFDictionaryGetCount(dictionary: CFTypeRef) -> CFIndex {
    // SAFETY: the caller's obligation.
    unsafe { collections::dictionary(dictionary) }.map_or(0, |d| d.entries.len() as CFIndex)
}

pub unsafe extern "C" fn CFDictionaryGetValue(dictionary: CFTypeRef, key: CFTypeRef) -> CFTypeRef {
    // SAFETY: the caller's obligation.
    let Some(d) = (unsafe { collections::dictionary(dictionary) }) else {
        return std::ptr::null();
    };
    d.position(key)
        .map_or(std::ptr::null(), |at| d.entries[at].1)
}

pub unsafe extern "C" fn CFDictionaryContainsKey(dictionary: CFTypeRef, key: CFTypeRef) -> Boolean {
    // SAFETY: the caller's obligation.
    unsafe { collections::dictionary(dictionary) }.is_some_and(|d| d.position(key).is_some())
        as Boolean
}

/// `CFDictionaryGetKeysAndValues`: both sides, in one pass, into the caller's
/// arrays. Either may be null, which means "not interested".
pub unsafe extern "C" fn CFDictionaryGetKeysAndValues(
    dictionary: CFTypeRef,
    keys: *mut CFTypeRef,
    values: *mut CFTypeRef,
) {
    // SAFETY: the caller's obligation.
    let Some(d) = (unsafe { collections::dictionary(dictionary) }) else {
        return;
    };
    for (i, (key, value)) in d.entries.iter().enumerate() {
        // SAFETY: the caller says each non-null array has room for `GetCount`
        // entries, which is what this loop writes.
        unsafe {
            if !keys.is_null() {
                *keys.add(i) = *key;
            }
            if !values.is_null() {
                *values.add(i) = *value;
            }
        }
    }
}

pub unsafe extern "C" fn CFNumberGetTypeID() -> CFTypeID {
    crate::base::type_id_of(collections::number_class())
}

pub unsafe extern "C" fn CFNumberCreate(
    _allocator: CFAllocatorRef,
    kind: i32,
    value: *const c_void,
) -> CFTypeRef {
    let (Some((width, is_float)), false) = (collections::number_shape(kind), value.is_null())
    else {
        return std::ptr::null();
    };
    let mut bytes = [0u8; 8];
    // SAFETY: the caller says `value` points at a number of this type, which is
    // `width` bytes.
    unsafe { std::ptr::copy_nonoverlapping(value as *const u8, bytes.as_mut_ptr(), width) };
    let number = match (is_float, width) {
        (true, 4) => collections::Number::Float(f32::from_le_bytes(
            bytes[..4].try_into().unwrap_or_default(),
        ) as f64),
        (true, _) => collections::Number::Float(f64::from_le_bytes(bytes)),
        // Sign-extended from its own width, so a `SInt8` of -1 is -1 and not 255.
        (false, w) => {
            let raw = i64::from_le_bytes(bytes);
            let shift = 64 - w * 8;
            collections::Number::Integer((raw << shift) >> shift)
        }
    };
    collections::new_number(number)
}

/// `CFNumberGetValue`: `false` when the value does not fit what was asked for.
///
/// Apple's writes a lossy answer *and* returns false; so does this. A caller
/// that ignores the return gets the same wrong number it would get there, which
/// is the only behaviour that makes a working program keep working.
pub unsafe extern "C" fn CFNumberGetValue(
    number: CFTypeRef,
    kind: i32,
    out: *mut c_void,
) -> Boolean {
    // SAFETY: the caller's obligation.
    let (Some(n), Some((width, is_float)), false) = (
        unsafe { collections::number(number) },
        collections::number_shape(kind),
        out.is_null(),
    ) else {
        return 0;
    };
    let (bytes, lossless) = match is_float {
        true => match width {
            4 => {
                let v = n.as_f64();
                let mut b = [0u8; 8];
                b[..4].copy_from_slice(&(v as f32).to_le_bytes());
                (b, (v as f32) as f64 == v)
            }
            _ => (n.as_f64().to_le_bytes(), true),
        },
        false => {
            let v = n.as_i64();
            let shift = 64 - width * 8;
            let fits = width == 8 || ((v << shift) >> shift) == v;
            (v.to_le_bytes(), fits && !n.is_float())
        }
    };
    // SAFETY: the caller says `out` has room for a number of this type.
    unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), out as *mut u8, width) };
    lossless as Boolean
}

pub unsafe extern "C" fn CFNumberIsFloatType(number: CFTypeRef) -> Boolean {
    // SAFETY: the caller's obligation.
    unsafe { collections::number(number) }.is_some_and(|n| n.is_float()) as Boolean
}

pub unsafe extern "C" fn CFNumberGetByteSize(number: CFTypeRef) -> CFIndex {
    // SAFETY: the caller's obligation.
    match unsafe { collections::number(number) } {
        Some(_) => 8,
        None => 0,
    }
}

pub unsafe extern "C" fn CFDataGetTypeID() -> CFTypeID {
    crate::base::type_id_of(collections::data_class())
}

pub unsafe extern "C" fn CFDataCreate(
    _allocator: CFAllocatorRef,
    bytes: *const u8,
    length: CFIndex,
) -> CFTypeRef {
    if length < 0 || (bytes.is_null() && length > 0) {
        return std::ptr::null();
    }
    // SAFETY: the caller says `bytes` covers `length` bytes.
    let slice = match length {
        0 => &[][..],
        _ => unsafe { std::slice::from_raw_parts(bytes, length as usize) },
    };
    collections::new_data(slice.to_vec())
}

/// `CFDataCreateCopy`, which forwards a foreign original rather than reading it.
///
/// The keyboard layout `TISGetInputSourceProperty` hands back is Apple's
/// `CFData`, and the copy of it goes straight back into `UCKeyTranslate` - so
/// the copy has to be Apple's too. See [`crate::foreign`].
pub unsafe extern "C" fn CFDataCreateCopy(
    _allocator: CFAllocatorRef,
    data: CFTypeRef,
) -> CFTypeRef {
    #[cfg(target_os = "macos")]
    // SAFETY: the caller's obligation - null or an object.
    if unsafe { crate::foreign::is(data) } {
        if !crate::foreign::forwarding() {
            objc::fail!(
                "CFDataCreateCopy: the data is not this runtime's, and ROSETTA3_NO_FOREIGN                  forbids passing it to the CoreFoundation that made it"
            );
        }
        let Some(f) = crate::foreign::entry(c"CFDataCreateCopy") else {
            objc::fail!(
                "CFDataCreateCopy: the data is not this runtime's and there is no host                  CoreFoundation to pass it to"
            );
        };
        // SAFETY: the framework's own declaration, and an object it made.
        return unsafe {
            let f: unsafe extern "C" fn(CFAllocatorRef, CFTypeRef) -> CFTypeRef =
                std::mem::transmute(f);
            f(allocator, data)
        };
    }
    // SAFETY: the caller's obligation.
    let Some(bytes) = (unsafe { collections::data(data) }) else {
        return std::ptr::null();
    };
    collections::new_data(bytes.clone())
}

pub unsafe extern "C" fn CFDataGetLength(data: CFTypeRef) -> CFIndex {
    // SAFETY: the caller's obligation.
    unsafe { collections::data(data) }.map_or(0, |d| d.len() as CFIndex)
}

/// The bytes, and for an empty `CFData` a readable byte rather than Rust's.
///
/// `Vec::as_ptr` on a `Vec` that never allocated is the *alignment* - address
/// `1` for a `u8` - which is not null and not readable. A guest that checks the
/// pointer against null sees a valid one and dereferences it, and Apple's
/// `CFDataGetBytePtr` never hands out an address that cannot be read. So an
/// empty `CFData` answers with a byte of this runtime's own instead.
pub unsafe extern "C" fn CFDataGetBytePtr(data: CFTypeRef) -> *const u8 {
    static EMPTY: u8 = 0;
    // SAFETY: the caller's obligation.
    unsafe { collections::data(data) }.map_or(std::ptr::null(), |d| match d.is_empty() {
        true => &raw const EMPTY,
        false => d.as_ptr(),
    })
}

//

pub unsafe extern "C" fn CFURLGetTypeID() -> CFTypeID {
    crate::base::type_id_of(crate::url::class())
}

/// `kCFURLPOSIXPathStyle`, the only one a path in this tree is written in.
const POSIX_PATH_STYLE: CFIndex = 0;

pub unsafe extern "C" fn CFURLCreateWithFileSystemPath(
    _allocator: CFAllocatorRef,
    path: CFTypeRef,
    style: CFIndex,
    directory: Boolean,
) -> CFTypeRef {
    if style != POSIX_PATH_STYLE {
        // HFS and Windows path styles describe a separator and a volume this
        // runtime has no notion of, so a path in one read as POSIX would name a
        // different file rather than fail.
        objc::fail!("CFURLCreateWithFileSystemPath: path style {style} is not POSIX");
    }
    // SAFETY: the caller's obligation - a `CFString` or null.
    let Some(path) = (unsafe { string::as_string(path) }) else {
        return std::ptr::null();
    };
    let bytes = path.utf8();
    let Ok(path) = std::str::from_utf8(&bytes) else {
        return std::ptr::null();
    };
    crate::url::new_file_url(path, directory != 0)
}

/// `CFURLGetFileSystemRepresentation`, which fills `buffer` with a
/// NUL-terminated path and says whether it fit.
///
/// `resolve` is ignored: every URL here is absolute already, so there is no base
/// to resolve against and the two answers are the same one.
pub unsafe extern "C" fn CFURLGetFileSystemRepresentation(
    url: CFTypeRef,
    _resolve: Boolean,
    buffer: *mut u8,
    length: CFIndex,
) -> Boolean {
    // SAFETY: the caller's obligation - a `CFURL` or null.
    let Some(url) = (unsafe { crate::url::as_url(url) }) else {
        return 0;
    };
    if buffer.is_null() || length <= 0 {
        return 0;
    }
    // Only a file URL has a file system representation. Apple's answers false
    // for anything else rather than handing back the path component, which a
    // caller would then try to open.
    let Some(path) = url.file_path() else {
        return 0;
    };
    let path = path.as_bytes();
    // The NUL is part of what has to fit: a caller passes `PATH_MAX` and then
    // reads the buffer as a C string.
    if path.len() >= length as usize {
        return 0;
    }
    // SAFETY: `buffer` holds `length` bytes by the caller's contract, and the
    // check above leaves room for the path and its terminator.
    unsafe {
        std::ptr::copy_nonoverlapping(path.as_ptr(), buffer, path.len());
        *buffer.add(path.len()) = 0;
    }
    1
}

//

pub unsafe extern "C" fn CFUUIDGetTypeID() -> CFTypeID {
    crate::base::type_id_of(crate::uuid::class())
}

pub unsafe extern "C" fn CFUUIDCreate(_allocator: CFAllocatorRef) -> CFTypeRef {
    crate::uuid::new(crate::uuid::generate())
}

/// `CFStringRef CFUUIDCreateString(CFAllocatorRef, CFUUIDRef)`
pub unsafe extern "C" fn CFUUIDCreateString(
    _allocator: CFAllocatorRef,
    uuid: CFTypeRef,
) -> CFTypeRef {
    // SAFETY: the caller's obligation - a `CFUUID` or null.
    let Some(bytes) = (unsafe { crate::uuid::bytes_of(uuid) }) else {
        return std::ptr::null();
    };
    string::from_bytes(crate::uuid::format(&bytes).as_bytes(), false)
}

/// `CFUUIDGetConstantUUIDWithBytes`, whose sixteen bytes are sixteen arguments.
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn CFUUIDGetConstantUUIDWithBytes(
    _allocator: CFAllocatorRef,
    b0: u8,
    b1: u8,
    b2: u8,
    b3: u8,
    b4: u8,
    b5: u8,
    b6: u8,
    b7: u8,
    b8: u8,
    b9: u8,
    b10: u8,
    b11: u8,
    b12: u8,
    b13: u8,
    b14: u8,
    b15: u8,
) -> CFTypeRef {
    crate::uuid::constant([
        b0, b1, b2, b3, b4, b5, b6, b7, b8, b9, b10, b11, b12, b13, b14, b15,
    ])
}

use std::sync::Arc;
use std::time::Duration;

use crate::runloop::{self, RunLoop, Source, Timer};

/// `CFRunLoopSourceContext`, as a guest lays it out.
#[repr(C)]
pub struct CFRunLoopSourceContext {
    pub version: CFIndex,
    pub info: *mut c_void,
    pub retain: *const c_void,
    pub release: *const c_void,
    pub copy_description: *const c_void,
    pub equal: *const c_void,
    pub hash: *const c_void,
    pub schedule: *const c_void,
    pub cancel: *const c_void,
    pub perform: Option<runloop::Perform>,
}

/// `CFRunLoopTimerContext`.
#[repr(C)]
pub struct CFRunLoopTimerContext {
    pub version: CFIndex,
    pub info: *mut c_void,
    pub retain: *const c_void,
    pub release: *const c_void,
    pub copy_description: *const c_void,
}

/// A leaked `Arc`, as the C world holds one.
///
/// Never reclaimed. A `CFRunLoopSourceRef` is owned by the loop it was added to
/// as much as by the guest that made it, and freeing one on the guest's last
/// release would take the loop's out from under it - which is a use-after-free
/// where the leak is a few dozen bytes.
fn leak<T>(value: Arc<T>) -> CFTypeRef {
    Arc::into_raw(value) as CFTypeRef
}

/// Borrow a leaked `Arc` without changing its count.
///
/// `ManuallyDrop`, because the `Arc` is reconstructed from a pointer that is
/// still owned by the C world: letting it drop would release a reference nobody
/// took.
///
/// Anything this crate did not make is `None` rather than a `T`, which is what
/// keeps a source from some other CoreFoundation - IOKit's notification port
/// makes one - from being read as this crate's. See [`runloop::note`].
///
/// # Safety
///
/// `p` must be a pointer [`leak`] produced for a `T`, or one of the stable
/// pointers `CFRunLoopGetCurrent` hands out.
unsafe fn borrow<T: 'static>(p: CFTypeRef) -> Option<std::mem::ManuallyDrop<Arc<T>>> {
    if !runloop::is_noted::<T>(p as usize) {
        return None;
    }
    // SAFETY: the caller's obligation.
    Some(std::mem::ManuallyDrop::new(unsafe {
        Arc::from_raw(p as *const T)
    }))
}

/// A run loop's address, which is stable for as long as its thread runs.
///
/// Not `leak`: `CFRunLoopGetCurrent` is called once a frame by anything with a
/// frame, and handing out an owned reference each time would add one to the
/// count forever. The thread-local holds the loop alive, so borrowing is both
/// sound and stable.
pub unsafe extern "C" fn CFRunLoopGetCurrent() -> CFTypeRef {
    Arc::as_ptr(&runloop::current()) as CFTypeRef
}

pub unsafe extern "C" fn CFRunLoopGetMain() -> CFTypeRef {
    Arc::as_ptr(&runloop::main()) as CFTypeRef
}

/// The mode name a guest passed, as a Rust string.
///
/// # Safety
///
/// `mode` must be null or a `CFStringRef`.
unsafe fn mode_name(mode: CFTypeRef) -> String {
    // SAFETY: the caller's obligation.
    match unsafe { string::as_string(mode) } {
        Some(s) => String::from_utf8_lossy(&s.utf8()).into_owned(),
        // Null is `kCFRunLoopDefaultMode` in every call that takes one.
        None => runloop::DEFAULT_MODE.to_string(),
    }
}

pub unsafe extern "C" fn CFRunLoopRunInMode(
    mode: CFTypeRef,
    seconds: f64,
    return_after_source_handled: Boolean,
) -> i32 {
    // SAFETY: the caller's obligation - a `CFStringRef` or null.
    let (name, loop_) = unsafe { (mode_name(mode), runloop::current()) };
    loop_.run_in_mode(&name, seconds, return_after_source_handled != 0)
}

pub unsafe extern "C" fn CFRunLoopRun() {
    // `CFRunLoopRun` is the default mode with no timeout, until stopped.
    runloop::current().run_in_mode(runloop::DEFAULT_MODE, f64::INFINITY, false);
}

pub unsafe extern "C" fn CFRunLoopStop(loop_: CFTypeRef) {
    // SAFETY: the caller's obligation.
    if let Some(l) = unsafe { borrow::<RunLoop>(loop_) } {
        l.stop();
    }
}

pub unsafe extern "C" fn CFRunLoopWakeUp(loop_: CFTypeRef) {
    // SAFETY: the caller's obligation.
    if let Some(l) = unsafe { borrow::<RunLoop>(loop_) } {
        l.wake();
    }
}

pub unsafe extern "C" fn CFRunLoopSourceCreate(
    _allocator: CFAllocatorRef,
    _order: CFIndex,
    context: *const CFRunLoopSourceContext,
) -> CFTypeRef {
    if context.is_null() {
        return std::ptr::null();
    }
    // SAFETY: the caller's obligation - a live context structure.
    let context = unsafe { &*context };
    if context.version != 0 {
        // A version-1 source is a mach port, and this runtime has no Mach.
        // Accepting one would produce a source that is never fired, which is a
        // program that hangs with nothing anywhere to say why.
        objc::fail!(
            "CFRunLoopSourceCreate: version {} is a mach-port source, which this runtime has no Mach for",
            context.version
        );
    }
    leak(runloop::new_source(context.perform, context.info as usize))
}

pub unsafe extern "C" fn CFRunLoopSourceSignal(source: CFTypeRef) {
    // SAFETY: the caller's obligation.
    if let Some(s) = unsafe { borrow::<Source>(source) } {
        s.signal();
    }
}

pub unsafe extern "C" fn CFRunLoopSourceInvalidate(source: CFTypeRef) {
    // SAFETY: the caller's obligation.
    if let Some(s) = unsafe { borrow::<Source>(source) } {
        s.invalidate();
    }
}

pub unsafe extern "C" fn CFRunLoopSourceIsValid(source: CFTypeRef) -> Boolean {
    // SAFETY: the caller's obligation.
    unsafe { borrow::<Source>(source) }.is_some_and(|s| s.is_valid()) as Boolean
}

pub unsafe extern "C" fn CFRunLoopAddSource(loop_: CFTypeRef, source: CFTypeRef, mode: CFTypeRef) {
    // SAFETY: the caller's obligation.
    let (Some(l), Some(s)) = (unsafe { borrow::<RunLoop>(loop_) }, unsafe {
        borrow::<Source>(source)
    }) else {
        return;
    };
    // SAFETY: as above.
    l.add_source(&s, &unsafe { mode_name(mode) });
}

pub unsafe extern "C" fn CFRunLoopRemoveSource(
    loop_: CFTypeRef,
    source: CFTypeRef,
    mode: CFTypeRef,
) {
    // SAFETY: the caller's obligation.
    let (Some(l), Some(s)) = (unsafe { borrow::<RunLoop>(loop_) }, unsafe {
        borrow::<Source>(source)
    }) else {
        return;
    };
    // SAFETY: as above.
    l.remove_source(&s, &unsafe { mode_name(mode) });
}

pub unsafe extern "C" fn CFRunLoopContainsSource(
    loop_: CFTypeRef,
    source: CFTypeRef,
    mode: CFTypeRef,
) -> Boolean {
    // SAFETY: the caller's obligation.
    let (Some(l), Some(s)) = (unsafe { borrow::<RunLoop>(loop_) }, unsafe {
        borrow::<Source>(source)
    }) else {
        return 0;
    };
    // SAFETY: as above.
    l.contains_source(&s, &unsafe { mode_name(mode) }) as Boolean
}

/// `CFAbsoluteTime` is seconds since 2001; a fire date is one of those.
///
/// Converted to a delay rather than kept as an absolute: the loop measures with
/// a monotonic clock, which is the only one a timer should depend on.
fn delay_until(fire_date: f64) -> Duration {
    let now = absolute_time();
    Duration::from_secs_f64((fire_date - now).max(0.0).min(1e9))
}

/// Seconds since 2001-01-01, which is `CFAbsoluteTimeGetCurrent`.
fn absolute_time() -> f64 {
    const APPLE_EPOCH: f64 = 978_307_200.0;
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64() - APPLE_EPOCH)
        .unwrap_or(0.0)
}

pub unsafe extern "C" fn CFAbsoluteTimeGetCurrent() -> f64 {
    absolute_time()
}

pub unsafe extern "C" fn CFRunLoopTimerCreate(
    _allocator: CFAllocatorRef,
    fire_date: f64,
    interval: f64,
    _flags: u32,
    _order: CFIndex,
    callback: Option<runloop::TimerCallback>,
    context: *mut CFRunLoopTimerContext,
) -> CFTypeRef {
    // SAFETY: the caller's obligation - null, or a live context structure.
    let info = unsafe { context.as_ref() }.map_or(0, |c| c.info as usize);
    // An interval of zero or less is a one-shot, which is what Apple's does.
    let repeat = (interval > 0.0).then(|| Duration::from_secs_f64(interval.min(1e9)));
    leak(runloop::new_timer(
        delay_until(fire_date),
        repeat,
        callback,
        info,
    ))
}

pub unsafe extern "C" fn CFRunLoopAddTimer(loop_: CFTypeRef, timer: CFTypeRef, mode: CFTypeRef) {
    // SAFETY: the caller's obligation.
    let (Some(l), Some(t)) = (unsafe { borrow::<RunLoop>(loop_) }, unsafe {
        borrow::<Timer>(timer)
    }) else {
        return;
    };
    // SAFETY: as above.
    l.add_timer(&t, &unsafe { mode_name(mode) });
}

pub unsafe extern "C" fn CFRunLoopRemoveTimer(loop_: CFTypeRef, timer: CFTypeRef, mode: CFTypeRef) {
    // SAFETY: the caller's obligation.
    let (Some(l), Some(t)) = (unsafe { borrow::<RunLoop>(loop_) }, unsafe {
        borrow::<Timer>(timer)
    }) else {
        return;
    };
    // SAFETY: as above.
    l.remove_timer(&t, &unsafe { mode_name(mode) });
}

pub unsafe extern "C" fn CFRunLoopTimerInvalidate(timer: CFTypeRef) {
    // SAFETY: the caller's obligation.
    if let Some(t) = unsafe { borrow::<Timer>(timer) } {
        t.invalidate();
    }
}

pub unsafe extern "C" fn CFRunLoopTimerIsValid(timer: CFTypeRef) -> Boolean {
    // SAFETY: the caller's obligation.
    unsafe { borrow::<Timer>(timer) }.is_some_and(|t| t.is_valid()) as Boolean
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An empty `CFData` must still answer a byte pointer that can be read.
    /// Rust's own answer is the alignment, which is not null and faults.
    #[test]
    fn an_empty_data_answers_a_readable_pointer() {
        let data = collections::new_data(Vec::new());
        // SAFETY: a `CFData` this test made.
        let (bytes, length) = unsafe { (CFDataGetBytePtr(data), CFDataGetLength(data)) };
        assert_eq!(length, 0);
        assert!(!bytes.is_null());
        assert!(bytes as usize > 0xffff, "{bytes:p} is not a mapped address");
        // SAFETY: the pointer this call just promised is readable.
        assert_eq!(unsafe { bytes.read() }, 0);
    }

    /// Null is still null: an object that is not a `CFData` has no bytes, and a
    /// readable pointer there would be a wrong answer rather than a safe one.
    #[test]
    fn a_non_data_has_no_bytes() {
        // SAFETY: null is the documented "not an object" case.
        assert!(unsafe { CFDataGetBytePtr(std::ptr::null()) }.is_null());
    }
}
