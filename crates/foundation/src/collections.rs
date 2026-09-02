//! `NSArray`, `NSDictionary`, `NSData` and `NSNumber`, on the storage
//! [`corefoundation::collections`] already holds.
//!
//! The mutable spellings are here too. A `__NSCFArray` descends from
//! `NSMutableArray` whether or not it was created mutable - it has to, or a
//! guest's `isKindOfClass:` disagrees with the object it is holding - so
//! mutability is a field, and mutating an immutable collection is refused *by
//! name* rather than quietly allowed.

use corefoundation::api;
use corefoundation::base::CFTypeRef;
use corefoundation::callbacks::Policy;
use corefoundation::collections;
use objc::object::Id;
use objc::sel::Sel;

use crate::define::{class_methods, methods};

/// Report a mutation of a collection that was not created mutable.
///
/// A named abort rather than a wrong answer: an `NSArray` that silently accepted
/// `-addObject:` would hand a different object back to the next reader with
/// nothing anywhere to say why. Apple raises here.
#[cold]
fn immutable(what: &str, sel: &str) -> ! {
    objc::fail!("-[{what} {sel}]: the collection was not created mutable");
}

unsafe extern "C" fn count(receiver: Id, _sel: Sel) -> usize {
    // SAFETY: an array.
    unsafe { api::CFArrayGetCount(receiver as CFTypeRef) as usize }
}

unsafe extern "C" fn object_at_index(receiver: Id, _sel: Sel, index: usize) -> Id {
    // SAFETY: an array.
    unsafe { api::CFArrayGetValueAtIndex(receiver as CFTypeRef, index as isize) as Id }
}

unsafe extern "C" fn add_object(receiver: Id, _sel: Sel, object: Id) {
    // SAFETY: an array.
    let Some(a) = (unsafe { collections::array(receiver as CFTypeRef) }) else {
        return;
    };
    if !a.mutable {
        immutable("NSArray", "addObject:");
    }
    a.values.push(a.policy.retain(object as CFTypeRef));
}

unsafe extern "C" fn remove_object_at_index(receiver: Id, _sel: Sel, index: usize) {
    // SAFETY: an array.
    let Some(a) = (unsafe { collections::array(receiver as CFTypeRef) }) else {
        return;
    };
    if !a.mutable {
        immutable("NSArray", "removeObjectAtIndex:");
    }
    if index < a.values.len() {
        let gone = a.values.remove(index);
        a.policy.release(gone);
    }
}

/// `-componentsJoinedByString:`, the other half of
/// `-[NSString componentsSeparatedByString:]`.
///
/// Every element is *sent* `-UTF8String` rather than read as a string, because
/// an array is entitled to hold anything and a guest joining an array of its own
/// objects expects each one's own answer. An element that answers nothing joins
/// as nothing, which is what a nil description would give.
unsafe extern "C" fn components_joined_by_string(receiver: Id, _sel: Sel, separator: Id) -> Id {
    // SAFETY: an array.
    let Some(array) = (unsafe { collections::array(receiver as CFTypeRef) }) else {
        return std::ptr::null_mut();
    };
    // SAFETY: null or a string.
    let separator = unsafe { crate::string::bytes(separator) };
    let mut out: Vec<u8> = Vec::new();
    for (i, value) in array.values.iter().enumerate() {
        if i > 0 {
            out.extend_from_slice(&separator);
        }
        // SAFETY: an element of a live array.
        out.extend_from_slice(&unsafe { crate::string::bytes(*value as Id) });
    }
    let joined = corefoundation::string::from_bytes(&out, false);
    // SAFETY: a fresh object.
    unsafe { api::CFAutorelease(joined) as Id }
}

/// `+[NSData dataWithContentsOfFile:]`.
///
/// Nil for a file that cannot be read, which is Apple's answer and the one a
/// caller checks for - a program asking for a file it may not have is asking a
/// question, not making a mistake.
unsafe extern "C" fn data_with_contents_of_file(_class: Id, _sel: Sel, path: Id) -> Id {
    // SAFETY: an `NSString`, or nil.
    let path = unsafe { crate::string::bytes(path) };
    let Ok(path) = String::from_utf8(path) else {
        return std::ptr::null_mut();
    };
    let Ok(bytes) = std::fs::read(&path) else {
        return std::ptr::null_mut();
    };
    let data = collections::new_data(bytes);
    // SAFETY: a fresh object.
    unsafe { api::CFAutorelease(data) as Id }
}

