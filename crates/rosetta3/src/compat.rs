//! Framework behaviour gated on **which binary is the main executable**, and the one
//! place this runtime cannot be invisible.

use crate::objc::send::{Id, class, class_method, ns_string, sel, send1, send2};

/// The compatibility defaults this runtime registers, and why each is here.
const DEFAULTS: &[(&str, bool)] = &[
    // See the module note.
    ("NSOpenGLContextSuppressThreadAssertions", true),
];

/// Apply every compatibility default this runtime needs.
pub fn install() {
    let defaults = class_method("NSUserDefaults", "standardUserDefaults");
    let dict = class_method("NSMutableDictionary", "dictionary");
    if defaults.is_null() || dict.is_null() {
        return;
    }
    for (key, value) in DEFAULTS {
        let (key, value) = (ns_string(key), number(*value));
        if key.is_null() || value.is_null() {
            return;
        }
        // SAFETY: `-[NSMutableDictionary setObject:forKey:]`, whose receiver is
        // the dictionary just created and whose two arguments are objects alive
        // for the duration of the call.
        unsafe { send2::<Id, Id, Id>(dict, sel("setObject:forKey:"), value, key) };
    }
    // SAFETY: `-[NSUserDefaults registerDefaults:]`, receiver and argument both
    // objects this function made or asked for.
    unsafe { send1::<Id, Id>(defaults, sel("registerDefaults:"), dict) };
}

/// `+[NSNumber numberWithBool:]`, whose argument is a `BOOL` - one byte, in the
/// low bits of the third argument register.
fn number(value: bool) -> Id {
    let cls = class("NSNumber");
    if cls.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: the prototype above, against `NSNumber` itself.
    unsafe { send1::<Id, u8>(cls, sel("numberWithBool:"), u8::from(value)) }
}
