//! `NSNull`, and the `NSValue` shell above `NSNumber`.
//!
//! `NSNull` is a singleton with no state at all, and its whole job is to be a
//! placeholder a collection can hold where `nil` would end an argument list.

use objc::object::Id;
use objc::sel::Sel;

use crate::define::class_methods;

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

pub(crate) fn install() {
    let null_class =
        corefoundation::base::register(c"NSNull", None, std::mem::size_of::<usize>(), false);
    class_methods!(null_class, {
        "null" => null, c"@16@0:8";
    });

    // `NSValue` is `NSNumber`'s superclass, and a guest's `isKindOfClass:`
    // against it has to succeed. Nothing is implemented on it yet: a guest that
    // sends `-getValue:` gets the runtime's unrecognised-selector report, with
    // the name in it, which is the work queue.
    let value = corefoundation::base::abstract_class(c"NSValue", None);
    let _ = value;
}