/// `-writeToFile:atomically:`.
///
/// The atomic flag is honoured by writing beside the target and renaming, which
/// is what makes it atomic - a program that asked for it and got a plain write
/// would have a half-written save file after a crash and nothing to say so.
unsafe extern "C" fn write_to_file(receiver: Id, _sel: Sel, path: Id, atomically: u8) -> u8 {
    // SAFETY: an `NSString`, or nil.
    let path = unsafe { crate::string::bytes(path) };
    let Ok(path) = String::from_utf8(path) else {
        return 0;
    };
    // SAFETY: a data object.
    let Some(bytes) = (unsafe { collections::data(receiver as CFTypeRef) }) else {
        return 0;
    };
    if atomically == 0 {
        return std::fs::write(&path, &**bytes).is_ok() as u8;
    }
    let temporary = format!("{path}.rosetta3-{}", std::process::id());
    if std::fs::write(&temporary, &**bytes).is_err() {
        return 0;
    }
    match std::fs::rename(&temporary, &path) {
        Ok(()) => 1,
        Err(_) => {
            let _ = std::fs::remove_file(&temporary);
            0
        }
    }
}

/// `-[NSDictionary valueForKey:]`, which is *not* the `NSObject` one.
///
/// A dictionary overrides key-value coding to mean its own contents: Apple's
/// answers `-objectForKey:` for any key not beginning with `@`. Without the
/// override the inherited implementation looks for a getter or an ivar of that
/// name, finds neither, and reports a class that in fact has the key.
unsafe extern "C" fn dictionary_value_for_key(receiver: Id, sel: Sel, key: Id) -> Id {
    // SAFETY: an `NSString`, or nil.
    let name = unsafe { crate::string::bytes(key) };
    if name.first() == Some(&b'@') {
        // `@count` and its relatives are the operators, which address the
        // dictionary itself rather than its contents. Refused by name: an
        // operator answered as a missing key would be nil where a number was
        // expected.
        objc::fail!(
            "-[NSDictionary valueForKey:]: {} is a key-value coding operator, which this runtime \
             does not implement",
            String::from_utf8_lossy(&name)
        );
    }
    // SAFETY: `-objectForKey:` is `id (id, SEL, id)`.
    unsafe { object_for_key(receiver, sel, key) }
}

/// `-[NSMutableDictionary setValue:forKey:]`, whose nil means *remove*.
unsafe extern "C" fn dictionary_set_value_for_key(receiver: Id, sel: Sel, value: Id, key: Id) {
    match value.is_null() {
        // SAFETY: `-removeObjectForKey:` is `void (id, SEL, id)`.
        true => unsafe { remove_object_for_key(receiver, sel, key) },
        // SAFETY: `-setObject:forKey:` is `void (id, SEL, id, id)`.
        false => unsafe { set_object_for_key(receiver, sel, value, key) },
    }
}

/// `NSFastEnumerationState`, which the guest allocates on its own stack.
///
/// The layout is ABI: `for (id x in collection)` is compiled by clang into a
/// call to `-countByEnumeratingWithState:objects:count:` and then a walk of
/// `items_ptr`, so a field at the wrong offset is a loop over the wrong memory.
#[repr(C)]
struct FastEnumerationState {
    state: usize,
    items: *mut Id,
    mutations: *mut usize,
    extra: [usize; 5],
}

/// A word every collection points `mutations` at.
///
/// The compiler reads it before and after the loop body and raises if it
/// changed, which is how a program is told it mutated a collection while
/// enumerating it. One shared word means this runtime never reports that - it
/// has no per-collection version counter - so the check always passes. The
/// alternative is pointing at the collection's own storage, whose value changes
/// for reasons that are not mutations and would raise on a correct program.
static NEVER_MUTATED: usize = 0;

