//! `CFTypeRef`: what every CoreFoundation object has in common.
//!
//! A `CFTypeRef` here is an Objective-C object, and `CFRetain` is `objc_retain`.
//! That is not a convenience - it is the only way toll-free bridging can be
//! made to work. `CFStringGetLength` is handed `NSString`s and
//! `-[NSString length]` is sent to `CFStringRef`s, constantly and by code that
//! knows it is allowed to; a parallel refcount and a parallel object model would
//! have to be reconciled at every one of those crossings, and would be wrong at
//! whichever one was forgotten.
//!
//! So each CF "type" is a class in [`objc`], each `CFTypeID` is that class, and
//! the bridge is class identity rather than a conversion.

use std::ffi::c_void;

use objc::class::Class;
use objc::object::Id;

/// A retained, non-null CoreFoundation object.
pub type CFTypeRef = *const c_void;

/// `CFIndex`: a signed `long`, and negative for "not found".
pub type CFIndex = isize;

/// `CFTypeID`. Apple's is a small integer; here it is the class pointer, which
/// is the same thing said more directly - `CFGetTypeID(x) == CFStringGetTypeID()`
/// asks whether `x` is a string, and the class answers that exactly.
pub type CFTypeID = usize;

/// The type ID of `class`.
pub fn type_id(class: &'static Class) -> CFTypeID {
    objc::class::ptr(class) as usize
}

/// The classes that are CoreFoundation types rather than private subclasses
/// of one.
///
/// `__NSCFConstantString` is a `__NSCFString` and must answer
/// `CFStringGetTypeID()`, not a type ID of its own: a guest that switches on
/// `CFGetTypeID` - and `libYoYoGamepad` does, on every value IOKit hands it -
/// would otherwise fall through every arm for a string that came from an image
/// rather than the heap.
static TYPES: std::sync::Mutex<Vec<usize>> = std::sync::Mutex::new(Vec::new());

fn note_type(class: &'static Class) {
    TYPES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(objc::class::ptr(class) as usize);
}

/// The `CFTypeID` an object of `class` reports: the nearest ancestor that is a
/// CoreFoundation type, or zero for something that is not one at all.
pub fn type_id_of(class: &'static Class) -> CFTypeID {
    let types = TYPES.lock().unwrap_or_else(|e| e.into_inner());
    class
        .chain()
        .map(|c| objc::class::ptr(c) as usize)
        .find(|c| types.contains(c))
        .unwrap_or(0)
}

/// The class of `object`, or `None` for null.
///
/// # Safety
///
/// `object` must be null or an object this runtime allocated.
pub unsafe fn class_of(object: CFTypeRef) -> Option<&'static Class> {
    // SAFETY: the caller's obligation.
    unsafe { objc::object::class_of(object as Id) }
}

/// Whether `object` is an instance of `class` or one of its subclasses.
///
/// # Safety
///
/// As [`class_of`].
pub unsafe fn is_kind_of(object: CFTypeRef, class: &'static Class) -> bool {
    // SAFETY: the caller's obligation.
    unsafe { class_of(object) }.is_some_and(|c| c.chain().any(|k| std::ptr::eq(k, class)))
}

/// Register a CoreFoundation class: a subclass of `NSObject` with no ivars of
/// the runtime's own.
///
/// Every CF object's layout is a `#[repr(C)]` struct in this crate rather than a
/// list of ivars, because the layouts are not ours to choose - a `__cfstring`
/// literal in the guest's `__DATA` is four words the compiler laid out, and
/// nothing gets a chance to convert it.
/// An abstract class with no storage of its own, created if it is not there.
///
/// `NSArray` and `NSMutableArray` are registered here rather than in a
/// Foundation crate, for the same reason Apple's CoreFoundation registers them:
/// `__NSCFArray` has to descend from them or a guest's `isKindOfClass:` and its
/// `_OBJC_CLASS_$_NSArray` binding do not agree with the object it is holding.
/// Foundation then adds the methods. The two crates meet in the runtime's class
/// table rather than through a dependency, which is what lets the arrow point
/// this way at all.
pub fn abstract_class(name: &std::ffi::CStr, superclass: Option<&'static Class>) -> &'static Class {
    register(name, superclass, size_of::<usize>(), false)
}

/// The chain `NSFoo` -> `NSMutableFoo`, for a concrete class to descend from.
pub fn mutable_pair(
    immutable: &std::ffi::CStr,
    mutable: &std::ffi::CStr,
) -> &'static Class {
    let base = abstract_class(immutable, None);
    abstract_class(mutable, Some(base))
}

pub fn register(
    name: &std::ffi::CStr,
    superclass: Option<&'static Class>,
    instance_size: usize,
    is_type: bool,
) -> &'static Class {
    objc::bootstrap();
    if let Some(existing) = objc::class::lookup(name) {
        return existing;
    }
    let superclass = superclass
        .unwrap_or_else(|| objc::class::lookup(c"NSObject").expect("NSObject is the runtime's own"));
    // The extra bytes are everything past what the superclass already accounts
    // for. `class_addIvar` is not used: the fields are this crate's, and naming
    // them to the runtime would only invite something to move them.
    let extra = instance_size.saturating_sub(superclass.instance_size());
    let class = match objc::class::allocate_pair(Some(superclass), name, extra) {
        Some(class) => class,
        // Another thread registered this name between the lookup above and
        // here. Its class is as good as this one would have been, and two
        // classes of one name is the outcome that must not happen.
        None => return objc::class::lookup(name).expect("taken by another thread"),
    };
    objc::class::register_pair(class);
    if is_type {
        note_type(class);
    }
    class
}

/// Mark `class` as the CoreFoundation type its subclasses report.
///
/// Used where the type is the *abstract* class - `CFGetTypeID` of any array
/// answers `CFArrayGetTypeID()`, whether the object is a `__NSCFArray` or
/// something a Foundation crate added underneath `NSArray` later.
pub fn note_type_class(class: &'static Class) {
    note_type(class);
}

/// Allocate an instance of `class` whose body is `T`, and hand back the body.
///
/// The returned pointer is the object *and* the struct: `T`'s first field is the
/// `isa`, which is what makes a CF object an Objective-C one.
///
/// # Safety
///
/// `T` must begin with an `isa`-sized field and be the layout `class` was
/// registered with.
pub unsafe fn allocate<T>(class: &'static Class) -> *mut T {
    let object = objc::object::create_instance(class, 0);
    object as *mut T
}

/// The address of the **variable** a pointer-typed global names.
///
/// `kCFRunLoopDefaultMode` is a `CFStringRef` variable, not an object. A guest
/// loads the symbol's address out of its GOT and then loads the object out of
/// *that*, so a symbol defined as the object itself hands the guest the object's
/// `isa` - and `kCFAllocatorDefault`, whose value is null, hands it address
/// zero. Interned by name, because the cell has to be the same one every time a
/// provider is built.
pub fn global_cell(name: &str, value: u64) -> u64 {
    static CELLS: std::sync::Mutex<Vec<(String, u64)>> = std::sync::Mutex::new(Vec::new());
    let mut cells = CELLS.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((_, addr)) = cells.iter().find(|(n, _)| n == name) {
        return *addr;
    }
    // Leaked: the guest holds the address for the life of the process, which is
    // exactly as long as a real framework's `__DATA` word lives.
    let addr = Box::leak(Box::new(value)) as *mut u64 as usize as u64;
    cells.push((name.to_string(), addr));
    addr
}
