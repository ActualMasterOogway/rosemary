use crate::objc::send::{Id, class, class_method, ns_string, sel, send1, send2};

/// The compatibility defaults this runtime registers, and why each is here.
const DEFAULTS: &[(&str, bool)] = &[
    // See the module note.
    ("NSOpenGLContextSuppressThreadAssertions", true),
];

/// The 10.14 SDK, before which AppKit's own default for
/// [`NIL_BACKING_LAYER`] looks for a Unity player.
const MOJAVE_SDK: u32 = 0x000A_0E00;

/// AppKit traps on a nil `-makeBackingLayer` unless this is on. Its default is
/// computed from the main executable's SDK, which is this binary's, so a
/// pre-10.14 Unity player (whose OpenGL view answers nil there) would trap the
/// first time a fullscreen transition forces the view to have a layer.
const NIL_BACKING_LAYER: &str = "NSViewFixupNilFromMakeBackingLayer";

/// Apply every compatibility default this runtime needs.
pub fn install() {
    let defaults = class_method("NSUserDefaults", "standardUserDefaults");
    let dict = class_method("NSMutableDictionary", "dictionary");
    if defaults.is_null() || dict.is_null() {
        return;
    }
    let fixup = [(NIL_BACKING_LAYER, true)];
    let program = if legacy_unity() { &fixup[..] } else { &[] };
    for (key, value) in DEFAULTS.iter().chain(program) {
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

/// AppKit's rule for [`NIL_BACKING_LAYER`], asked about the guest: built before
/// 10.14 and carrying `UnityBuildNumber` in its `Info.plist`. A program whose
/// linker recorded no SDK is that old too.
fn legacy_unity() -> bool {
    if darwin::images::program_sdk().is_some_and(|sdk| sdk >= MOJAVE_SDK) {
        return false;
    }
    let bundle = class_method("NSBundle", "mainBundle");
    let key = ns_string("UnityBuildNumber");
    if bundle.is_null() || key.is_null() {
        return false;
    }
    // SAFETY: `-[NSBundle objectForInfoDictionaryKey:]`, against the main
    // bundle with a string key.
    let value = unsafe { send1::<Id, Id>(bundle, sel("objectForInfoDictionaryKey:"), key) };
    !value.is_null()
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