/// `-countByEnumeratingWithState:objects:count:`, for anything whose elements
/// are a contiguous vector.
///
/// One pass: everything is already in one buffer, so the first call hands the
/// guest a pointer straight into it and the second says there is no more. That
/// is what Apple's array does too, and it is why `buffer` is untouched - it is
/// there for a collection that has to *materialise* its elements, which none of
/// these do.
///
/// # Safety
///
/// `state` must be a writable `NSFastEnumerationState`, and `values` must
/// outlive the loop - which it does, because a mutation during enumeration is
/// the program's error and the one this cannot detect.
unsafe fn enumerate(state: *mut FastEnumerationState, values: &mut [CFTypeRef]) -> usize {
    if state.is_null() {
        return 0;
    }
    // SAFETY: the caller's obligation.
    let state = unsafe { &mut *state };
    if state.state != 0 {
        return 0;
    }
    state.state = 1;
    state.items = values.as_mut_ptr() as *mut Id;
    state.mutations = &NEVER_MUTATED as *const usize as *mut usize;
    values.len()
}

unsafe extern "C" fn array_enumerate(
    receiver: Id,
    _sel: Sel,
    state: *mut FastEnumerationState,
    _buffer: *mut Id,
    _len: usize,
) -> usize {
    // SAFETY: an array.
    let Some(array) = (unsafe { collections::array(receiver as CFTypeRef) }) else {
        return 0;
    };
    // SAFETY: the guest's own state structure, and storage that outlives the
    // loop.
    unsafe { enumerate(state, &mut array.values) }
}

/// The dictionary's, which enumerates **keys** - as `for (id key in dict)` does.
///
/// A dictionary's keys are not contiguous here: it is a vector of *pairs*. So
/// this fills the caller's buffer a batch at a time, which is what that argument
/// is for and the reason the protocol has one. Copying them into a buffer of
/// this object's own would either leak it - a `break` means the second call
/// never comes - or need a lifetime nothing here can give it.
///
/// `state` is the cursor: zero on entry, and the index of the next key after
/// that.
unsafe extern "C" fn dictionary_enumerate(
    receiver: Id,
    _sel: Sel,
    state: *mut FastEnumerationState,
    buffer: *mut Id,
    len: usize,
) -> usize {
    // SAFETY: a dictionary.
    let Some(dictionary) = (unsafe { collections::dictionary(receiver as CFTypeRef) }) else {
        return 0;
    };
    if state.is_null() || buffer.is_null() || len == 0 {
        return 0;
    }
    // SAFETY: the guest's own state structure.
    let state = unsafe { &mut *state };
    let from = state.state;
    let batch = dictionary.entries.len().saturating_sub(from).min(len);
    if batch == 0 {
        return 0;
    }
    for i in 0..batch {
        // SAFETY: the caller said `buffer` holds `len` items, and `batch` is at
        // most that.
        unsafe { *buffer.add(i) = dictionary.entries[from + i].0 as Id };
    }
    state.state = from + batch;
    state.items = buffer;
    state.mutations = &NEVER_MUTATED as *const usize as *mut usize;
    batch
}

unsafe extern "C" fn remove_all_objects(receiver: Id, _sel: Sel) {
    // SAFETY: an array.
    let Some(a) = (unsafe { collections::array(receiver as CFTypeRef) }) else {
        return;
    };
    if !a.mutable {
        immutable("NSArray", "removeAllObjects");
    }
    for value in a.values.drain(..) {
        a.policy.release(value);
    }
}

unsafe extern "C" fn contains_object(receiver: Id, _sel: Sel, object: Id) -> bool {
    // SAFETY: an array.
    let Some(a) = (unsafe { collections::array(receiver as CFTypeRef) }) else {
        return false;
    };
    a.values
        .iter()
        .any(|v| a.policy.equal(*v, object as CFTypeRef))
}

unsafe extern "C" fn new_array(_class: Id, _sel: Sel) -> Id {
    collections::new_array(&[], Policy::from_type_callbacks(), true) as Id
}

unsafe extern "C" fn array_fn(_class: Id, _sel: Sel) -> Id {
    // SAFETY: a fresh object.
    unsafe {
        api::CFAutorelease(collections::new_array(
            &[],
            Policy::from_type_callbacks(),
            true,
        )) as Id
    }
}

unsafe extern "C" fn array_with_capacity(_class: Id, _sel: Sel, _capacity: usize) -> Id {
    // SAFETY: a fresh object.
    unsafe {
        api::CFAutorelease(collections::new_array(
            &[],
            Policy::from_type_callbacks(),
            true,
        )) as Id
    }
}

unsafe extern "C" fn array_with_object(_class: Id, _sel: Sel, object: Id) -> Id {
    // SAFETY: a fresh object holding a live one.
    unsafe {
        api::CFAutorelease(collections::new_array(
            &[object as CFTypeRef],
            Policy::from_type_callbacks(),
            false,
        )) as Id
    }
}

