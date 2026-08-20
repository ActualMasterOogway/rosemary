//! Sending messages, from this runtime rather than for the guest.

use std::ffi::{CString, c_void};

use super::host;

/// An object pointer. Opaque, and never dereferenced.
pub type Id = *mut c_void;

/// A uniqued selector.
pub type Sel = *const c_void;

/// A class by name, or null if this process has not loaded it.
///
/// Null is the ordinary answer rather than an error.
pub fn class(name: &str) -> Id {
    let Ok(c) = CString::new(name) else {
        return std::ptr::null_mut();
    };
    // SAFETY: a NUL-terminated name. `objc_lookUpClass` - rather than
    // `objc_getClass` - because it does *not* call the class handler, and a
    // class this process has genuinely never loaded should come back null
    // instead of causing a lazy load of a framework nothing asked for.
    unsafe { host::objc_lookUpClass(c.as_ptr()) }
}

/// A selector, uniqued into the runtime's table.
pub fn sel(name: &str) -> Sel {
    let Ok(c) = CString::new(name) else {
        return std::ptr::null();
    };
    // SAFETY: a NUL-terminated name; the runtime copies it.
    unsafe { host::sel_registerName(c.as_ptr()) as Sel }
}

/// The result of a nullary class method - `+[NSUserDefaults standardUserDefaults]`
/// and `+[NSMutableDictionary dictionary]` are both this shape.
///
/// Null when this process has not loaded the class.
pub fn class_method(class_name: &str, name: &str) -> Id {
    let cls = class(class_name);
    if cls.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: a nullary class method returning an object, which is what every
    // caller of this asks for.
    unsafe { send0::<Id>(cls, sel(name)) }
}

/// Whether `object` implements `selector`.
///
/// # Safety
///
/// `object` must be null or a live object pointer.
pub unsafe fn responds(object: Id, selector: Sel) -> bool {
    if object.is_null() || selector.is_null() {
        return false;
    }
    // SAFETY: `object_getClass` takes any object pointer, and
    // `class_getInstanceMethod` any class and selector. Both are read-only
    // queries against tables the runtime owns.
    unsafe {
        let class = host::object_getClass(object);
        !class.is_null() && !host::class_getInstanceMethod(class, selector as host::Sel).is_null()
    }
}

/// `objc_msgSend` as an address, to be cast to the prototype the method has.
fn entry() -> *const c_void {
    host::objc_msgSend as *const c_void
}

macro_rules! sender {
    ($name:ident, $($arg:ident),* $(,)?) => {
        /// Send a message whose prototype the caller states.
        ///
        /// # Safety
        ///
        /// `R` and the argument types must be the method's actual signature, as its
        /// framework header declares it, and `receiver` must respond to `selector`.
        #[allow(clippy::missing_safety_doc, non_snake_case)]
        pub unsafe fn $name<R, $($arg),*>(receiver: Id, selector: Sel, $($arg: $arg),*) -> R {
            // SAFETY: `entry` is `objc_msgSend`'s address, and the caller has
            // stated the prototype. Transmuting the address rather than the
            // declaration is the documented way to call it - see
            // [`crate::objc::host::objc_msgSend`], which is declared with no
            // parameters for exactly this reason.
            let f = unsafe {
                std::mem::transmute::<
                    *const c_void,
                    extern "C" fn(Id, Sel, $($arg),*) -> R,
                >(entry())
            };
            f(receiver, selector, $($arg),*)
        }
    };
}

sender!(send0,);
sender!(send1, A);
sender!(send2, A, B);
sender!(send3, A, B, C);
sender!(send4, A, B, C, D);

/// Push an autorelease pool, returning the token that pops it.
///
/// # Safety
///
/// The token must be popped, on the same thread, exactly once.
pub unsafe fn pool_push() -> *mut c_void {
    unsafe extern "C" {
        fn objc_autoreleasePoolPush() -> *mut c_void;
    }
    // SAFETY: public libobjc API, takes nothing.
    unsafe { objc_autoreleasePoolPush() }
}

/// Pop a pool pushed by [`pool_push`].
///
/// # Safety
///
/// `token` must come from [`pool_push`] on this thread and be popped once.
pub unsafe fn pool_pop(token: *mut c_void) {
    unsafe extern "C" {
        fn objc_autoreleasePoolPop(token: *mut c_void);
    }
    // SAFETY: the caller's obligation, stated above.
    unsafe { objc_autoreleasePoolPop(token) }
}

/// An `NSString` for `s`, autoreleased into the caller's pool.
pub fn ns_string(s: &str) -> Id {
    let Ok(c) = CString::new(s) else {
        return std::ptr::null_mut();
    };
    let class = class("NSString");
    if class.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: `+stringWithUTF8String:` is `NSString`'s, takes a C string, and
    // returns an autoreleased instance. `c` outlives the call, which copies.
    unsafe { send1::<Id, *const std::ffi::c_char>(class, sel("stringWithUTF8String:"), c.as_ptr()) }
}
