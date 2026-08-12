//! Framework behaviour gated on **which binary is the main executable**, and
//! the one place this runtime cannot be invisible.
//!
//! Apple's frameworks are full of compatibility gates. Each one asks a question
//! about the *process* — `_CFExecutableLinkedOnOrAfter`, `dyld_sdk_at_least`,
//! `dyld_image_header_containing_address` — and behaves the way an older program
//! expected when the answer is old. That is how a 2017 game keeps working on a
//! 2026 system.
//!
//! Under Rosetta 2 the guest *is* the main executable, so every one of those
//! questions is answered about the guest and the gates open. Under this runtime
//! they are answered about `rosetta3`: an arm64 binary built against the current
//! SDK, whose Objective-C classes are `objc_allocateClassPair` allocations rather
//! than metadata inside any image. A gate that would have opened for the guest
//! closes, and the failure is not a missing symbol or a wrong register — it is
//! the framework working exactly as designed, for a program the guest is not.
//!
//! What is here is therefore neither emulation nor a workaround for a bug in this
//! runtime. It is the small set of places where a gate has to be told the answer
//! it would have computed for the guest.
//!
//! ## The one gate that exists so far
//!
//! `-[NSOpenGLContext setView:]` calls `_os_crash` with
//! `"must be called from the main thread"` unless
//! `NSOpenGLContextSuppressThreadAssertionsForView` says otherwise, and that
//! function suppresses for an old program in two steps: the
//! `NSOpenGLContextSuppressThreadAssertions` user default, and failing that,
//! whether the *view's class* lives in an image whose SDK predates 10.15.1.
//!
//! Unreal Engine 3 sets its context's view from the render thread, which is what
//! the assertion is about and is legal for `NSOpenGLContext` — the assertion was
//! added in 10.15 and exempts anything built before it. A Hat in Time is built
//! against the 10.7 SDK, so on a real system the second step suppresses. Here it
//! cannot: `UOpenGLView`'s class is a host allocation, so
//! `dyld_image_header_containing_address` answers null and the exemption the
//! guest is entitled to is unreachable.
//!
//! So the *first* step is used instead, which is the same answer by the route
//! Apple documents. It is registered in `NSRegistrationDomain` — the lowest-
//! priority domain, held in memory and never written to the guest's preferences
//! — so a guest or a user that sets the key themselves still wins, and nothing
//! about the machine is changed by having run a program under this runtime.

use std::ffi::{CString, c_char, c_void};

/// The compatibility defaults this runtime registers, and why each is here.
///
/// A list rather than a call per key because the shape is the same for all of
/// them and the interesting part is the comment.
const DEFAULTS: &[(&str, bool)] = &[
    // See the module note. Without this, any guest that drives its own
    // `NSOpenGLContext` from a render thread is killed by AppKit on the first
    // `setView:` — not a fault this runtime can report, an `_os_crash`.
    ("NSOpenGLContextSuppressThreadAssertions", true),
];

/// Apply every compatibility default this runtime needs.
///
/// Silent when the frameworks are not loaded: a guest that never touches Cocoa
/// has no `NSUserDefaults` to register with, and dragging Foundation in to set a
/// default about OpenGL would be the wrong trade for the reason
/// [`crate::shims::nslog`] looks its `NSLog` up rather than linking it.
pub fn install() {
    let Some(rt) = Runtime::load() else {
        return;
    };
    let (Some(defaults), Some(dict)) = (
        rt.class_method("NSUserDefaults", "standardUserDefaults"),
        rt.class_method("NSMutableDictionary", "dictionary"),
    ) else {
        return;
    };
    for (key, value) in DEFAULTS {
        let (Some(key), Some(value)) = (rt.string(key), rt.number(*value)) else {
            return;
        };
        // SAFETY: `-[NSMutableDictionary setObject:forKey:]`, whose receiver is
        // the dictionary just created and whose two arguments are objects alive
        // for the duration of the call.
        unsafe { rt.send3(dict, "setObject:forKey:", value, key) };
    }
    // SAFETY: `-[NSUserDefaults registerDefaults:]`, receiver and argument both
    // objects this function made or asked for.
    unsafe { rt.send2(defaults, "registerDefaults:", dict) };
}

/// `objc_msgSend` and the two lookups that make it usable, resolved once.
///
/// Looked up rather than linked, and messaged through `objc_msgSend` rather than
/// through a bridging header, because the whole point is to do nothing at all to
/// a process that has no Cocoa in it.
struct Runtime {
    send: *mut c_void,
    get_class: CStrLookup,
    sel: CStrLookup,
}

/// `objc_getClass` and `sel_registerName`, which have the same shape: a name in,
/// a uniqued runtime object out.
type CStrLookup = unsafe extern "C" fn(*const c_char) -> *mut c_void;

