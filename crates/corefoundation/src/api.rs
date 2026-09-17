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

/// The default allocator is represented by null, as it is by the allocator
/// globals this crate publishes. A custom allocator still needs an object of
/// its own: callers retain and release it, even though this CoreFoundation does
/// not route its allocations through the callback table.
fn allocator_class() -> &'static objc::class::Class {
    crate::base::register(c"__NSCFAllocator", None, std::mem::size_of::<usize>(), true)
}

pub unsafe extern "C" fn CFAllocatorGetTypeID() -> CFTypeID {
    crate::base::type_id(allocator_class())
}

pub unsafe extern "C" fn CFAllocatorGetDefault() -> CFAllocatorRef {
    std::ptr::null()
}

pub unsafe extern "C" fn CFAllocatorCreate(
    _allocator: CFAllocatorRef,
    _context: *const c_void,
) -> CFAllocatorRef {
    // The context's callbacks are deliberately not copied: every constructor
    // in this implementation owns its storage and therefore treats an
    // allocator argument as advisory. The returned object preserves the part
    // of the contract guests can observe here: distinct identity and CF/ObjC
    // retain/release behavior.
    objc::object::create_instance(allocator_class(), 0) as CFAllocatorRef
}

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
    if std::env::var_os("ROSETTA3_TRACE_FOREIGN_CF").is_some() {
        eprintln!(
            "rosetta3: foreign CoreFoundation {}({object:p})",
            name.to_string_lossy()
        );
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
    if crate::base::is_opaque(object) {
        return 1;
    }
    objc::arc::retain_count(object as Id) as CFIndex
}

pub unsafe extern "C" fn CFGetTypeID(object: CFTypeRef) -> CFTypeID {
    if let Some(type_id) = crate::base::opaque_type_id(object) {
        return type_id;
    }
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
    if crate::base::is_opaque(a) || crate::base::is_opaque(b) {
        return 0;
    }
    // SAFETY: two live objects; `-isEqual:` is `BOOL (*)(id, SEL, id)`.
    unsafe { crate::send::send1_bool(a as Id, objc::sel::get("isEqual:"), b as usize) as Boolean }
}

