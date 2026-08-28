pub mod bundle;
mod define;
pub mod plist;
pub mod pool;
pub mod string;
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
        bundle::class::install();
    });
}

/// The class objects this crate publishes as *data*, Mach-O spelling included.
///
/// A guest's `[NSMutableArray array]` binds `_OBJC_CLASS_$_NSMutableArray` and
/// sends to whatever it gets, so the address has to be the class itself.
pub fn class_data() -> Vec<(String, u64)> {
    bootstrap();
    let mut out = Vec::new();
    for name in CLASSES {
        let Some(class) = std::ffi::CString::new(*name)
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
    "NSAutoreleasePool",
    "NSBundle",
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
            .copied()
            .filter(|n| {
                std::ffi::CString::new(*n)
                    .ok()
                    .and_then(|c| objc::class::lookup(&c))
                    .is_none()
            })
            .collect();
        assert!(missing.is_empty(), "claimed but not registered: {missing:?}");
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