/// `+arrayWithObjects:count:`, which is the counted spelling
/// `objc::dispatch` rewrites the nil-terminated variadic into.
unsafe extern "C" fn array_with_objects_count(
    _class: Id,
    _sel: Sel,
    objects: *const Id,
    count: usize,
) -> Id {
    if objects.is_null() && count > 0 {
        return std::ptr::null_mut();
    }
    // SAFETY: the caller says `objects` covers `count` entries.
    let slice = match count {
        0 => &[][..],
        _ => unsafe { std::slice::from_raw_parts(objects as *const CFTypeRef, count) },
    };
    // SAFETY: a fresh object.
    unsafe {
        api::CFAutorelease(collections::new_array(
            slice,
            Policy::from_type_callbacks(),
            false,
        )) as Id
    }
}

unsafe extern "C" fn dictionary_count(receiver: Id, _sel: Sel) -> usize {
    // SAFETY: a dictionary.
    unsafe { api::CFDictionaryGetCount(receiver as CFTypeRef) as usize }
}

unsafe extern "C" fn object_for_key(receiver: Id, _sel: Sel, key: Id) -> Id {
    // SAFETY: a dictionary and a key.
    unsafe { api::CFDictionaryGetValue(receiver as CFTypeRef, key as CFTypeRef) as Id }
}

unsafe extern "C" fn set_object_for_key(receiver: Id, _sel: Sel, object: Id, key: Id) {
    // SAFETY: a dictionary.
    let Some(d) = (unsafe { collections::dictionary(receiver as CFTypeRef) }) else {
        return;
    };
    if !d.mutable {
        immutable("NSDictionary", "setObject:forKey:");
    }
    d.set(key as CFTypeRef, object as CFTypeRef);
}

unsafe extern "C" fn remove_object_for_key(receiver: Id, _sel: Sel, key: Id) {
    // SAFETY: a dictionary.
    let Some(d) = (unsafe { collections::dictionary(receiver as CFTypeRef) }) else {
        return;
    };
    if !d.mutable {
        immutable("NSDictionary", "removeObjectForKey:");
    }
    if let Some(at) = d.position(key as CFTypeRef) {
        let (key, value) = d.entries.remove(at);
        d.keys.release(key);
        d.values.release(value);
    }
}

/// `-allKeys`, which is an `NSArray` of the keys in insertion order.
unsafe extern "C" fn all_keys(receiver: Id, _sel: Sel) -> Id {
    // SAFETY: a dictionary.
    let Some(d) = (unsafe { collections::dictionary(receiver as CFTypeRef) }) else {
        return std::ptr::null_mut();
    };
    let keys: Vec<CFTypeRef> = d.entries.iter().map(|(k, _)| *k).collect();
    // SAFETY: a fresh object holding live ones.
    unsafe {
        api::CFAutorelease(collections::new_array(
            &keys,
            Policy::from_type_callbacks(),
            false,
        )) as Id
    }
}

unsafe extern "C" fn all_values(receiver: Id, _sel: Sel) -> Id {
    // SAFETY: a dictionary.
    let Some(d) = (unsafe { collections::dictionary(receiver as CFTypeRef) }) else {
        return std::ptr::null_mut();
    };
    let values: Vec<CFTypeRef> = d.entries.iter().map(|(_, v)| *v).collect();
    // SAFETY: a fresh object holding live ones.
    unsafe {
        api::CFAutorelease(collections::new_array(
            &values,
            Policy::from_type_callbacks(),
            false,
        )) as Id
    }
}

unsafe extern "C" fn new_dictionary(_class: Id, _sel: Sel) -> Id {
    collections::new_dictionary(
        &[],
        Policy::from_key_callbacks(),
        Policy::from_type_callbacks(),
        true,
    ) as Id
}

unsafe extern "C" fn dictionary(_class: Id, _sel: Sel) -> Id {
    // SAFETY: a fresh object.
    unsafe { api::CFAutorelease(new_dictionary(_class, _sel) as CFTypeRef) as Id }
}

unsafe extern "C" fn dictionary_with_capacity(_class: Id, _sel: Sel, _capacity: usize) -> Id {
    // SAFETY: a fresh object.
    unsafe { api::CFAutorelease(new_dictionary(_class, _sel) as CFTypeRef) as Id }
}

