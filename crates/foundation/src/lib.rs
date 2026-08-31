pub mod bundle;
pub mod error;
mod constant;
mod define;
pub mod exception;
pub mod filemanager;
pub mod geometry;
pub mod kvc;
pub mod locale;
pub mod notification;
pub mod objectify;
pub mod paths;
pub mod plist;
pub mod pool;
pub mod string;
pub mod thread;
pub mod timer;
pub mod unarchiver;
pub mod url;
pub mod urlsession;
pub mod value;

mod collections;

use std::sync::Once;

/// Install this crate's methods and classes. Idempotent.
pub fn bootstrap() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        corefoundation::bootstrap();
        string::install();
        collections::install();
        pool::install();
        value::install();
        geometry::install();
        kvc::install();
        notification::install();
        constant::install();
        declare_abstract();
        bundle::class::install();
        filemanager::install();
        exception::install();
        locale::install();
        timer::install();
        thread::install();
        url::install();
        urlsession::install();
    });
}

/// The C entry points this crate answers, from whichever module owns them.
///
/// Bundle and search-path calls are CoreFoundation and Foundation spellings of
/// questions only this crate can answer - `corefoundation` is underneath it and
/// has neither an `NSBundle` nor a `$HOME`.
pub fn symbol(name: &str) -> Option<u64> {
    paths::symbol(name)
        .or_else(|| bundle::cf::symbol(name))
        .or_else(|| notification::cf::symbol(name))
}

/// Every name [`symbol`] answers.
pub fn names() -> impl Iterator<Item = &'static str> {
    paths::NAMES
        .iter()
        .chain(bundle::cf::NAMES)
        .chain(notification::cf::NAMES)
        .copied()
}

/// This user's home directory.
///
/// `HOME` first, as every Unix does, and the password database only if it is
/// unset - which is the order a guest's own `getenv("HOME")` would see, so the
/// two agree about where a save file goes.
pub fn home_directory() -> String {
    if let Some(home) = std::env::var_os("HOME")
        && !home.is_empty()
    {
        return home.to_string_lossy().into_owned();
    }
    // SAFETY: `getpwuid` returns a pointer into the library's own storage, valid
    // until the next call to it - which this makes before returning.
    unsafe {
        let pw = libc::getpwuid(libc::getuid());
        if pw.is_null() || (*pw).pw_dir.is_null() {
            return "/".to_string();
        }
        std::ffi::CStr::from_ptr((*pw).pw_dir)
            .to_string_lossy()
            .into_owned()
    }
}

/// An integer as an autoreleased `NSNumber`.
pub(crate) fn number(value: i64) -> objc::object::Id {
    let n = corefoundation::collections::new_number(corefoundation::collections::Number::Integer(
        value,
    ));
    // SAFETY: a fresh object.
    unsafe { corefoundation::api::CFAutorelease(n) as objc::object::Id }
}

/// A double as an autoreleased `NSNumber`.
pub(crate) fn real(value: f64) -> objc::object::Id {
    let n =
        corefoundation::collections::new_number(corefoundation::collections::Number::Float(value));
    // SAFETY: a fresh object.
    unsafe { corefoundation::api::CFAutorelease(n) as objc::object::Id }
}

/// The value an `NSNumber` holds, asked for rather than reached into.
///
/// A number is not always a `__NSCFNumber`: `@(9.25)` with a constant operand is
/// an `NSConstantDoubleNumber` the compiler laid out in the image, and a guest
/// may subclass `NSNumber` itself. So the accessor is *sent*, which every one of
/// them answers, instead of the storage being read - which only one of them has.
///
/// # Safety
///
/// `value` must be null or an object this runtime allocated.
pub(crate) unsafe fn number_of(value: objc::object::Id) -> Option<i64> {
    // SAFETY: the caller's obligation; `-longLongValue` is `long long (id, SEL)`.
    unsafe { send_scalar::<i64>(value, "longLongValue") }
}