impl Runtime {
    fn load() -> Option<Self> {
        let send = dlsym("objc_msgSend")?;
        // SAFETY: both are libobjc's own, called with their declared signatures.
        unsafe {
            Some(Runtime {
                send,
                get_class: core::mem::transmute::<*mut c_void, CStrLookup>(dlsym("objc_getClass")?),
                sel: core::mem::transmute::<*mut c_void, CStrLookup>(dlsym("sel_registerName")?),
            })
        }
    }

    fn selector(&self, name: &str) -> Option<*mut c_void> {
        let c = CString::new(name).ok()?;
        // SAFETY: `sel_registerName` copies the name and answers a uniqued
        // selector; the string outlives the call.
        Some(unsafe { (self.sel)(c.as_ptr()) })
    }

    fn class(&self, name: &str) -> Option<*mut c_void> {
        let c = CString::new(name).ok()?;
        // SAFETY: as above, for `objc_getClass`, which answers null for a class
        // this process has not loaded rather than faulting.
        let cls = unsafe { (self.get_class)(c.as_ptr()) };
        (!cls.is_null()).then_some(cls)
    }

    /// Send a nullary message. `objc_msgSend` has whichever prototype the
    /// selector's method declares, which is why each arity is transmuted at its
    /// own call site rather than once.
    ///
    /// # Safety
    /// `receiver` must be an object or class, and `name` a nullary method it has.
    unsafe fn send1(&self, receiver: *mut c_void, name: &str) -> Option<*mut c_void> {
        let sel = self.selector(name)?;
        let f: unsafe extern "C" fn(*mut c_void, *mut c_void) -> *mut c_void =
            unsafe { core::mem::transmute(self.send) };
        let r = unsafe { f(receiver, sel) };
        (!r.is_null()).then_some(r)
    }

    /// # Safety
    /// As [`Runtime::send1`], for a method taking one object.
    unsafe fn send2(
        &self,
        receiver: *mut c_void,
        name: &str,
        a: *mut c_void,
    ) -> Option<*mut c_void> {
        let sel = self.selector(name)?;
        let f: unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void) -> *mut c_void =
            unsafe { core::mem::transmute(self.send) };
        let r = unsafe { f(receiver, sel, a) };
        (!r.is_null()).then_some(r)
    }

    /// # Safety
    /// As [`Runtime::send1`], for a method taking two objects.
    unsafe fn send3(
        &self,
        receiver: *mut c_void,
        name: &str,
        a: *mut c_void,
        b: *mut c_void,
    ) -> Option<*mut c_void> {
        let sel = self.selector(name)?;
        let f: unsafe extern "C" fn(
            *mut c_void,
            *mut c_void,
            *mut c_void,
            *mut c_void,
        ) -> *mut c_void = unsafe { core::mem::transmute(self.send) };
        let r = unsafe { f(receiver, sel, a, b) };
        (!r.is_null()).then_some(r)
    }

    /// A nullary class method's result — `+[NSUserDefaults standardUserDefaults]`
    /// and `+[NSMutableDictionary dictionary]` are both this shape.
    fn class_method(&self, class: &str, name: &str) -> Option<*mut c_void> {
        // SAFETY: `class` is a class this process has loaded and `name` a
        // nullary class method Foundation declares on it.
        unsafe { self.send1(self.class(class)?, name) }
    }

    fn string(&self, text: &str) -> Option<*mut c_void> {
        let cls = self.class("NSString")?;
        let sel = self.selector("stringWithUTF8String:")?;
        let c = CString::new(text).ok()?;
        // SAFETY: `+[NSString stringWithUTF8String:]`, whose argument is a
        // NUL-terminated C string that outlives the call and is copied out of.
        let s = unsafe {
            let f: unsafe extern "C" fn(*mut c_void, *mut c_void, *const c_char) -> *mut c_void =
                core::mem::transmute(self.send);
            f(cls, sel, c.as_ptr())
        };
        (!s.is_null()).then_some(s)
    }

    fn number(&self, value: bool) -> Option<*mut c_void> {
        let cls = self.class("NSNumber")?;
        let sel = self.selector("numberWithBool:")?;
        // SAFETY: `+[NSNumber numberWithBool:]`, whose argument is a `BOOL` —
        // one byte, in the low bits of the third argument register.
        let n = unsafe {
            let f: unsafe extern "C" fn(*mut c_void, *mut c_void, u8) -> *mut c_void =
                core::mem::transmute(self.send);
            f(cls, sel, u8::from(value))
        };
        (!n.is_null()).then_some(n)
    }
}

/// Resolve a symbol in an image this process has already loaded.
fn dlsym(name: &str) -> Option<*mut c_void> {
    let c = CString::new(name).ok()?;
    // SAFETY: `RTLD_DEFAULT` (-2) searches loaded images and answers null for a
    // symbol nothing exports, which is checked rather than dereferenced.
    let p = unsafe { libc::dlsym(-2isize as *mut c_void, c.as_ptr()) };
    (!p.is_null()).then_some(p)
}
