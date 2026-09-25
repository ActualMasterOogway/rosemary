use objc::object::Id;
use objc::sel::Sel;

/// # Safety
///
/// `receiver` must be a live object and the method must return a word.
pub unsafe fn send0_usize(receiver: Id, sel: Sel) -> usize {
    let Some(method) = (unsafe { objc::object::class_of(receiver) }).and_then(|c| c.method(sel))
    else {
        return 0;
    };
    let f: unsafe extern "C" fn(Id, Sel) -> usize =
        unsafe { std::mem::transmute(method.implementation()) };
    unsafe { f(receiver, sel) }
}

/// # Safety
///
/// `receiver` must be a live object and the method must take one word and
/// return a `BOOL`.
pub unsafe fn send1_bool(receiver: Id, sel: Sel, argument: usize) -> bool {
    let Some(method) = (unsafe { objc::object::class_of(receiver) }).and_then(|c| c.method(sel))
    else {
        return false;
    };
    let f: unsafe extern "C" fn(Id, Sel, usize) -> bool =
        unsafe { std::mem::transmute(method.implementation()) };
    unsafe { f(receiver, sel, argument) }
}