pub unsafe extern "C" fn CFHash(object: CFTypeRef) -> usize {
    if object.is_null() {
        return 0;
    }
    if crate::base::is_opaque(object) {
        return object as usize;
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

pub unsafe extern "C" fn CFStringCreateWithFileSystemRepresentation(
    _allocator: CFAllocatorRef,
    path: *const c_char,
) -> CFTypeRef {
    // SAFETY: the caller's obligation - a NUL-terminated filesystem path.
    let Some(bytes) = (unsafe { string::cstr_bytes(path) }) else {
        return std::ptr::null();
    };
    string::from_bytes(bytes, false)
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

/// `CFStringCreateWithSubstring`, whose range is measured in UTF-16 units.
pub unsafe extern "C" fn CFStringCreateWithSubstring(
    _allocator: CFAllocatorRef,
    source: CFTypeRef,
    location: CFIndex,
    length: CFIndex,
) -> CFTypeRef {
    let (Ok(location), Ok(length)) = (usize::try_from(location), usize::try_from(length)) else {
        return std::ptr::null();
    };
    // SAFETY: the caller's obligation - a live string or null.
    let Some(source) = (unsafe { string::as_string(source) }) else {
        return std::ptr::null();
    };
    let units = source.utf16();
    let Some(end) = location
        .checked_add(length)
        .filter(|end| *end <= units.len())
    else {
        return std::ptr::null();
    };
    string::from_utf16(&units[location..end])
}

pub unsafe extern "C" fn CFStringGetLength(s: CFTypeRef) -> CFIndex {
    // SAFETY: the caller's obligation.
    unsafe { string::as_string(s) }.map_or(0, |s| s.length())
}

pub unsafe extern "C" fn CFStringGetIntValue(value: CFTypeRef) -> i32 {
    // SAFETY: the caller's obligation - a string or null.
    let Some(value) = (unsafe { string::as_string(value) }) else {
        return 0;
    };
    let bytes = value.utf8();
    let text = String::from_utf8_lossy(&bytes);
    let text = text.trim_start();
    let end = text
        .char_indices()
        .position(|(index, ch)| !(ch.is_ascii_digit() || (index == 0 && matches!(ch, '+' | '-'))))
        .unwrap_or(text.len());
    text[..end].parse::<i64>().map_or(0, |number| number as i32)
}

pub unsafe extern "C" fn CFStringGetDoubleValue(value: CFTypeRef) -> f64 {
    // SAFETY: the caller's obligation - a string or null.
    let Some(value) = (unsafe { string::as_string(value) }) else {
        return 0.0;
    };
    let bytes = value.utf8();
    let text = String::from_utf8_lossy(&bytes);
    let text = text.trim_start();
    let end = text
        .char_indices()
        .position(|(_, ch)| !(ch.is_ascii_digit() || matches!(ch, '+' | '-' | '.' | 'e' | 'E')))
        .unwrap_or(text.len());
    text[..end].parse().unwrap_or(0.0)
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
        string::encoding::UTF32 | string::encoding::UTF32_BE | string::encoding::UTF32_LE => {
            length * 4 + 4
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
            || e == string::encoding::UTF16_LE
            || e == string::encoding::UTF32
            || e == string::encoding::UTF32_BE
            || e == string::encoding::UTF32_LE =>
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
            for unit in mark.then_some(0xFEFF_u16).iter().chain(&units[start..end]) {
                match big_endian {
                    true => bytes.extend_from_slice(&unit.to_be_bytes()),
                    false => bytes.extend_from_slice(&unit.to_le_bytes()),
                }
            }
            (bytes, 2, 2 * mark as usize)
        }
        string::encoding::UTF32 | string::encoding::UTF32_BE | string::encoding::UTF32_LE => {
            // Characters here are UTF-16 units, so a range may split a pair;
            // the half is dropped rather than written as an invalid scalar.
            let units = s.units();
            if start > units.len() {
                return 0;
            }
            let end = (start + length).min(units.len());
            let mark = external_representation != 0 && encoding == string::encoding::UTF32;
            let big_endian = encoding == string::encoding::UTF32_BE;
            let scalars = char::decode_utf16(units[start..end].iter().copied())
                .filter_map(Result::ok)
                .map(u32::from);
            let mut bytes = Vec::new();
            for scalar in mark.then_some(0xFEFF_u32).into_iter().chain(scalars) {
                match big_endian {
                    true => bytes.extend_from_slice(&scalar.to_be_bytes()),
                    false => bytes.extend_from_slice(&scalar.to_le_bytes()),
                }
            }
            (bytes, 4, 4 * mark as usize)
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

pub unsafe extern "C" fn CFStringCreateExternalRepresentation(
    _allocator: CFAllocatorRef,
    string: CFTypeRef,
    encoding: u32,
    loss_byte: u8,
) -> CFTypeRef {
    // SAFETY: the caller's obligation - a string or null.
    let Some(value) = (unsafe { crate::string::as_string(string) }) else {
        return std::ptr::null();
    };
    let length = value.length();
    let mut used = 0;
    // SAFETY: a sizing call writes only the local `used` value.
    let converted = unsafe {
        CFStringGetBytes(
            string,
            0,
            length,
            encoding,
            loss_byte,
            1,
            std::ptr::null_mut(),
            0,
            &mut used,
        )
    };
    if converted != length || used < 0 {
        return std::ptr::null();
    }
    let mut bytes = vec![0; used as usize];
    // SAFETY: the buffer has the exact size the sizing call returned.
    let converted = unsafe {
        CFStringGetBytes(
            string,
            0,
            length,
            encoding,
            loss_byte,
            1,
            bytes.as_mut_ptr(),
            used,
            &mut used,
        )
    };
    if converted != length {
        return std::ptr::null();
    }
    collections::new_data(bytes)
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

/// Insert a string at a character boundary in a mutable string.
pub unsafe extern "C" fn CFStringInsert(target: CFTypeRef, index: CFIndex, inserted: CFTypeRef) {
    let Ok(index) = usize::try_from(index) else {
        return;
    };
    // Read the inserted value first because it is allowed to alias `target`.
    // SAFETY: the caller's obligation.
    let Some(inserted) = (unsafe { string::as_string(inserted) }) else {
        return;
    };
    let inserted_wide = inserted.is_wide();
    let inserted_units = inserted_wide.then(|| inserted.utf16());
    let inserted_bytes = (!inserted_wide).then(|| inserted.utf8());
    let Some(target_value) = (unsafe { string::as_string(target) }) else {
        return;
    };
    if target_value.is_wide() {
        let mut units = target_value.utf16();
        if index > units.len() {
            return;
        }
        let addition = inserted_units.unwrap_or_else(|| {
            String::from_utf8_lossy(inserted_bytes.as_deref().unwrap_or_default())
                .encode_utf16()
                .collect()
        });
        units.splice(index..index, addition);
        // SAFETY: the caller's target is a mutable string.
        unsafe { string::set_utf16(target, &units) };
        return;
    }
    let mut bytes = target_value.utf8();
    if index > bytes.len() {
        return;
    }
    let addition = inserted_bytes.unwrap_or_else(|| {
        String::from_utf16_lossy(inserted_units.as_deref().unwrap_or_default()).into_bytes()
    });
    bytes.splice(index..index, addition);
    // SAFETY: the caller's target is a mutable string.
    unsafe { string::set_bytes(target, &bytes) };
}

pub unsafe extern "C" fn CFStringDelete(target: CFTypeRef, location: CFIndex, length: CFIndex) {
    let (Ok(location), Ok(length)) = (usize::try_from(location), usize::try_from(length)) else {
        return;
    };
    // SAFETY: the caller's obligation - a live mutable string or null.
    let Some(value) = (unsafe { string::as_string(target) }) else {
        return;
    };
    if value.is_wide() {
        let mut units = value.utf16();
        let Some(end) = location
            .checked_add(length)
            .filter(|end| *end <= units.len())
        else {
            return;
        };
        units.drain(location..end);
        // SAFETY: the caller's target is a mutable string.
        unsafe { string::set_utf16(target, &units) };
        return;
    }
    let mut bytes = value.utf8();
    let Some(end) = location
        .checked_add(length)
        .filter(|end| *end <= bytes.len())
    else {
        return;
    };
    bytes.drain(location..end);
    // SAFETY: the caller's target is a mutable string.
    unsafe { string::set_bytes(target, &bytes) };
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
    let (Ok(start), Ok(count)) = (
        usize::try_from(range_location),
        usize::try_from(range_length),
    ) else {
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

#[repr(C)]
pub struct CFRange {
    location: CFIndex,
    length: CFIndex,
}

/// Find a string inside a caller-selected UTF-16 range.
pub unsafe extern "C" fn CFStringFindWithOptions(
    searched: CFTypeRef,
    needle: CFTypeRef,
    range_location: CFIndex,
    range_length: CFIndex,
    options: u64,
    result: *mut CFRange,
) -> Boolean {
    let (Ok(start), Ok(length)) = (
        usize::try_from(range_location),
        usize::try_from(range_length),
    ) else {
        return 0;
    };
    // SAFETY: the caller's obligation - live strings or null.
    let (Some(searched), Some(needle)) = (unsafe { string::as_string(searched) }, unsafe {
        string::as_string(needle)
    }) else {
        return 0;
    };
    let mut haystack = searched.utf16();
    let mut needle = needle.utf16();
    let Some(end) = start
        .checked_add(length)
        .filter(|end| *end <= haystack.len())
    else {
        return 0;
    };
    const CASE_INSENSITIVE: u64 = 1;
    const BACKWARDS: u64 = 4;
    const ANCHORED: u64 = 8;
    if options & CASE_INSENSITIVE != 0 {
        // ASCII case folding preserves UTF-16 indices, unlike a full Unicode
        // case mapping whose expansions would make the returned range wrong.
        for unit in haystack.iter_mut().chain(needle.iter_mut()) {
            if let Ok(byte) = u8::try_from(*unit) {
                *unit = u16::from(byte.to_ascii_lowercase());
            }
        }
    }
    if needle.len() > length {
        return 0;
    }
    let mut candidates = start..=end - needle.len();
    let found = if options & ANCHORED != 0 {
        let index = if options & BACKWARDS != 0 {
            end - needle.len()
        } else {
            start
        };
        (haystack[index..index + needle.len()] == needle).then_some(index)
    } else if options & BACKWARDS != 0 {
        candidates
            .rev()
            .find(|index| haystack[*index..*index + needle.len()] == needle)
    } else {
        candidates.find(|index| haystack[*index..*index + needle.len()] == needle)
    };
    let Some(location) = found else {
        return 0;
    };
    if !result.is_null() {
        // SAFETY: the caller supplied writable storage for one `CFRange`.
        unsafe {
            *result = CFRange {
                location: location as CFIndex,
                length: needle.len() as CFIndex,
            }
        };
    }
    1
}

/// `CFRange` is flattened to its two `CFIndex` fields by both 64-bit ABIs.
pub unsafe extern "C" fn CFStringCompareWithOptions(
    a: CFTypeRef,
    b: CFTypeRef,
    location: CFIndex,
    length: CFIndex,
    options: u64,
) -> CFIndex {
    // SAFETY: the caller's obligation.
    let (Some(a), Some(b)) = (unsafe { string::as_string(a) }, unsafe {
        string::as_string(b)
    }) else {
        return 0;
    };
    let (Ok(start), Some(end)) = (
        usize::try_from(location),
        location
            .checked_add(length)
            .and_then(|end| usize::try_from(end).ok()),
    ) else {
        return 0;
    };
    let units = a.utf16();
    let Some(range) = units.get(start..end) else {
        return 0;
    };
    let mut x = String::from_utf16_lossy(range).into_bytes();
    let mut y = b.utf8();
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

pub unsafe extern "C" fn CFSetGetTypeID() -> CFTypeID {
    crate::base::type_id_of(collections::set_class())
}

pub unsafe extern "C" fn CFSetGetCount(set: CFTypeRef) -> CFIndex {
    // SAFETY: the caller's obligation.
    unsafe { collections::set(set) }.map_or(0, |set| set.values.len() as CFIndex)
}

pub unsafe extern "C" fn CFSetGetValues(set: CFTypeRef, values: *mut CFTypeRef) {
    if values.is_null() {
        return;
    }
    // SAFETY: the caller's obligation.
    let Some(set) = (unsafe { collections::set(set) }) else {
        return;
    };
    // SAFETY: the caller provides room for CFSetGetCount entries.
    unsafe { std::ptr::copy_nonoverlapping(set.values.as_ptr(), values, set.values.len()) };
}

pub unsafe extern "C" fn CFSetApplyFunction(
    set: CFTypeRef,
    callback: Option<ArrayApplierFunction>,
    context: *mut c_void,
) {
    let Some(callback) = callback else {
        return;
    };
    // SAFETY: the caller's obligation. Copy before entering guest code.
    let Some(values) = (unsafe { collections::set(set) }).map(|set| set.values.clone()) else {
        return;
    };
    for value in values {
        // SAFETY: the caller supplies this CFSetApplierFunction.
        unsafe { callback(value, context) };
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

pub unsafe extern "C" fn CFDictionaryAddValue(
    dictionary: CFTypeRef,
    key: CFTypeRef,
    value: CFTypeRef,
) {
    // Core Foundation documents an existing key as a programming error. This
    // runtime leaves its value alone, which preserves Add rather than turning
    // it into Set in release guests.
    // SAFETY: the caller's obligation.
    let Some(d) = (unsafe { collections::dictionary(dictionary) }) else {
        return;
    };
    if !d.mutable || d.position(key).is_some() {
        return;
    }
    d.set(key, value);
}

pub unsafe extern "C" fn CFDictionaryGetCount(dictionary: CFTypeRef) -> CFIndex {
    // SAFETY: the caller's obligation.
    unsafe { collections::dictionary(dictionary) }.map_or(0, |d| d.entries.len() as CFIndex)
}

pub type DictionaryApplierFunction = unsafe extern "C" fn(CFTypeRef, CFTypeRef, *mut c_void);

pub unsafe extern "C" fn CFDictionaryApplyFunction(
    dictionary: CFTypeRef,
    callback: Option<DictionaryApplierFunction>,
    context: *mut c_void,
) {
    let Some(callback) = callback else {
        return;
    };
    // Snapshot before invoking guest code, which may mutate the dictionary.
    // SAFETY: the caller's obligation - a dictionary or null.
    let Some(entries) = (unsafe { collections::dictionary(dictionary) })
        .map(|dictionary| dictionary.entries.clone())
    else {
        return;
    };
    for (key, value) in entries {
        // SAFETY: the callback is supplied by the caller with this signature.
        unsafe { callback(key, value, context) };
    }
}

pub unsafe extern "C" fn CFDictionaryGetValue(dictionary: CFTypeRef, key: CFTypeRef) -> CFTypeRef {
    // SAFETY: the caller's obligation.
    let Some(d) = (unsafe { collections::dictionary(dictionary) }) else {
        return std::ptr::null();
    };
    d.position(key)
        .map_or(std::ptr::null(), |at| d.entries[at].1)
}

pub unsafe extern "C" fn CFDictionaryGetValueIfPresent(
    dictionary: CFTypeRef,
    key: CFTypeRef,
    value: *mut CFTypeRef,
) -> Boolean {
    // SAFETY: the caller's obligation.
    let Some(d) = (unsafe { collections::dictionary(dictionary) }) else {
        return 0;
    };
    let Some(at) = d.position(key) else {
        return 0;
    };
    if !value.is_null() {
        // SAFETY: the caller supplies a writable result pointer when non-null.
        unsafe { *value = d.entries[at].1 };
    }
    1
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

pub unsafe extern "C" fn CFBooleanGetTypeID() -> CFTypeID {
    crate::base::type_id_of(collections::boolean_class())
}

pub unsafe extern "C" fn CFBooleanGetValue(value: CFTypeRef) -> Boolean {
    (value == collections::boolean(true)) as Boolean
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
    collections::new_number_of_type(number, kind)
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

/// `CFNumberGetType`: the canonical type the number was created with, which is
/// one of the six `SInt`/`Float` storage types whatever C name it was given.
///
/// Zero, which is no type, for anything that is not a number.
pub unsafe extern "C" fn CFNumberGetType(number: CFTypeRef) -> CFIndex {
    // SAFETY: the caller's obligation.
    unsafe { collections::number_type(number) }.map_or(0, |kind| kind as CFIndex)
}

/// The width of the number's canonical type.
pub unsafe extern "C" fn CFNumberGetByteSize(number: CFTypeRef) -> CFIndex {
    // SAFETY: the caller's obligation.
    unsafe { collections::number_type(number) }
        .and_then(collections::number_shape)
        .map_or(0, |(width, _)| width as CFIndex)
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

/// `CFDataCreateWithBytesNoCopy` with an owned compatibility copy.
///
/// CoreFoundation can borrow the caller's buffer and later release it through
/// `contents_deallocator`. This runtime's data objects own a `Vec`, so copying
/// preserves the bytes for the object's lifetime without trying to interpret a
/// foreign allocator object.
pub unsafe extern "C" fn CFDataCreateWithBytesNoCopy(
    allocator: CFAllocatorRef,
    bytes: *const u8,
    length: CFIndex,
    _contents_deallocator: CFAllocatorRef,
) -> CFTypeRef {
    // SAFETY: the validity requirement is identical to `CFDataCreate`'s.
    unsafe { CFDataCreate(allocator, bytes, length) }
}

/// `CFDataCreateCopy`, which forwards a foreign original rather than reading it.
///
/// The keyboard layout `TISGetInputSourceProperty` hands back is Apple's
/// `CFData`, and the copy of it goes straight back into `UCKeyTranslate` - so
/// the copy has to be Apple's too. See [`crate::foreign`].
pub unsafe extern "C" fn CFDataCreateCopy(allocator: CFAllocatorRef, data: CFTypeRef) -> CFTypeRef {
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
    // silence clippy warning on non-macos
    let _ = allocator;
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
/// `CFMutableDataRef CFDataCreateMutable(CFAllocatorRef, CFIndex capacity)`
///
/// The capacity is a hint, and a zero one means "no limit" - neither is a length,
/// so the data starts empty either way.
pub unsafe extern "C" fn CFDataCreateMutable(
    _allocator: CFAllocatorRef,
    capacity: CFIndex,
) -> CFTypeRef {
    let mut bytes = Vec::new();
    if let Ok(capacity) = usize::try_from(capacity) {
        bytes.reserve(capacity.min(1 << 24));
    }
    collections::new_data(bytes)
}

/// `void CFDataGetBytes(CFDataRef, CFRange, UInt8 *buffer)`, with the range
/// flattened into the two words it is passed in.
///
/// A range that is not inside the data copies nothing, for
/// [`CFStringGetCharacters`]' reason: the caller sized its buffer from the range it
/// asked for.
///
/// # Safety
///
/// `buffer` is writable for `range_length` bytes.
pub unsafe extern "C" fn CFDataGetBytes(
    data: CFTypeRef,
    range_location: CFIndex,
    range_length: CFIndex,
    buffer: *mut u8,
) {
    // SAFETY: the caller's obligation.
    let Some(bytes) = (unsafe { collections::data(data) }) else {
        return;
    };
    let (Ok(start), Ok(count)) = (
        usize::try_from(range_location),
        usize::try_from(range_length),
    ) else {
        return;
    };
    if buffer.is_null() || start.saturating_add(count) > bytes.len() {
        return;
    }
    // SAFETY: the caller states `buffer` holds the range, which has just been
    // checked against the data.
    unsafe { std::ptr::copy_nonoverlapping(bytes[start..].as_ptr(), buffer, count) };
}

/// `CFStringEncoding CFStringGetSystemEncoding(void)`
///
/// UTF-8. On a Mac this is the encoding of the user's legacy script system, which
/// is MacRoman on a machine that has never changed it; here every string this
/// runtime builds is UTF-8 and every path the host takes is bytes, so UTF-8 is
/// both the truth and what a caller round-tripping text through it needs.
pub unsafe extern "C" fn CFStringGetSystemEncoding() -> u32 {
    string::encoding::UTF8
}

/// `Boolean CFArrayContainsValue(CFArrayRef, CFRange, const void *)`, with the
/// range flattened.
///
/// Compared with the array's own equality callback, which is what makes a search
/// for a string find an equal one rather than only the same object.
///
/// # Safety
///
/// Both arguments are null or objects this runtime allocated.
pub unsafe extern "C" fn CFArrayContainsValue(
    array: CFTypeRef,
    range_location: CFIndex,
    range_length: CFIndex,
    value: CFTypeRef,
) -> Boolean {
    // SAFETY: the caller's obligation.
    let Some(contents) = (unsafe { collections::array(array) }) else {
        return 0;
    };
    let (Ok(start), Ok(count)) = (
        usize::try_from(range_location),
        usize::try_from(range_length),
    ) else {
        return 0;
    };
    let end = start.saturating_add(count).min(contents.values.len());
    contents
        .values
        .get(start..end)
        .is_some_and(|slice| slice.iter().any(|v| contents.policy.equal(*v, value))) as Boolean
}

/// `void CFArrayAppendArray(CFMutableArrayRef, CFArrayRef, CFRange)`, with the
/// range flattened.
///
/// # Safety
///
/// As [`CFArrayContainsValue`].
pub unsafe extern "C" fn CFArrayAppendArray(
    array: CFTypeRef,
    other: CFTypeRef,
    range_location: CFIndex,
    range_length: CFIndex,
) {
    // SAFETY: the caller's obligation.
    let Some(source) = (unsafe { collections::array(other) }) else {
        return;
    };
    let (Ok(start), Ok(count)) = (
        usize::try_from(range_location),
        usize::try_from(range_length),
    ) else {
        return;
    };
    let end = start.saturating_add(count).min(source.values.len());
    let Some(taken) = source.values.get(start..end).map(<[_]>::to_vec) else {
        return;
    };
    for value in taken {
        // SAFETY: a value from the other array and the caller's destination; the
        // append refuses an immutable array by name.
        unsafe { CFArrayAppendValue(array, value) };
    }
}

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

/// Turn Carbon's opaque `FSRef` into a URL owned by this CoreFoundation.
/// Only the host knows the reference's layout, so it is asked only to decode
/// the path; the URL object itself remains in this runtime's object model.
pub unsafe extern "C" fn CFURLCreateFromFSRef(
    _allocator: CFAllocatorRef,
    fs_ref: *const c_void,
) -> CFTypeRef {
    if fs_ref.is_null() {
        return std::ptr::null();
    }
    #[cfg(target_os = "macos")]
    {
        let Some(entry) = crate::foreign::entry(c"FSRefMakePath") else {
            return std::ptr::null();
        };
        // SAFETY: the Carbon entry point has the documented
        // `OSStatus(const FSRef *, UInt8 *, UInt32)` signature.
        let make_path: unsafe extern "C" fn(*const c_void, *mut u8, u32) -> i32 =
            unsafe { std::mem::transmute(entry) };
        let mut bytes = [0_u8; libc::PATH_MAX as usize];
        // SAFETY: `fs_ref` is the caller's live reference and `bytes` is a
        // writable buffer whose exact capacity accompanies it.
        if unsafe { make_path(fs_ref, bytes.as_mut_ptr(), bytes.len() as u32) } != 0 {
            return std::ptr::null();
        }
        let Some(end) = bytes.iter().position(|byte| *byte == 0) else {
            return std::ptr::null();
        };
        let path = String::from_utf8_lossy(&bytes[..end]);
        let directory = std::fs::metadata(path.as_ref()).is_ok_and(|metadata| metadata.is_dir());
        return crate::url::new_file_url(&path, directory);
    }
    #[cfg(not(target_os = "macos"))]
    std::ptr::null()
}

pub unsafe extern "C" fn CFURLGetFSRef(url: CFTypeRef, fs_ref: *mut c_void) -> Boolean {
    if fs_ref.is_null() {
        return 0;
    }
    // SAFETY: the caller's obligation - a URL or null.
    let Some(path) = (unsafe { crate::url::as_url(url) }).and_then(|url| url.file_path()) else {
        return 0;
    };
    let Ok(path) = std::ffi::CString::new(path) else {
        return 0;
    };
    #[cfg(not(target_os = "macos"))]
    let _ = path;
    #[cfg(target_os = "macos")]
    {
        let Some(entry) = crate::foreign::entry(c"FSPathMakeRef") else {
            return 0;
        };
        // SAFETY: the host entry has the documented Carbon signature; the
        // guest supplied storage for an `FSRef`, whose layout is architecture
        // independent opaque bytes.
        let make_ref: unsafe extern "C" fn(*const u8, *mut c_void, *mut u8) -> i32 =
            unsafe { std::mem::transmute(entry) };
        return u8::from(
            unsafe { make_ref(path.as_ptr().cast(), fs_ref, std::ptr::null_mut()) } == 0,
        );
    }
    #[cfg(not(target_os = "macos"))]
    0
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

pub unsafe extern "C" fn CFURLCreateWithFileSystemPathRelativeToBase(
    _allocator: CFAllocatorRef,
    path: CFTypeRef,
    style: CFIndex,
    directory: Boolean,
    base: CFTypeRef,
) -> CFTypeRef {
    if style != POSIX_PATH_STYLE {
        objc::fail!("CFURLCreateWithFileSystemPathRelativeToBase: path style {style} is not POSIX");
    }
    // SAFETY: the caller's obligation - a string and optional URL.
    let Some(path) = (unsafe { string::as_string(path) }) else {
        return std::ptr::null();
    };
    let path_bytes = path.utf8();
    let Ok(path) = std::str::from_utf8(&path_bytes) else {
        return std::ptr::null();
    };
    if std::path::Path::new(path).is_absolute() {
        return crate::url::new_file_url(path, directory != 0);
    }
    let Some(base_path) = (unsafe { crate::url::as_url(base) }).and_then(|base| base.file_path())
    else {
        return crate::url::new_file_url(path, directory != 0);
    };
    let joined = std::path::Path::new(&base_path).join(path);
    joined.to_str().map_or(std::ptr::null(), |path| {
        crate::url::new_file_url(path, directory != 0)
    })
}

pub unsafe extern "C" fn CFURLCreateWithString(
    _allocator: CFAllocatorRef,
    text: CFTypeRef,
    base: CFTypeRef,
) -> CFTypeRef {
    // SAFETY: the caller's obligation - a string and an optional URL.
    let Some(text) = (unsafe { string::as_string(text) }) else {
        return std::ptr::null();
    };
    let text_bytes = text.utf8();
    let Ok(text) = std::str::from_utf8(&text_bytes) else {
        return std::ptr::null();
    };
    let Some(base) = (unsafe { crate::url::as_url(base) }) else {
        return crate::url::new(text);
    };
    if text.contains(':') {
        return crate::url::new(text);
    }
    let absolute = if text.starts_with('/') {
        match base.text.find("://") {
            Some(scheme_end) => {
                let authority_end = base.text[scheme_end + 3..]
                    .find('/')
                    .map_or(base.text.len(), |slash| scheme_end + 3 + slash);
                format!("{}{text}", &base.text[..authority_end])
            }
            None => text.to_string(),
        }
    } else {
        let directory_end = base
            .text
            .rfind('/')
            .map_or(base.text.len(), |slash| slash + 1);
        format!("{}{text}", &base.text[..directory_end])
    };
    crate::url::new(&absolute)
}

pub unsafe extern "C" fn CFURLGetString(url: CFTypeRef) -> CFTypeRef {
    // SAFETY: the caller's obligation - a URL or null.
    unsafe { crate::url::as_url(url) }.map_or(std::ptr::null(), crate::url::Contents::string)
}

pub unsafe extern "C" fn CFURLCopyAbsoluteURL(url: CFTypeRef) -> CFTypeRef {
    // URLs are stored absolute after construction, so a copy preserves text.
    // SAFETY: the caller's obligation - a URL or null.
    unsafe { crate::url::as_url(url) }.map_or(std::ptr::null(), |url| crate::url::new(&url.text))
}

fn copied_url_piece(
    url: CFTypeRef,
    piece: impl FnOnce(&crate::url::Contents) -> String,
) -> CFTypeRef {
    // SAFETY: callers pass through the guest's live URL argument.
    unsafe { crate::url::as_url(url) }.map_or(std::ptr::null(), |url| {
        let piece = piece(url);
        string::from_bytes(piece.as_bytes(), false)
    })
}

pub unsafe extern "C" fn CFURLCopyPath(url: CFTypeRef) -> CFTypeRef {
    copied_url_piece(url, crate::url::Contents::path)
}

pub unsafe extern "C" fn CFURLCopyLastPathComponent(url: CFTypeRef) -> CFTypeRef {
    copied_url_piece(url, |url| {
        url.path()
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or_default()
            .to_string()
    })
}

pub unsafe extern "C" fn CFURLCopyScheme(url: CFTypeRef) -> CFTypeRef {
    copied_url_piece(url, crate::url::Contents::scheme)
}

pub unsafe extern "C" fn CFURLCopyHostName(url: CFTypeRef) -> CFTypeRef {
    copied_url_piece(url, |url| {
        let Some((_, rest)) = url.text.split_once("://") else {
            return String::new();
        };
        let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
        let host_port = authority.rsplit('@').next().unwrap_or_default();
        if host_port.starts_with('[') {
            return host_port
                .find(']')
                .map_or(host_port, |end| &host_port[..=end])
                .to_string();
        }
        host_port.split(':').next().unwrap_or_default().to_string()
    })
}

pub unsafe extern "C" fn CFURLGetPortNumber(url: CFTypeRef) -> i32 {
    // SAFETY: the caller's obligation - a URL or null.
    let Some(url) = (unsafe { crate::url::as_url(url) }) else {
        return -1;
    };
    let Some((_, rest)) = url.text.split_once("://") else {
        return -1;
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host_port = authority.rsplit('@').next().unwrap_or_default();
    if host_port.starts_with('[') {
        return host_port
            .split_once("]:")
            .and_then(|(_, port)| port.parse().ok())
            .unwrap_or(-1);
    }
    host_port
        .rsplit_once(':')
        .and_then(|(_, port)| port.parse().ok())
        .unwrap_or(-1)
}

pub unsafe extern "C" fn CFURLHasDirectoryPath(url: CFTypeRef) -> Boolean {
    // SAFETY: the caller's obligation - a URL or null.
    u8::from(unsafe { crate::url::as_url(url) }.is_some_and(|url| {
        url.text
            .split(['?', '#'])
            .next()
            .is_some_and(|path| path.ends_with('/'))
    }))
}

pub unsafe extern "C" fn CFURLCreateCopyAppendingPathComponent(
    _allocator: CFAllocatorRef,
    url: CFTypeRef,
    component: CFTypeRef,
    directory: Boolean,
) -> CFTypeRef {
    // SAFETY: the caller's obligation - a URL and a string from this runtime.
    let (Some(base), Some(component)) = (unsafe { crate::url::as_url(url) }, unsafe {
        string::as_string(component)
    }) else {
        return std::ptr::null();
    };
    let Some(mut path) = base.file_path() else {
        return std::ptr::null();
    };
    let component = component.utf8();
    let Ok(component) = std::str::from_utf8(&component) else {
        return std::ptr::null();
    };
    if !path.ends_with('/') {
        path.push('/');
    }
    path.push_str(component.trim_matches('/'));
    crate::url::new_file_url(&path, directory != 0)
}

pub unsafe extern "C" fn CFURLCreateCopyDeletingLastPathComponent(
    _allocator: CFAllocatorRef,
    url: CFTypeRef,
) -> CFTypeRef {
    // SAFETY: the caller's obligation - a URL or null.
    let Some(url) = (unsafe { crate::url::as_url(url) }) else {
        return std::ptr::null();
    };
    if let Some(path) = url.file_path() {
        return std::path::Path::new(&path)
            .parent()
            .and_then(std::path::Path::to_str)
            .map_or(std::ptr::null(), |parent| {
                crate::url::new_file_url(parent, true)
            });
    }
    let suffix = url.text.find(['?', '#']).unwrap_or(url.text.len());
    let path = &url.text[..suffix];
    let Some(slash) = path.trim_end_matches('/').rfind('/') else {
        return crate::url::new(&url.text);
    };
    crate::url::new(&format!("{}/", &path[..slash]))
}

pub unsafe extern "C" fn CFURLCreateCopyDeletingPathExtension(
    _allocator: CFAllocatorRef,
    url: CFTypeRef,
) -> CFTypeRef {
    // SAFETY: the caller's obligation - a URL or null.
    let Some(url) = (unsafe { crate::url::as_url(url) }) else {
        return std::ptr::null();
    };
    if let Some(path) = url.file_path() {
        let mut path = std::path::PathBuf::from(path);
        path.set_extension("");
        return path.to_str().map_or(std::ptr::null(), |path| {
            crate::url::new_file_url(path, false)
        });
    }
    let suffix = url.text.find(['?', '#']).unwrap_or(url.text.len());
    let (path, tail) = url.text.split_at(suffix);
    let slash = path.rfind('/').map_or(0, |slash| slash + 1);
    let Some(dot) = path[slash..].rfind('.').map(|dot| slash + dot) else {
        return crate::url::new(&url.text);
    };
    crate::url::new(&format!("{}{tail}", &path[..dot]))
}

/// Add URL percent escapes while preserving the caller-selected exceptions.
pub unsafe extern "C" fn CFURLCreateStringByAddingPercentEscapes(
    _allocator: CFAllocatorRef,
    original: CFTypeRef,
    leave_unescaped: CFTypeRef,
    force_escaped: CFTypeRef,
    encoding: u32,
) -> CFTypeRef {
    if encoding != string::encoding::UTF8 {
        objc::fail!("CFURLCreateStringByAddingPercentEscapes: unsupported encoding {encoding:#x}");
    }
    // SAFETY: the caller's obligation - strings or null for the two sets.
    let Some(original) = (unsafe { string::as_string(original) }) else {
        return std::ptr::null();
    };
    let leave = unsafe { string::as_string(leave_unescaped) }
        .map(|value| String::from_utf8_lossy(&value.utf8()).into_owned())
        .unwrap_or_default();
    let force = unsafe { string::as_string(force_escaped) }
        .map(|value| String::from_utf8_lossy(&value.utf8()).into_owned())
        .unwrap_or_default();
    let original_utf8 = original.utf8();
    let source = String::from_utf8_lossy(&original_utf8);
    let mut escaped = String::with_capacity(source.len());
    for character in source.chars() {
        let ordinarily_legal =
            character.is_ascii_alphanumeric() || "-._~:/?#[]@!$&'()*+,;=%".contains(character);
        if (ordinarily_legal || leave.contains(character)) && !force.contains(character) {
            escaped.push(character);
        } else {
            let mut utf8 = [0_u8; 4];
            for byte in character.encode_utf8(&mut utf8).bytes() {
                escaped.push_str(&format!("%{byte:02X}"));
            }
        }
    }
    string::from_bytes(escaped.as_bytes(), false)
}

unsafe fn replacing_percent_escapes(
    original: CFTypeRef,
    leave_escaped: CFTypeRef,
    encoding: u32,
) -> CFTypeRef {
    if encoding != string::encoding::UTF8 {
        return std::ptr::null();
    }
    // SAFETY: the caller's obligation - strings or null.
    let Some(original) = (unsafe { string::as_string(original) }) else {
        return std::ptr::null();
    };
    let leave = unsafe { string::as_string(leave_escaped) }
        .map(|value| String::from_utf8_lossy(&value.utf8()).into_owned())
        .unwrap_or_default();
    let bytes = original.utf8();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let value = if bytes[index] == b'%' && index + 2 < bytes.len() {
            std::str::from_utf8(&bytes[index + 1..index + 3])
                .ok()
                .and_then(|hex| u8::from_str_radix(hex, 16).ok())
        } else {
            None
        };
        match value {
            Some(value) if value.is_ascii() && leave.contains(value as char) => {
                decoded.extend_from_slice(&bytes[index..index + 3]);
                index += 3;
            }
            Some(value) => {
                decoded.push(value);
                index += 3;
            }
            None => {
                decoded.push(bytes[index]);
                index += 1;
            }
        }
    }
    string::from_bytes(&decoded, false)
}

pub unsafe extern "C" fn CFURLCreateStringByReplacingPercentEscapes(
    _allocator: CFAllocatorRef,
    original: CFTypeRef,
    leave_escaped: CFTypeRef,
) -> CFTypeRef {
    // SAFETY: the arguments retain their documented meaning.
    unsafe { replacing_percent_escapes(original, leave_escaped, string::encoding::UTF8) }
}

pub unsafe extern "C" fn CFURLCreateStringByReplacingPercentEscapesUsingEncoding(
    _allocator: CFAllocatorRef,
    original: CFTypeRef,
    leave_escaped: CFTypeRef,
    encoding: u32,
) -> CFTypeRef {
    // SAFETY: the arguments retain their documented meaning.
    unsafe { replacing_percent_escapes(original, leave_escaped, encoding) }
}

pub unsafe extern "C" fn CFURLCopyFileSystemPath(url: CFTypeRef, style: CFIndex) -> CFTypeRef {
    if style != POSIX_PATH_STYLE {
        objc::fail!("CFURLCopyFileSystemPath: path style {style} is not POSIX");
    }
    // SAFETY: the caller's obligation - a URL or null.
    let Some(path) = (unsafe { crate::url::as_url(url) }).and_then(|url| url.file_path()) else {
        return std::ptr::null();
    };
    string::from_bytes(path.as_bytes(), false)
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

/// `CFRunLoopObserverRef CFRunLoopObserverCreate(CFAllocatorRef,
/// CFOptionFlags activities, Boolean repeats, CFIndex order,
/// CFRunLoopObserverCallBack, CFRunLoopObserverContext *)`
///
/// The context's `info` is what the callback is handed back; its retain and release
/// callbacks are not called, as they are not for a source here either - the
/// observer lives until the caller invalidates it.
///
/// # Safety
///
/// `context` is null or a live `CFRunLoopObserverContext`.
pub unsafe extern "C" fn CFRunLoopObserverCreate(
    _allocator: CFAllocatorRef,
    activities: u32,
    repeats: Boolean,
    order: CFIndex,
    callback: Option<runloop::ObserverCallback>,
    context: *const CFRunLoopObserverContext,
) -> CFTypeRef {
    let info = match context.is_null() {
        true => 0,
        // SAFETY: the caller states it is a live context.
        false => (unsafe { (*context).info }) as usize,
    };
    leak(runloop::new_observer(
        activities,
        repeats != 0,
        order as i32,
        callback,
        info,
    ))
}

/// `CFRunLoopObserverContext`, of which only `info` is read.
#[repr(C)]
pub struct CFRunLoopObserverContext {
    pub version: CFIndex,
    pub info: *mut std::ffi::c_void,
    pub retain: Option<unsafe extern "C" fn(*const std::ffi::c_void) -> *const std::ffi::c_void>,
    pub release: Option<unsafe extern "C" fn(*const std::ffi::c_void)>,
    pub copy_description: Option<unsafe extern "C" fn(*const std::ffi::c_void) -> CFTypeRef>,
}

pub unsafe extern "C" fn CFRunLoopObserverInvalidate(observer: CFTypeRef) {
    // SAFETY: the caller's obligation.
    if let Some(o) = unsafe { borrow::<runloop::Observer>(observer) } {
        o.invalidate();
    }
}

pub unsafe extern "C" fn CFRunLoopObserverIsValid(observer: CFTypeRef) -> Boolean {
    // SAFETY: the caller's obligation.
    unsafe { borrow::<runloop::Observer>(observer) }.is_some_and(|o| o.is_valid()) as Boolean
}

pub unsafe extern "C" fn CFRunLoopObserverGetOrder(observer: CFTypeRef) -> CFIndex {
    // SAFETY: the caller's obligation.
    unsafe { borrow::<runloop::Observer>(observer) }.map_or(0, |o| o.order() as CFIndex)
}

pub unsafe extern "C" fn CFRunLoopAddObserver(
    loop_: CFTypeRef,
    observer: CFTypeRef,
    mode: CFTypeRef,
) {
    // SAFETY: the caller's obligation.
    let (Some(l), Some(o)) = (unsafe { borrow::<RunLoop>(loop_) }, unsafe {
        borrow::<runloop::Observer>(observer)
    }) else {
        return;
    };
    // SAFETY: as above.
    l.add_observer(&o, &unsafe { mode_name(mode) });
}

pub unsafe extern "C" fn CFRunLoopRemoveObserver(
    loop_: CFTypeRef,
    observer: CFTypeRef,
    mode: CFTypeRef,
) {
    // SAFETY: the caller's obligation.
    let (Some(l), Some(o)) = (unsafe { borrow::<RunLoop>(loop_) }, unsafe {
        borrow::<runloop::Observer>(observer)
    }) else {
        return;
    };
    // SAFETY: as above.
    l.remove_observer(&o, &unsafe { mode_name(mode) });
}

pub unsafe extern "C" fn CFRunLoopContainsObserver(
    loop_: CFTypeRef,
    observer: CFTypeRef,
    mode: CFTypeRef,
) -> Boolean {
    // SAFETY: the caller's obligation.
    let (Some(l), Some(o)) = (unsafe { borrow::<RunLoop>(loop_) }, unsafe {
        borrow::<runloop::Observer>(observer)
    }) else {
        return 0;
    };
    // SAFETY: as above.
    l.contains_observer(&o, &unsafe { mode_name(mode) }) as Boolean
}

/// `void CFRunLoopAddCommonMode(CFRunLoopRef, CFStringRef)`
pub unsafe extern "C" fn CFRunLoopAddCommonMode(loop_: CFTypeRef, mode: CFTypeRef) {
    // SAFETY: the caller's obligation.
    let Some(l) = (unsafe { borrow::<RunLoop>(loop_) }) else {
        return;
    };
    // SAFETY: as above.
    l.add_common_mode(&unsafe { mode_name(mode) });
}

/// `Boolean CFRunLoopIsWaiting(CFRunLoopRef)`
pub unsafe extern "C" fn CFRunLoopIsWaiting(loop_: CFTypeRef) -> Boolean {
    // SAFETY: the caller's obligation.
    unsafe { borrow::<RunLoop>(loop_) }.is_some_and(|l| l.is_waiting()) as Boolean
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

/// `CFTimeInterval CFRunLoopTimerGetInterval(CFRunLoopTimerRef)`
pub unsafe extern "C" fn CFRunLoopTimerGetInterval(timer: CFTypeRef) -> f64 {
    // SAFETY: the caller's obligation.
    unsafe { borrow::<Timer>(timer) }.map_or(0.0, |t| t.interval_seconds())
}

/// `void CFRunLoopTimerSetNextFireDate(CFRunLoopTimerRef, CFAbsoluteTime)`
///
/// How a caller reschedules a timer it already owns, which is what an event
/// dispatcher does every time its next deadline moves - cheaper than invalidating
/// one and creating another, and the reason it is not a courtesy to implement it.
pub unsafe extern "C" fn CFRunLoopTimerSetNextFireDate(timer: CFTypeRef, fire_date: f64) {
    // SAFETY: the caller's obligation.
    if let Some(t) = unsafe { borrow::<Timer>(timer) } {
        // `delay_until` is a delay from now, which is the form the creator takes;
        // the timer wants the instant it lands on.
        t.set_next(std::time::Instant::now() + delay_until(fire_date));
    }
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

    #[test]
    fn an_opaque_cf_handle_uses_its_registered_type_without_an_isa() {
        static HANDLE: u8 = 0;
        let handle = std::ptr::addr_of!(HANDLE).cast::<c_void>();
        crate::base::note_typed_opaque(handle, 0x1234);
        // SAFETY: a registered opaque CF handle.
        unsafe {
            assert_eq!(CFGetTypeID(handle), 0x1234);
            assert_eq!(CFGetRetainCount(handle), 1);
            assert_eq!(CFHash(handle), handle as usize);
            assert_eq!(CFEqual(handle, handle), 1);
            assert_eq!(CFRetain(handle), handle);
            CFRelease(handle);
        }
    }

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

    #[test]
    fn no_copy_data_keeps_an_owned_snapshot() {
        let mut source = vec![1u8, 2, 3];
        // SAFETY: `source` covers the stated length.
        let data = unsafe {
            CFDataCreateWithBytesNoCopy(
                std::ptr::null(),
                source.as_ptr(),
                source.len() as CFIndex,
                std::ptr::null(),
            )
        };
        source[0] = 9;

        // SAFETY: `data` was created above and remains retained.
        let bytes = unsafe {
            std::slice::from_raw_parts(CFDataGetBytePtr(data), CFDataGetLength(data) as usize)
        };
        assert_eq!(bytes, [1, 2, 3]);
        // SAFETY: balances the create rule.
        unsafe { CFRelease(data) };
    }

    /// Null is still null: an object that is not a `CFData` has no bytes, and a
    /// readable pointer there would be a wrong answer rather than a safe one.
    #[test]
    fn a_non_data_has_no_bytes() {
        // SAFETY: null is the documented "not an object" case.
        assert!(unsafe { CFDataGetBytePtr(std::ptr::null()) }.is_null());
    }
}