/// `+dictionaryWithObjects:forKeys:count:`, the counted spelling.
unsafe extern "C" fn dictionary_with_objects_for_keys_count(
    _class: Id,
    _sel: Sel,
    objects: *const Id,
    keys: *const Id,
    count: usize,
) -> Id {
    if (objects.is_null() || keys.is_null()) && count > 0 {
        return std::ptr::null_mut();
    }
    // SAFETY: the caller says both arrays cover `count` entries.
    let pairs: Vec<(CFTypeRef, CFTypeRef)> = match count {
        0 => Vec::new(),
        _ => unsafe {
            std::slice::from_raw_parts(keys as *const CFTypeRef, count)
                .iter()
                .copied()
                .zip(
                    std::slice::from_raw_parts(objects as *const CFTypeRef, count)
                        .iter()
                        .copied(),
                )
                .collect()
        },
    };
    // SAFETY: a fresh object.
    unsafe {
        api::CFAutorelease(collections::new_dictionary(
            &pairs,
            Policy::from_key_callbacks(),
            Policy::from_type_callbacks(),
            false,
        )) as Id
    }
}

unsafe extern "C" fn data_length(receiver: Id, _sel: Sel) -> usize {
    // SAFETY: a data object.
    unsafe { api::CFDataGetLength(receiver as CFTypeRef) as usize }
}

unsafe extern "C" fn data_bytes(receiver: Id, _sel: Sel) -> *const u8 {
    // SAFETY: a data object.
    unsafe { api::CFDataGetBytePtr(receiver as CFTypeRef) }
}

unsafe extern "C" fn data_with_bytes(_class: Id, _sel: Sel, bytes: *const u8, length: usize) -> Id {
    // SAFETY: the caller says `bytes` covers `length` bytes.
    unsafe { api::CFAutorelease(api::CFDataCreate(std::ptr::null(), bytes, length as isize)) as Id }
}

unsafe extern "C" fn append_bytes(receiver: Id, _sel: Sel, bytes: *const u8, length: usize) {
    // SAFETY: a data object.
    let Some(d) = (unsafe { collections::data(receiver as CFTypeRef) }) else {
        return;
    };
    if bytes.is_null() {
        return;
    }
    // SAFETY: the caller says `bytes` covers `length` bytes.
    d.extend_from_slice(unsafe { std::slice::from_raw_parts(bytes, length) });
}

unsafe extern "C" fn int_value(receiver: Id, _sel: Sel) -> i32 {
    // SAFETY: a number.
    unsafe { collections::number(receiver as CFTypeRef) }.map_or(0, |n| n.as_i64() as i32)
}

/// The narrower integer accessors.
///
/// Each truncates the stored value the way a C cast does, which is what Apple's
/// do: an `NSNumber` holds one value and every accessor is a *conversion* of
/// it, not a claim about how it was made.
unsafe extern "C" fn unsigned_int_value(receiver: Id, _sel: Sel) -> u32 {
    // SAFETY: a number.
    unsafe { collections::number(receiver as CFTypeRef) }.map_or(0, |n| n.as_i64() as u32)
}

unsafe extern "C" fn short_value(receiver: Id, _sel: Sel) -> i16 {
    // SAFETY: a number.
    unsafe { collections::number(receiver as CFTypeRef) }.map_or(0, |n| n.as_i64() as i16)
}

unsafe extern "C" fn unsigned_short_value(receiver: Id, _sel: Sel) -> u16 {
    // SAFETY: a number.
    unsafe { collections::number(receiver as CFTypeRef) }.map_or(0, |n| n.as_i64() as u16)
}

unsafe extern "C" fn char_value(receiver: Id, _sel: Sel) -> i8 {
    // SAFETY: a number.
    unsafe { collections::number(receiver as CFTypeRef) }.map_or(0, |n| n.as_i64() as i8)
}

unsafe extern "C" fn unsigned_char_value(receiver: Id, _sel: Sel) -> u8 {
    // SAFETY: a number.
    unsafe { collections::number(receiver as CFTypeRef) }.map_or(0, |n| n.as_i64() as u8)
}

unsafe extern "C" fn integer_value(receiver: Id, _sel: Sel) -> i64 {
    // SAFETY: a number.
    unsafe { collections::number(receiver as CFTypeRef) }.map_or(0, |n| n.as_i64())
}

