#![allow(
    clippy::chunks_exact_to_as_chunks,
    clippy::collapsible_if,
    clippy::let_and_return,
    clippy::ptr_eq,
    clippy::unnecessary_cast
)]

pub mod appleevent;
mod attributed_string;
mod block;
mod boxed;
pub mod bundle;
mod characterset;
mod constant;
pub mod cookies;
pub mod defaults;
mod define;
pub mod error;
pub mod exception;
pub mod filemanager;
pub mod formatter;
pub mod geometry;
pub mod json;
pub mod keychain;
pub mod kvc;
pub mod kvo;
pub mod locale;
pub mod notification;
pub mod objectify;
pub mod operation;
pub mod paths;
pub mod plist;
pub mod pool;
pub mod processinfo;
pub mod runtime;
pub mod string;
pub mod thread;
pub mod timer;
pub mod unarchiver;
pub mod url;
mod urlcomponents;
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
        characterset::install();
        attributed_string::install();
        collections::install();
        pool::install();
        geometry::install();
        value::install();
        kvc::install();
        kvo::install();
        runtime::install();
        notification::install();
        constant::install();
        declare_abstract();
        bundle::class::install();
        filemanager::install();
        exception::install();
        locale::install();
        formatter::install();
        timer::install();
        thread::install();
        operation::install();
        url::install();
        urlcomponents::install();
        json::install();
        urlsession::install();
        cookies::install();
        defaults::install();
        processinfo::install();
        appleevent::install();
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
        .or_else(|| keychain::symbol(name))
        .or_else(|| runtime::symbol(name))
        .or_else(|| locale::cf::symbol(name))
        .or_else(|| formatter::cf::symbol(name))
        .or_else(|| timer::cf::symbol(name))
}

/// Every name [`symbol`] answers.
pub fn names() -> impl Iterator<Item = &'static str> {
    paths::NAMES
        .iter()
        .chain(bundle::cf::NAMES)
        .chain(notification::cf::NAMES)
        .chain(keychain::NAMES)
        .chain(runtime::NAMES)
        .chain(locale::cf::NAMES)
        .chain(formatter::cf::NAMES)
        .chain(timer::cf::NAMES)
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

/// An autoreleased, attribute-free `NSAttributedString`.
pub fn attributed_string(text: &str) -> objc::object::Id {
    attributed_string::make(text)
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
    for name in [c"NSFormatter", c"NSThread", c"NSLock", c"NSRecursiveLock"] {
        corefoundation::base::abstract_class(name, None);
    }
}

