//! Sending messages, from this runtime rather than for the guest.
//!
//! [`crate::objc::dispatch`] is the guest's side of Objective-C: it takes a
//! message the *guest* sent, works out the signature from the method's type
//! encoding, and calls through. Nothing here needs any of that. The overlay
//! sends a fixed, small set of messages whose signatures are in Apple's headers
//! and are known at compile time, so what it wants is the plain thing: an
//! `objc_msgSend` cast to the prototype the header gives and called.
//!
//! ## Why arm64 makes this short
//!
//! On x86-64 a message returning a large struct goes to `objc_msgSend_stret`
//! and one returning a `long double` to `objc_msgSend_fpret`, so a wrapper has
//! to know the return type's class before it can pick an entry point. AAPCS64
//! has neither: a large return is an indirect result in `x8`, which is an
//! ordinary calling-convention detail Rust already emits, and **every** message
//! goes through `objc_msgSend`. So one transmute per arity covers everything the
//! overlay sends, including the one that returns a `CGRect`.

use std::ffi::{CString, c_void};

use crate::objc::host;

/// An object pointer. Opaque, and never dereferenced.
pub type Id = *mut c_void;

/// A uniqued selector.
pub type Sel = *const c_void;

/// A class by name, or null if this process has not loaded it.
///
/// Null is the ordinary answer rather than an error: a guest that draws with
/// OpenGL has never loaded Metal, and asking is how the overlay finds that out.
pub fn class(name: &str) -> Id {
    let Ok(c) = CString::new(name) else {
        return std::ptr::null_mut();
    };
    // SAFETY: a NUL-terminated name. `objc_lookUpClass` — rather than
    // `objc_getClass` — because it does *not* call the class handler, and a
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

/// Whether `object` implements `selector`.
///
/// **Not politeness — the alternative is fatal.** Sending a selector a class
/// does not implement raises `NSInvalidArgumentException`, and a foreign
/// exception unwinding out of this runtime aborts the process with no
/// diagnostic. The overlay identifies objects by the messages the guest sends
/// *about* them, so it is regularly holding something it believes is a drawable
/// or a command buffer on the strength of one selector, and asking first is what
/// makes that belief safe to act on.
///
/// Asked of the runtime's method tables rather than by sending
/// `respondsToSelector:`, because that is itself a message and the receiver
/// might not be an object at all.
///
/// # Safety
///
/// `object` must be null or a live object pointer. That is a weaker obligation
/// than it looks — the whole point is that nothing about the object's *class* is
/// assumed — but it is not nothing: a pointer to freed memory is read here just
/// as it would be by the send this is protecting.
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
        /// `R` and the argument types must be the method's actual signature, as
        /// its framework header declares it, and `receiver` must respond to
        /// `selector`. Nothing checks either — this is `objc_msgSend`, which has
        /// no signature of its own and takes whichever one the method has.
        #[allow(clippy::missing_safety_doc, non_snake_case)]
        pub unsafe fn $name<R, $($arg),*>(receiver: Id, selector: Sel, $($arg: $arg),*) -> R {
            // SAFETY: `entry` is `objc_msgSend`'s address, and the caller has
            // stated the prototype. Transmuting the address rather than the
            // declaration is the documented way to call it — see
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
/// The overlay runs inside the guest's render loop, and a render loop that
/// presents through a `CAMetalLayer` may or may not have a pool on the stack —
/// SDL's does, a hand-written `CVDisplayLink` callback often does not. Every
/// per-frame object the Metal backend creates is autoreleased, so without a pool
/// of its own the overlay would leak a render pass descriptor and an encoder per
/// frame in exactly the programs least able to afford it.
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