/// # Safety
///
/// As [`number_of`].
pub(crate) unsafe fn real_of(value: objc::object::Id) -> Option<f64> {
    // SAFETY: the caller's obligation; `-doubleValue` is `double (id, SEL)`.
    unsafe { send_scalar::<f64>(value, "doubleValue") }
}

/// Send a nullary message returning `T`, or `None` if the receiver has no such
/// method.
///
/// # Safety
///
/// `receiver` must be null or a live object, and the method - if there is one -
/// must return `T`.
unsafe fn send_scalar<T>(receiver: objc::object::Id, name: &str) -> Option<T> {
    let sel = objc::sel::get(name);
    // SAFETY: the caller's obligation.
    let method = unsafe { objc::object::class_of(receiver) }?.method(sel)?;
    // SAFETY: the caller's obligation.
    let f: unsafe extern "C" fn(objc::object::Id, objc::sel::Sel) -> T =
        unsafe { std::mem::transmute(method.implementation()) };
    // SAFETY: as above.
    Some(unsafe { f(receiver, sel) })
}

/// The class objects this crate publishes as *data*, Mach-O spelling included.
///
/// A guest's `[NSMutableArray array]` binds `_OBJC_CLASS_$_NSMutableArray` and
/// sends to whatever it gets, so the address has to be the class itself.
pub fn class_data() -> Vec<(String, u64)> {
    bootstrap();
    let mut out = Vec::new();
    for name in CLASSES
        .iter()
        .chain(constant::CLASSES)
        .chain(urlsession::CLASSES)
        .copied()
    {
        let Some(class) = std::ffi::CString::new(name)
            .ok()
            .and_then(|n| objc::class::lookup(&n))
        else {
            continue;
        };
        out.push((
            format!("_OBJC_CLASS_$_{name}"),
            objc::class::ptr(class) as usize as u64,
        ));
        out.push((
            format!("_OBJC_METACLASS_$_{name}"),
            objc::class::ptr(class.metaclass()) as usize as u64,
        ));
    }
    out
}

/// Classes declared so a guest can subclass them, with no methods of their own.
///
/// Declaring one is strictly better than leaving it out. A missing superclass
/// refuses the guest's *whole subclass* - every method on it, and every message
/// to it - whereas a declared one registers, and only the inherited messages the
/// guest actually sends are refused, each by name. That difference is the whole
/// diagnostic.
///
/// Only classes a guest in this tree has been seen to subclass. AppKit's -
/// `NSView`, `NSWindow`, `NSApplication` - belong to AppKit and arrive with it.
fn declare_abstract() {
    for name in [
        c"NSFormatter",
        c"NSThread",
        c"NSLock",
        c"NSRecursiveLock",
        c"NSOperation",
        c"NSOperationQueue",
    ] {
        corefoundation::base::abstract_class(name, None);
    }
}

/// Every class this crate answers for.
pub const CLASSES: &[&str] = &[
    "NSString",
    "NSMutableString",
    "NSArray",
    "NSMutableArray",
    "NSDictionary",
    "NSMutableDictionary",
    "NSData",
    "NSMutableData",
    "NSNumber",
    "NSValue",
    "NSNull",
    "NSNotification",
    "NSNotificationCenter",
    "NSAutoreleasePool",
    "NSBundle",
    "NSDate",
    "NSFileManager",
    "NSLocale",
    "NSException",
    "NSRunLoop",
    "NSTimer",
    "NSFormatter",
    "NSThread",
    "NSLock",
    "NSRecursiveLock",
    "NSOperation",
    "NSOperationQueue",
    "NSURL",
];

// The image-resident number classes are `constant::CLASSES`, published the same
// way and listed there because their layouts are that module's business.

