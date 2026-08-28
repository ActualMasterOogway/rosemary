//! `CFArray`, `CFDictionary`, `CFNumber` and `CFData`.
//!
//! Each is an object of this tree's Objective-C runtime whose body is a
//! `#[repr(C)]` struct beginning with the `isa`, and whose contents live in a
//! `Box`ed Rust value hanging off it. Unlike [`crate::string`], no guest lays
//! one of these out by hand, so the interior is free.
//!
//! The dictionary is a vector of pairs searched linearly. That is not an
//! oversight: the keys are compared with a *callback the caller supplied*, so a
//! hash table would have to hash through another one and would still fall back
//! to the callback on collision, and the dictionaries a guest builds here are
//! IOKit matching criteria and option sets - a handful of entries each. When one
//! shows up in a profile, the fix is a hash index beside the vector and not a
//! change of interface.

use std::ffi::c_void;
use std::sync::OnceLock;

use objc::class::Class;
use objc::object::Id;

use crate::base::{CFIndex, CFTypeRef};
use crate::callbacks::Policy;

/// The shape every collection here shares: the object, and a pointer to the
/// Rust value holding its contents.
#[repr(C)]
struct Body<T> {
    isa: *const Class,
    contents: *mut T,
}

/// Register a class whose instances are a [`Body<T>`], with a `-dealloc` that
/// gives the contents back.
fn register<T: 'static>(name: &'static std::ffi::CStr) -> &'static Class {
    let class = crate::base::register(name, None, size_of::<Body<T>>(), true);
    unsafe extern "C" fn dealloc<T>(receiver: Id, _sel: objc::sel::Sel) {
        // SAFETY: `-dealloc` is reached with the last reference gone, and the
        // receiver is a `Body<T>` whose `contents` came from `Box::into_raw`.
        unsafe {
            let body = receiver as *mut Body<T>;
            if !(*body).contents.is_null() {
                drop(Box::from_raw((*body).contents));
                (*body).contents = std::ptr::null_mut();
            }
            objc::arc::finalize(receiver);
        }
    }
    class.replace_method(
        objc::sel::get("dealloc"),
        dealloc::<T> as *const c_void,
        c"v16@0:8",
    );
    class
}

/// Allocate an instance of `class` holding `contents`.
fn make<T>(class: &'static Class, contents: T) -> CFTypeRef {
    // SAFETY: `Body<T>` begins with `isa` and is the layout `class` was
    // registered with.
    let body: *mut Body<T> = unsafe { crate::base::allocate(class) };
    if body.is_null() {
        return std::ptr::null();
    }
    // SAFETY: a live instance whose second word is ours to fill in.
    unsafe { (*body).contents = Box::into_raw(Box::new(contents)) };
    body as CFTypeRef
}

/// The contents of `object`, if it is an instance of `class`.
///
/// # Safety
///
/// `object` must be null or an object this runtime allocated.
unsafe fn contents<'a, T>(object: CFTypeRef, class: &'static Class) -> Option<&'a mut T> {
    // SAFETY: the caller's obligation.
    if !unsafe { crate::base::is_kind_of(object, class) } {
        return None;
    }
    // SAFETY: an instance of `class` is a `Body<T>` with a live `contents`.
    unsafe {
        let body = object as *mut Body<T>;
        (!(*body).contents.is_null()).then(|| &mut *(*body).contents)
    }
}

// 

pub struct ArrayContents {
    pub values: Vec<CFTypeRef>,
    pub policy: Policy,
}

impl Drop for ArrayContents {
    fn drop(&mut self) {
        for value in self.values.drain(..) {
            self.policy.release(value);
        }
    }
}

pub fn array_class() -> &'static Class {
    static CLASS: OnceLock<usize> = OnceLock::new();
    let p = *CLASS.get_or_init(|| {
        objc::class::ptr(register::<ArrayContents>(c"__NSCFArray")) as usize
    });
    // SAFETY: a leaked `Class` this crate registered.
    unsafe { &*(p as *const Class) }
}

/// Build an array from `values`, retaining each as `policy` says.
pub fn new_array(values: &[CFTypeRef], policy: Policy) -> CFTypeRef {
    let values: Vec<CFTypeRef> = values.iter().map(|v| policy.retain(*v)).collect();
    make(array_class(), ArrayContents { values, policy })
}

/// # Safety
///
/// As [`contents`].
pub unsafe fn array<'a>(object: CFTypeRef) -> Option<&'a mut ArrayContents> {
    // SAFETY: the caller's obligation.
    unsafe { contents(object, array_class()) }
}

// 

pub struct DictionaryContents {
    pub entries: Vec<(CFTypeRef, CFTypeRef)>,
    pub keys: Policy,
    pub values: Policy,
}

impl Drop for DictionaryContents {
    fn drop(&mut self) {
        for (key, value) in self.entries.drain(..) {
            self.keys.release(key);
            self.values.release(value);
        }
    }
}

impl DictionaryContents {
    pub fn position(&self, key: CFTypeRef) -> Option<usize> {
        self.entries.iter().position(|(k, _)| self.keys.equal(*k, key))
    }

