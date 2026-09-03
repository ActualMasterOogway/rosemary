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
    let null_class =
        corefoundation::base::register(c"NSNull", None, std::mem::size_of::<usize>(), false);
    class_methods!(null_class, {
        "null" => null, c"@16@0:8";
    });
}