unsafe extern "C" fn double_value(receiver: Id, _sel: Sel) -> f64 {
    // SAFETY: a number.
    unsafe { collections::number(receiver as CFTypeRef) }.map_or(0.0, |n| n.as_f64())
}

unsafe extern "C" fn float_value(receiver: Id, _sel: Sel) -> f32 {
    // SAFETY: a number.
    unsafe { collections::number(receiver as CFTypeRef) }.map_or(0.0, |n| n.as_f64() as f32)
}

unsafe extern "C" fn bool_value(receiver: Id, _sel: Sel) -> bool {
    // SAFETY: a number.
    unsafe { collections::number(receiver as CFTypeRef) }.is_some_and(|n| n.as_i64() != 0)
}

/// `-stringValue`, which is also `-description` for a number.
///
/// An integer prints as itself; a float prints the way Rust's `{}` does, which
/// is the shortest text that reads back as the same double - the same rule
/// Apple's uses, so `@(9.25)` is `9.25` and `@(9.0)` is `9` on both.
unsafe extern "C" fn string_value(receiver: Id, _sel: Sel) -> Id {
    // SAFETY: a number.
    let text = match unsafe { collections::number(receiver as CFTypeRef) } {
        Some(collections::Number::Integer(value)) => value.to_string(),
        Some(collections::Number::Float(value)) => value.to_string(),
        // Not a number this runtime made. Apple would raise; an empty string is
        // the answer that lets the caller print something and carry on.
        None => String::new(),
    };
    let string = crate::string::from_str(&text);
    // SAFETY: a string this call just created.
    unsafe { api::CFAutorelease(string as CFTypeRef) as Id }
}

unsafe extern "C" fn number_with_int(_class: Id, _sel: Sel, value: i32) -> Id {
    // SAFETY: a fresh object.
    unsafe {
        api::CFAutorelease(collections::new_number(collections::Number::Integer(
            value as i64,
        ))) as Id
    }
}

unsafe extern "C" fn number_with_integer(_class: Id, _sel: Sel, value: i64) -> Id {
    // SAFETY: a fresh object.
    unsafe {
        api::CFAutorelease(collections::new_number(collections::Number::Integer(value))) as Id
    }
}

unsafe extern "C" fn number_with_double(_class: Id, _sel: Sel, value: f64) -> Id {
    // SAFETY: a fresh object.
    unsafe { api::CFAutorelease(collections::new_number(collections::Number::Float(value))) as Id }
}

unsafe extern "C" fn number_with_bool(_class: Id, _sel: Sel, value: bool) -> Id {
    // SAFETY: a fresh object.
    unsafe {
        api::CFAutorelease(collections::new_number(collections::Number::Integer(
            value as i64,
        ))) as Id
    }
}