    /// Insert or replace, retaining both as the policies say.
    pub fn set(&mut self, key: CFTypeRef, value: CFTypeRef) {
        let value = self.values.retain(value);
        match self.position(key) {
            Some(at) => {
                self.values.release(self.entries[at].1);
                self.entries[at].1 = value;
            }
            None => self.entries.push((self.keys.retain(key), value)),
        }
    }
}

pub fn dictionary_class() -> &'static Class {
    static CLASS: OnceLock<usize> = OnceLock::new();
    let p = *CLASS.get_or_init(|| {
        objc::class::ptr(register::<DictionaryContents>(c"__NSCFDictionary")) as usize
    });
    // SAFETY: a leaked `Class` this crate registered.
    unsafe { &*(p as *const Class) }
}

pub fn new_dictionary(
    pairs: &[(CFTypeRef, CFTypeRef)],
    keys: Policy,
    values: Policy,
) -> CFTypeRef {
    let mut contents = DictionaryContents {
        entries: Vec::with_capacity(pairs.len()),
        keys,
        values,
    };
    for (key, value) in pairs {
        contents.set(*key, *value);
    }
    make(dictionary_class(), contents)
}

/// # Safety
///
/// As [`contents`].
pub unsafe fn dictionary<'a>(object: CFTypeRef) -> Option<&'a mut DictionaryContents> {
    // SAFETY: the caller's obligation.
    unsafe { contents(object, dictionary_class()) }
}

// 

/// `CFNumberType`, which says how to read the sixteen bytes a number carries.
///
/// The values are Apple's and are part of the interface: a guest passes one in
/// and reads one back out.
pub mod number_type {
    pub const SINT8: i32 = 1;
    pub const SINT16: i32 = 2;
    pub const SINT32: i32 = 3;
    pub const SINT64: i32 = 4;
    pub const FLOAT32: i32 = 5;
    pub const FLOAT64: i32 = 6;
    pub const CHAR: i32 = 7;
    pub const SHORT: i32 = 8;
    pub const INT: i32 = 9;
    pub const LONG: i32 = 10;
    pub const LONG_LONG: i32 = 11;
    pub const FLOAT: i32 = 12;
    pub const DOUBLE: i32 = 13;
    pub const CFINDEX: i32 = 14;
    pub const NSINTEGER: i32 = 15;
    pub const CGFLOAT: i32 = 16;
}

/// A number, held as whichever of the two things it is.
///
/// Not one `f64`: a `SInt64` past 2^53 does not survive the trip, and a guest
/// that stores a HID element cookie or a vendor ID would get a neighbouring
/// value back with nothing to say so.
#[derive(Clone, Copy)]
pub enum Number {
    Integer(i64),
    Float(f64),
}

impl Number {
    pub fn is_float(&self) -> bool {
        matches!(self, Number::Float(_))
    }

    pub fn as_i64(&self) -> i64 {
        match self {
            Number::Integer(v) => *v,
            Number::Float(v) => *v as i64,
        }
    }

    pub fn as_f64(&self) -> f64 {
        match self {
            Number::Integer(v) => *v as f64,
            Number::Float(v) => *v,
        }
    }
}

/// How wide `type` is, and whether it is floating point.
pub fn number_shape(kind: i32) -> Option<(usize, bool)> {
    use number_type as t;
    Some(match kind {
        t::SINT8 | t::CHAR => (1, false),
        t::SINT16 | t::SHORT => (2, false),
        t::SINT32 | t::INT => (4, false),
        t::SINT64 | t::LONG | t::LONG_LONG | t::CFINDEX | t::NSINTEGER => (8, false),
        t::FLOAT32 | t::FLOAT => (4, true),
        t::FLOAT64 | t::DOUBLE | t::CGFLOAT => (8, true),
        _ => return None,
    })
}

pub fn number_class() -> &'static Class {
    static CLASS: OnceLock<usize> = OnceLock::new();
    let p = *CLASS.get_or_init(|| objc::class::ptr(register::<Number>(c"__NSCFNumber")) as usize);
    // SAFETY: a leaked `Class` this crate registered.
    unsafe { &*(p as *const Class) }
}

pub fn new_number(value: Number) -> CFTypeRef {
    make(number_class(), value)
}

/// # Safety
///
/// As [`contents`].
pub unsafe fn number<'a>(object: CFTypeRef) -> Option<&'a mut Number> {
    // SAFETY: the caller's obligation.
    unsafe { contents(object, number_class()) }
}

pub fn data_class() -> &'static Class {
    static CLASS: OnceLock<usize> = OnceLock::new();
    let p = *CLASS.get_or_init(|| objc::class::ptr(register::<Vec<u8>>(c"__NSCFData")) as usize);
    // SAFETY: a leaked `Class` this crate registered.
    unsafe { &*(p as *const Class) }
}

pub fn new_data(bytes: Vec<u8>) -> CFTypeRef {
    make(data_class(), bytes)
}

/// # Safety
///
/// As [`contents`].
pub unsafe fn data<'a>(object: CFTypeRef) -> Option<&'a mut Vec<u8>> {
    // SAFETY: the caller's obligation.
    unsafe { contents(object, data_class()) }
}

/// A `CFIndex` clamped into a slice index, or `None` for out of range.
pub fn at(index: CFIndex, len: usize) -> Option<usize> {
    (index >= 0 && (index as usize) < len).then_some(index as usize)
}
