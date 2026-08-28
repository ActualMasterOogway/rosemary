//! `NSAutoreleasePool`, which is the old spelling of `@autoreleasepool`.
//!
//! Still very much in use: `libYoYoGamepad` and every GameMaker runner allocate
//! one per frame. The object holds the token [`objc::arc::pool_push`] returned,
//! and `-drain` or `-release` pops it.

use std::ffi::c_void;

use objc::class::Class;
use objc::object::Id;
use objc::sel::Sel;

use crate::define::methods;

/// The pool object: `isa`, then the token.
#[repr(C)]
struct Pool {
    isa: *const Class,
    token: *mut c_void,
}

unsafe extern "C" fn init(receiver: Id, _sel: Sel) -> Id {
    if receiver.is_null() {
        return receiver;
    }
    // SAFETY: a freshly allocated `NSAutoreleasePool`, whose second word is
    // this method's to fill in. The token is popped by `-drain`.
    unsafe { (*(receiver as *mut Pool)).token = objc::arc::pool_push() };
    receiver
}

unsafe extern "C" fn drain(receiver: Id, _sel: Sel) {
    if receiver.is_null() {
        return;
    }
    // SAFETY: a pool this thread pushed. Popped once: the token is cleared so a
    // second `-drain` - or a `-release` after one - does nothing rather than
    // popping a pool it does not own.
    unsafe {
        let pool = receiver as *mut Pool;
        let token = std::mem::replace(&mut (*pool).token, std::ptr::null_mut());
        if !token.is_null() {
            objc::arc::pool_pop(token);
        }
        objc::arc::release(receiver);
    }
}

pub(crate) fn install() {
    let class = corefoundation::base::register(
        c"NSAutoreleasePool",
        None,
        std::mem::size_of::<Pool>(),
        false,
    );
    methods!(class, {
        "init" => init, c"@16@0:8";
        "drain" => drain, c"v16@0:8";
        "release" => drain, c"v16@0:8";
    });
}
