//! `NSException`: the object an `@throw` carries.
//!
//! The class exists whether or not anything raises one, because
//! `_OBJC_EHTYPE_$_NSException` is built from it - see `rosetta3::objcexc`. A
//! guest's `@catch (NSException *e)` names that symbol, and an unresolved one
//! binds to zero, which the LSDA reads as `catch (...)`.
//!
//! `+raise:format:` is not here. Raising means throwing, and a throw out of an
//! `extern "C"` method implementation is the failure `objc::fail!` exists to
//! avoid - so a guest that asks for one is refused by name at the send rather
//! than handed an object it will never see again.

use std::ffi::c_void;

use objc::object::Id;
use objc::sel::Sel;

use crate::define::{class_methods, methods};

#[repr(C)]
struct Exception {
    isa: *const c_void,
    name: Id,
    reason: Id,
    user_info: Id,
}

pub const INSTANCE_SIZE: usize = size_of::<Exception>();

/// # Safety
///
/// `receiver` must be an `NSException` this module made.
unsafe fn exception_of<'a>(receiver: Id) -> Option<&'a Exception> {
    // SAFETY: the caller's obligation.
    let is_exception = unsafe { objc::object::class_of(receiver) }.is_some_and(|c| {
        objc::class::lookup(c"NSException").is_some_and(|k| c.chain().any(|x| std::ptr::eq(x, k)))
    });
    // SAFETY: the check above says the storage is this module's.
    is_exception.then(|| unsafe { &*(receiver as *const Exception) })
}

unsafe extern "C" fn name(receiver: Id, _sel: Sel) -> Id {
    // SAFETY: the caller sends to a live object.
    unsafe { exception_of(receiver) }.map_or(std::ptr::null_mut(), |e| e.name)
}

unsafe extern "C" fn reason(receiver: Id, _sel: Sel) -> Id {
    // SAFETY: as above.
    unsafe { exception_of(receiver) }.map_or(std::ptr::null_mut(), |e| e.reason)
}

unsafe extern "C" fn user_info(receiver: Id, _sel: Sel) -> Id {
    // SAFETY: as above.
    unsafe { exception_of(receiver) }.map_or(std::ptr::null_mut(), |e| e.user_info)
}

/// `+exceptionWithName:reason:userInfo:`, and the `-initWith...` it wraps.
unsafe extern "C" fn init(
    receiver: Id,
    _sel: Sel,
    name: Id,
    reason: Id,
    user_info: Id,
) -> Id {
    for object in [name, reason, user_info] {
        objc::arc::retain(object);
    }
    // SAFETY: `Exception` is the layout the class was registered with, and the
    // receiver is an instance of it.
    unsafe {
        let e = receiver as *mut Exception;
        (*e).name = name;
        (*e).reason = reason;
        (*e).user_info = user_info;
    }
    receiver
}

unsafe extern "C" fn exception_with(
    class: Id,
    _sel: Sel,
    name: Id,
    reason: Id,
    user_info: Id,
) -> Id {
    // SAFETY: `class` is `NSException`, which this module registered.
    let exception = unsafe {
        corefoundation::base::allocate::<Exception>(
            objc::class::from_ptr(class as *const objc::class::Class).expect("a class"),
        )
    } as Id;
    // SAFETY: a fresh instance, and the prototype above.
    unsafe { init(exception, objc::sel::get("initWithName:reason:userInfo:"), name, reason, user_info) };
    objc::arc::autorelease(exception)
}

pub(crate) fn install() {
    let class = corefoundation::base::register(c"NSException", None, INSTANCE_SIZE, false);
    class_methods!(class, {
        "exceptionWithName:reason:userInfo:" => exception_with, c"@40@0:8@16@24@32";
    });
    methods!(class, {
        "initWithName:reason:userInfo:" => init, c"@40@0:8@16@24@32";
        "name" => name, c"@16@0:8";
        "reason" => reason, c"@16@0:8";
        "userInfo" => user_info, c"@16@0:8";
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_exception_reports_the_three_it_was_made_with() {
        crate::bootstrap();
        let class = objc::class::lookup(c"NSException").expect("NSException");
        let text = crate::string::from_str("NSRangeException");
        let why = crate::string::from_str("out of bounds");
        // SAFETY: the prototypes the selectors declare.
        unsafe {
            let e = exception_with(
                objc::class::ptr(class) as Id,
                objc::sel::get("exceptionWithName:reason:userInfo:"),
                text,
                why,
                std::ptr::null_mut(),
            );
            assert_eq!(name(e, objc::sel::get("name")), text);
            assert_eq!(reason(e, objc::sel::get("reason")), why);
            assert!(user_info(e, objc::sel::get("userInfo")).is_null());
        }
    }
}
