//! Sending a message from inside this crate.
//!
//! Small and typed rather than general: everything here knows the prototype of
//! the method it is sending, and going through `objc_msgSend` would only add a
//! spill and a reload of registers this caller already has in the right places.

use objc::object::Id;
use objc::sel::Sel;

/// # Safety
///
/// `receiver` must be a live object and the method must return a word.
pub unsafe fn send0_usize(receiver: Id, sel: Sel) -> usize {
    // SAFETY: the caller's obligation.
    let Some(method) = (unsafe { objc::object::class_of(receiver) }).and_then(|c| c.method(sel))
    else {
        return 0;
    };
    // SAFETY: the caller's obligation.
    let f: unsafe extern "C" fn(Id, Sel) -> usize =
        unsafe { std::mem::transmute(method.implementation()) };
    // SAFETY: as above.
    unsafe { f(receiver, sel) }
}

/// # Safety
///
/// `receiver` must be a live object and the method must take one word and
/// return a `BOOL`.
pub unsafe fn send1_bool(receiver: Id, sel: Sel, argument: usize) -> bool {
    // SAFETY: the caller's obligation.
    let Some(method) = (unsafe { objc::object::class_of(receiver) }).and_then(|c| c.method(sel))
    else {
        return false;
    };
    // SAFETY: the caller's obligation.
    let f: unsafe extern "C" fn(Id, Sel, usize) -> bool =
        unsafe { std::mem::transmute(method.implementation()) };
    // SAFETY: as above.
    unsafe { f(receiver, sel, argument) }
}
