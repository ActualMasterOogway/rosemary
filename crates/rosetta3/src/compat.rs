//! Framework behaviour gated on **which binary is the main executable**, and the one
//! place this runtime cannot be invisible.

use std::ffi::{CString, c_char, c_void};

/// The compatibility defaults this runtime registers, and why each is here.
const DEFAULTS: &[(&str, bool)] = &[
    // See the module note. Without this, any guest that drives its own
    // `NSOpenGLContext` from a render thread is killed by AppKit on the first
    // `setView:` — not a fault this runtime can report, an `_os_crash`.
    ("NSOpenGLContextSuppressThreadAssertions", true),
];

/// Apply every compatibility default this runtime needs.
///
/// Silent when the frameworks are not loaded.
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

    /// Send a nullary message.
    ///
    /// # Safety
    ///
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