/// Everything this crate publishes as data: the class objects, and the string
/// globals a program reads through their own symbol.
///
/// A key global is a **variable** holding an `NSString *`, not the string, so
/// each goes through a cell - see `corefoundation::base::global_cell`.
pub fn data_symbols() -> Vec<(String, u64)> {
    let mut out = class_data();
    for (symbol, text) in locale::KEYS
        .iter()
        .copied()
        .chain(STRING_GLOBALS.iter().copied())
    {
        out.push((
            symbol.to_string(),
            corefoundation::base::global_cell(symbol, notification::name_object(text)),
        ));
    }
    for name in NOTIFICATION_NAMES {
        let symbol = format!("_{name}");
        let object = notification::name_object(name);
        out.push((
            symbol.clone(),
            corefoundation::base::global_cell(&symbol, object),
        ));
    }
    // `NSZeroPoint` and `NSZeroRect` are **structs** in Foundation's `__DATA`,
    // not pointers to them: the guest loads the symbol's address and reads two
    // or four doubles out of it. A one-word cell would leave the rest of the
    // read off the end of the allocation.
    out.push(("_NSZeroPoint".to_string(), zeroed::<geometry::NSPoint>()));
    out.push(("_NSZeroRect".to_string(), zeroed::<geometry::NSRect>()));
    out
}

/// The address of a zeroed `T` that lives for the process.
///
/// Not interned: each of these names a distinct global, and a guest that writes
/// through one - which nothing should, but the memory is writable either way -
/// must not disturb the other.
fn zeroed<T>() -> u64 {
    Box::leak(vec![0u8; size_of::<T>()].into_boxed_slice()).as_ptr() as usize as u64
}

/// The `NSString *` globals whose value is a fixed name.
///
/// Each is a documented constant rather than something this runtime chooses, so
/// a guest using one as a dictionary key gets the key Apple's Foundation would.
pub const STRING_GLOBALS: &[(&str, &str)] = &[
    ("_NSLocalizedDescriptionKey", "NSLocalizedDescription"),
    ("_NSArgumentDomain", "NSArgumentDomain"),
    // The cookie property keys, whose values are the bare words a `Set-Cookie`
    // attribute is spelled with.
    ("_NSHTTPCookieDomain", "Domain"),
    ("_NSHTTPCookieExpires", "Expires"),
    ("_NSHTTPCookieName", "Name"),
    ("_NSHTTPCookiePath", "Path"),
    ("_NSHTTPCookieSecure", "Secure"),
    ("_NSHTTPCookieValue", "Value"),
    ("_NSHTTPCookieVersion", "Version"),
    (
        "_NSURLVolumeAvailableCapacityForImportantUsageKey",
        "NSURLVolumeAvailableCapacityForImportantUsageKey",
    ),
];

/// The Foundation notification names a program compares against.
///
/// Published whether or not anything posts them, for the reason AppKit's are: an
/// unresolved data import binds to zero and the guest's next instruction
/// dereferences it.
pub const NOTIFICATION_NAMES: &[&str] = &["NSProcessInfoThermalStateDidChangeNotification"];

#[cfg(test)]
mod tests {
    use super::*;
    use objc::object::Id;
    use objc::sel::Sel;

    /// Send a message whose prototype the caller states.
    ///
    /// # Safety
    ///
    /// `R` and the arguments must be the method's actual signature.
    unsafe fn send<R, F>(receiver: Id, name: &str, call: F) -> R
    where
        F: FnOnce(Id, Sel) -> R,
    {
        call(receiver, objc::sel::get(name))
    }

    fn class(name: &std::ffi::CStr) -> Id {
        bootstrap();
        objc::class::ptr(objc::class::lookup(name).expect("registered")) as Id
    }

    #[test]
    fn every_class_this_crate_claims_is_registered() {
        bootstrap();
        let missing: Vec<&str> = CLASSES
            .iter()
            .chain(constant::CLASSES)
            .copied()
            .filter(|n| {
                std::ffi::CString::new(*n)
                    .ok()
                    .and_then(|c| objc::class::lookup(&c))
                    .is_none()
            })
            .collect();
        assert!(
            missing.is_empty(),
            "claimed but not registered: {missing:?}"
        );
    }