pub(crate) fn install() {
    let array = objc::class::lookup(c"NSArray").expect("CoreFoundation registered NSArray");
    let mutable_array = objc::class::lookup(c"NSMutableArray").expect("NSMutableArray");
    methods!(array, {
        "count" => count, c"Q16@0:8";
        "objectAtIndex:" => object_at_index, c"@24@0:8Q16";
        "objectAtIndexedSubscript:" => object_at_index, c"@24@0:8Q16";
        "containsObject:" => contains_object, c"B24@0:8@16";
        "componentsJoinedByString:" => components_joined_by_string, c"@24@0:8@16";
        "countByEnumeratingWithState:objects:count:" => array_enumerate, c"Q40@0:8^{?=Q^@^QQ5}16^@24Q32";
    });
    class_methods!(array, {
        "array" => array_fn, c"@16@0:8";
        "arrayWithObject:" => array_with_object, c"@24@0:8@16";
        "arrayWithObjects:count:" => array_with_objects_count, c"@32@0:8^@16Q24";
    });
    methods!(mutable_array, {
        "addObject:" => add_object, c"v24@0:8@16";
        "removeObjectAtIndex:" => remove_object_at_index, c"v24@0:8Q16";
        "removeAllObjects" => remove_all_objects, c"v16@0:8";
    });
    class_methods!(mutable_array, {
        "new" => new_array, c"@16@0:8";
        "array" => array_fn, c"@16@0:8";
        "arrayWithCapacity:" => array_with_capacity, c"@24@0:8Q16";
    });

    let dictionary_class =
        objc::class::lookup(c"NSDictionary").expect("CoreFoundation registered NSDictionary");
    let mutable_dictionary =
        objc::class::lookup(c"NSMutableDictionary").expect("NSMutableDictionary");
    methods!(dictionary_class, {
        "count" => dictionary_count, c"Q16@0:8";
        "objectForKey:" => object_for_key, c"@24@0:8@16";
        "objectForKeyedSubscript:" => object_for_key, c"@24@0:8@16";
        "allKeys" => all_keys, c"@16@0:8";
        "allValues" => all_values, c"@16@0:8";
        "valueForKey:" => dictionary_value_for_key, c"@24@0:8@16";
        "countByEnumeratingWithState:objects:count:" => dictionary_enumerate, c"Q40@0:8^{?=Q^@^QQ5}16^@24Q32";
    });
    class_methods!(dictionary_class, {
        "dictionaryWithObjects:forKeys:count:" => dictionary_with_objects_for_keys_count, c"@40@0:8^@16^@24Q32";
    });
    methods!(mutable_dictionary, {
        "setObject:forKey:" => set_object_for_key, c"v32@0:8@16@24";
        "setObject:forKeyedSubscript:" => set_object_for_key, c"v32@0:8@16@24";
        "removeObjectForKey:" => remove_object_for_key, c"v24@0:8@16";
        "setValue:forKey:" => dictionary_set_value_for_key, c"v32@0:8@16@24";
    });
    class_methods!(mutable_dictionary, {
        "new" => new_dictionary, c"@16@0:8";
        "dictionary" => dictionary, c"@16@0:8";
        "dictionaryWithCapacity:" => dictionary_with_capacity, c"@24@0:8Q16";
    });

    let data = objc::class::lookup(c"NSData").expect("CoreFoundation registered NSData");
    let mutable_data = objc::class::lookup(c"NSMutableData").expect("NSMutableData");
    methods!(data, {
        "length" => data_length, c"Q16@0:8";
        "bytes" => data_bytes, c"^v16@0:8";
    });
    class_methods!(data, {
        "dataWithBytes:length:" => data_with_bytes, c"@32@0:8^v16Q24";
        "dataWithContentsOfFile:" => data_with_contents_of_file, c"@24@0:8@16";
    });
    methods!(data, {
        "writeToFile:atomically:" => write_to_file, c"B28@0:8@16c24";
    });
    methods!(mutable_data, {
        "appendBytes:length:" => append_bytes, c"v32@0:8^v16Q24";
        "mutableBytes" => data_bytes, c"^v16@0:8";
    });

    let number = objc::class::lookup(c"NSNumber").expect("CoreFoundation registered NSNumber");
    methods!(number, {
        "intValue" => int_value, c"i16@0:8";
        "integerValue" => integer_value, c"q16@0:8";
        "longValue" => integer_value, c"q16@0:8";
        "longLongValue" => integer_value, c"q16@0:8";
        "unsignedIntegerValue" => integer_value, c"Q16@0:8";
        "unsignedLongValue" => integer_value, c"Q16@0:8";
        "unsignedLongLongValue" => integer_value, c"Q16@0:8";
        "unsignedIntValue" => unsigned_int_value, c"I16@0:8";
        "shortValue" => short_value, c"s16@0:8";
        "unsignedShortValue" => unsigned_short_value, c"S16@0:8";
        "charValue" => char_value, c"c16@0:8";
        "unsignedCharValue" => unsigned_char_value, c"C16@0:8";
        "doubleValue" => double_value, c"d16@0:8";
        "floatValue" => float_value, c"f16@0:8";
        "boolValue" => bool_value, c"B16@0:8";
        "stringValue" => string_value, c"@16@0:8";
        "description" => string_value, c"@16@0:8";
    });
    class_methods!(number, {
        "numberWithInt:" => number_with_int, c"@24@0:8i16";
        "numberWithInteger:" => number_with_integer, c"@24@0:8q16";
        "numberWithLong:" => number_with_integer, c"@24@0:8q16";
        "numberWithLongLong:" => number_with_integer, c"@24@0:8q16";
        "numberWithDouble:" => number_with_double, c"@24@0:8d16";
        "numberWithBool:" => number_with_bool, c"@24@0:8B16";
    });
}
