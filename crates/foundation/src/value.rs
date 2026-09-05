//! `NSNull`, and the `NSValue` shell above `NSNumber`.
//!
//! `NSNull` is a singleton with no state at all, and its whole job is to be a
//! placeholder a collection can hold where `nil` would end an argument list.

use std::ffi::c_void;

use objc::object::Id;
use objc::sel::Sel;

use crate::define::{class_methods, methods};

#[repr(C)]
struct PointerValue {
    isa: *const c_void,
    pointer: *const c_void,
}

fn pointer_value_class() -> &'static objc::class::Class {
    let base = objc::class::lookup(c"NSValue").expect("Foundation registered NSValue");
    corefoundation::base::register(
        c"NSConcretePointerValue",
        Some(base),
        std::mem::size_of::<PointerValue>(),
        false,
    )
}

unsafe extern "C" fn value_with_pointer(_class: Id, _sel: Sel, pointer: *const c_void) -> Id {
    // SAFETY: the body begins with `isa` and matches the registered size.
    let value = unsafe { corefoundation::base::allocate::<PointerValue>(pointer_value_class()) };
    // SAFETY: a fresh instance whose payload is ours to initialize.
    unsafe { (*value).pointer = pointer };
    objc::arc::autorelease(value as Id)
}

unsafe extern "C" fn pointer_value(receiver: Id, _sel: Sel) -> *const c_void {
    // SAFETY: installed only on `NSConcretePointerValue`.
    unsafe { (*(receiver as *const PointerValue)).pointer }
}

/// The one `NSNull`, which `+null` must return every time.
static NULL: std::sync::OnceLock<usize> = std::sync::OnceLock::new();

unsafe extern "C" fn null(class: Id, _sel: Sel) -> Id {
    *NULL.get_or_init(|| {
        // SAFETY: `class` is `NSNull`, which this crate registered.
        let object = unsafe {
            objc::object::create_instance(
                objc::class::from_ptr(class as *const objc::class::Class).expect("a class"),
                0,
            )
        };
        // Retained forever: a singleton that could be deallocated is one a guest
        // can make dangle by releasing what it never owned.
        objc::arc::retain(object);
        object as usize
    }) as Id
}

/// The `NSNull` singleton, for this crate's own converters.
///
/// Sent rather than called, so a guest that replaced `+null` is the one
/// answering - `NSNull` is a class a guest may categorise.
pub(crate) fn null_object() -> Id {
    let class = objc::class::lookup(c"NSNull").expect("Foundation registered NSNull");
    // SAFETY: `+null` is `id (Class, SEL)`, installed below.
    unsafe {
        let f: unsafe extern "C" fn(Id, Sel) -> Id = std::mem::transmute(objc::msg::entry());
        f(objc::class::ptr(class) as Id, objc::sel::get("null"))
    }
}

pub(crate) fn install() {
    let value_class =
        corefoundation::base::register(c"NSValue", None, std::mem::size_of::<usize>(), false);
    class_methods!(value_class, {
        "valueWithPointer:" => value_with_pointer, c"@24@0:8^v16";
    });
    methods!(pointer_value_class(), {
        "pointerValue" => pointer_value, c"^v16@0:8";
    });

    let null_class =
        corefoundation::base::register(c"NSNull", None, std::mem::size_of::<usize>(), false);
    class_methods!(null_class, {
        "null" => null, c"@16@0:8";
    });
}