/// Every class this crate answers for.
pub const CLASSES: &[&str] = &[
    "NSString",
    "NSAttributedString",
    "NSCharacterSet",
    "NSURLComponents",
    "NSHTTPCookie",
    "NSHTTPCookieStorage",
    "NSUserDefaults",
    "NSMutableString",
    "NSArray",
    "NSMutableArray",
    "NSDictionary",
    "NSMutableDictionary",
    "NSData",
    "NSMutableData",
    "NSNumber",
    "NSPredicate",
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
    "NSBlockOperation",
    "NSOperationQueue",
    "NSURL",
    "NSJSONSerialization",
    "NSProcessInfo",
    "NSAppleEventManager",
    "NSAppleEventDescriptor",
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
        .chain(formatter::cf::KEYS)
        .chain(kvo::KEYS)
        .chain(keychain::KEYS)
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
    // `NSZeroPoint`, `NSZeroSize` and `NSZeroRect` are **structs** in Foundation's `__DATA`,
    // not pointers to them: the guest loads the symbol's address and reads two
    // or four doubles out of it. A one-word cell would leave the rest of the
    // read off the end of the allocation.
    out.push(("_NSZeroPoint".to_string(), zeroed::<geometry::NSPoint>()));
    out.push(("_NSZeroSize".to_string(), zeroed::<geometry::NSSize>()));
    out.push(("_NSZeroRect".to_string(), zeroed::<geometry::NSRect>()));
    // The CoreGraphics spellings of the same three, which are separate symbols
    // in a separate framework's `__DATA` even though the types are identical.
    out.push(("_CGPointZero".to_string(), zeroed::<geometry::NSPoint>()));
    out.push(("_CGSizeZero".to_string(), zeroed::<geometry::NSSize>()));
    out.push(("_CGRectZero".to_string(), zeroed::<geometry::NSRect>()));
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
pub const NOTIFICATION_NAMES: &[&str] = &[
    "NSProcessInfoThermalStateDidChangeNotification",
    // Weakly imported by programs that predate it, and still dereferenced
    // without a null check by at least one of them.
    "NSProcessInfoPowerStateDidChangeNotification",
];

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

    /// `[[NSMutableDictionary alloc] init]`, which is a class cluster: what
    /// `+alloc` answers cannot hold anything, so `-init` has to substitute.
    ///
    /// The write-then-read is the point. A placeholder answers `-objectForKey:`
    /// with nil and drops `-setObject:forKey:` on the floor without failing, so
    /// a test that only checked the object was non-nil would pass on the bug
    /// this covers.
    #[test]
    fn alloc_init_gives_a_collection_that_holds_what_is_put_in_it() {
        bootstrap();
        // SAFETY: each prototype is the one its selector declares.
        unsafe {
            let send0: unsafe extern "C" fn(Id, Sel) -> Id =
                std::mem::transmute(objc::msg::entry());
            let send1: unsafe extern "C" fn(Id, Sel, Id) -> Id =
                std::mem::transmute(objc::msg::entry());
            let send2: unsafe extern "C" fn(Id, Sel, Id, Id) =
                std::mem::transmute(objc::msg::entry());
            let count: unsafe extern "C" fn(Id, Sel) -> usize =
                std::mem::transmute(objc::msg::entry());

            let dictionary = send0(
                send0(class(c"NSMutableDictionary"), objc::sel::get("alloc")),
                objc::sel::get("init"),
            );
            let key = string::from_str("deeplink");
            let value = string::from_str("roblox://experiences/start");
            send2(dictionary, objc::sel::get("setObject:forKey:"), value, key);
            assert_eq!(count(dictionary, objc::sel::get("count")), 1);
            assert_eq!(
                send1(dictionary, objc::sel::get("objectForKey:"), key),
                value
            );

            let array = send0(
                send0(class(c"NSMutableArray"), objc::sel::get("alloc")),
                objc::sel::get("init"),
            );
            send1(array, objc::sel::get("addObject:"), value);
            assert_eq!(count(array, objc::sel::get("count")), 1);
        }
    }

    /// `+dictionaryWithObject:forKey:` is declared on the immutable class and
    /// inherited by the mutable one, so what comes back depends on which was
    /// asked - and a program that asks `NSMutableDictionary` writes to it.
    #[test]
    fn a_convenience_constructor_answers_the_class_it_was_sent_to() {
        bootstrap();
        // SAFETY: `+dictionaryWithObject:forKey:` is `id (*)(Class, SEL, id, id)`.
        unsafe {
            let with: unsafe extern "C" fn(Id, Sel, Id, Id) -> Id =
                std::mem::transmute(objc::msg::entry());
            let sel = objc::sel::get("dictionaryWithObject:forKey:");
            let key = string::from_str("k");
            let value = string::from_str("v");
            for (name, mutable) in [(c"NSDictionary", false), (c"NSMutableDictionary", true)] {
                let d = with(class(name), sel, value, key);
                assert_eq!(
                    corefoundation::collections::dictionary(d as corefoundation::base::CFTypeRef)
                        .expect("a dictionary")
                        .mutable,
                    mutable,
                    "{name:?}"
                );
            }
        }
    }

    /// `-description` on a container spells out what it holds, by asking each
    /// element for its own - which is what a program logging one is after.
    #[test]
    fn a_container_describes_its_elements() {
        bootstrap();
        // SAFETY: `-description` is `id (*)(id, SEL)` and `+arrayWithObject:`
        // is `id (*)(Class, SEL, id)`.
        unsafe {
            let send0: unsafe extern "C" fn(Id, Sel) -> Id =
                std::mem::transmute(objc::msg::entry());
            let send1: unsafe extern "C" fn(Id, Sel, Id) -> Id =
                std::mem::transmute(objc::msg::entry());
            let array = send1(
                class(c"NSArray"),
                objc::sel::get("arrayWithObject:"),
                string::from_str("roblox://experiences/start"),
            );
            let text = send0(array, objc::sel::get("description"));
            assert_eq!(string::bytes(text), b"(\n    roblox://experiences/start\n)");
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