    /// The whole of toll-free bridging, in one assertion each way: a string
    /// built through CoreFoundation answers `-length`, and one built through
    /// Foundation answers `CFStringGetLength`.
    #[test]
    fn a_string_answers_to_both_vocabularies() {
        bootstrap();
        // SAFETY: `+stringWithUTF8String:` is `id (*)(Class, SEL, const char *)`
        // and `-length` is `NSUInteger (*)(id, SEL)`.
        unsafe {
            let ns: Id = {
                let f: unsafe extern "C" fn(Id, Sel, *const std::ffi::c_char) -> Id =
                    std::mem::transmute(objc::msg::entry());
                f(
                    class(c"NSString"),
                    objc::sel::get("stringWithUTF8String:"),
                    c"bridged".as_ptr(),
                )
            };
            assert!(!ns.is_null());
            assert_eq!(
                corefoundation::api::CFStringGetLength(ns as corefoundation::base::CFTypeRef),
                7,
                "a Foundation-built string is not a CFString"
            );

            let cf = corefoundation::api::CFStringCreateWithCString(
                std::ptr::null(),
                c"bridged".as_ptr(),
                corefoundation::string::encoding::UTF8,
            );
            let length: usize = send(cf as Id, "length", |r, s| {
                let f: unsafe extern "C" fn(Id, Sel) -> usize =
                    std::mem::transmute(objc::msg::entry());
                f(r, s)
            });
            assert_eq!(length, 7, "a CFString does not answer -length");
            corefoundation::api::CFRelease(cf);
        }
    }

    /// A mutable array built the way a guest builds one, and read back the same
    /// way - through the runtime's real dispatch rather than by calling the
    /// implementations here.
    #[test]
    fn a_mutable_array_holds_what_is_added_to_it() {
        bootstrap();
        // SAFETY: each prototype is the one its selector declares.
        unsafe {
            let send0: unsafe extern "C" fn(Id, Sel) -> Id =
                std::mem::transmute(objc::msg::entry());
            let send1: unsafe extern "C" fn(Id, Sel, Id) = std::mem::transmute(objc::msg::entry());
            let count: unsafe extern "C" fn(Id, Sel) -> usize =
                std::mem::transmute(objc::msg::entry());
            let at: unsafe extern "C" fn(Id, Sel, usize) -> Id =
                std::mem::transmute(objc::msg::entry());

            let array = send0(class(c"NSMutableArray"), objc::sel::get("array"));
            assert!(!array.is_null());
            let element = send0(class(c"NSNumber"), objc::sel::get("class"));
            send1(array, objc::sel::get("addObject:"), element);
            assert_eq!(count(array, objc::sel::get("count")), 1);
            assert_eq!(at(array, objc::sel::get("objectAtIndex:"), 0), element);
        }
    }

    /// An array built the immutable way is marked immutable, which is what
    /// `-addObject:` refuses on.
    ///
    /// Asserted on the flag rather than by sending the message: the refusal is
    /// an `objc::fail!`, which aborts the process. It has to - an `IMP` is an
    /// `extern "C"` function and a panic cannot unwind out of one - so there is
    /// nothing for a test to catch.
    #[test]
    fn an_array_created_immutable_says_so() {
        bootstrap();
        // SAFETY: `+arrayWithObject:` is `id (*)(Class, SEL, id)`.
        unsafe {
            let with_object: unsafe extern "C" fn(Id, Sel, Id) -> Id =
                std::mem::transmute(objc::msg::entry());
            let fixed = with_object(
                class(c"NSArray"),
                objc::sel::get("arrayWithObject:"),
                class(c"NSNumber"),
            );
            assert!(
                !corefoundation::collections::array(fixed as corefoundation::base::CFTypeRef)
                    .expect("an array")
                    .mutable
            );

            let send0: unsafe extern "C" fn(Id, Sel) -> Id =
                std::mem::transmute(objc::msg::entry());
            let growable = send0(class(c"NSMutableArray"), objc::sel::get("array"));
            assert!(
                corefoundation::collections::array(growable as corefoundation::base::CFTypeRef)
                    .expect("an array")
                    .mutable
            );
        }
    }
}
