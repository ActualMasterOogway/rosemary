//! CoreFoundation, on this tree's own Objective-C runtime.
//!
//! Every CF type here is a class in [`objc`] and every `CFTypeRef` is an object,
//! so `CFRetain` *is* `objc_retain` and toll-free bridging is class identity
//! rather than a conversion. A parallel object model would have to be reconciled
//! at every `CFStringGetLength(nsstring)` and every `[cfstring length]`, and
//! would be wrong at whichever crossing was forgotten.
//!
//! What is here is what the guests in this tree have actually reached for. An
//! entry nobody has needed is absent rather than guessed at, and absent means
//! the import is refused by name - see `darwin::divergent` for why that is the
//! rule everywhere in this project.

pub mod api;
pub mod base;
pub mod callbacks;
pub mod collections;
pub mod runloop;
mod send;
pub mod string;
pub mod sysconfig;

/// Install the classes this crate owns. Idempotent.
pub fn bootstrap() {
    objc::bootstrap();
    string::class();
    string::constant_class();
    collections::array_class();
    collections::dictionary_class();
    collections::number_class();
    collections::boolean_class();
    collections::data_class();
}

/// One entry point, as an address.
macro_rules! at {
    ($f:expr) => {
        $f as *const () as usize as u64
    };
}

/// The address `name` has here, or `None`.
///
/// `name` is the C identifier, without the leading underscore a Mach-O export
/// carries - the shape [`darwin::symbol`] uses, and for the same reason.
pub fn symbol(name: &str) -> Option<u64> {
    bootstrap();
    Some(match name {
        "CFRetain" => at!(api::CFRetain),
        "CFRelease" => at!(api::CFRelease),
        "CFAutorelease" => at!(api::CFAutorelease),
        "CFGetRetainCount" => at!(api::CFGetRetainCount),
        "CFGetTypeID" => at!(api::CFGetTypeID),
        "CFEqual" => at!(api::CFEqual),
        "CFHash" => at!(api::CFHash),

        "CFStringGetTypeID" => at!(api::CFStringGetTypeID),
        "CFStringCreateWithCString" => at!(api::CFStringCreateWithCString),
        "CFStringCreateWithCStringNoCopy" => at!(api::CFStringCreateWithCStringNoCopy),
        "CFStringCreateWithBytes" => at!(api::CFStringCreateWithBytes),
        "CFStringCreateCopy" => at!(api::CFStringCreateCopy),
        "CFStringGetLength" => at!(api::CFStringGetLength),
        "CFStringGetMaximumSizeForEncoding" => at!(api::CFStringGetMaximumSizeForEncoding),
        "CFStringGetCString" => at!(api::CFStringGetCString),
        "CFStringGetCStringPtr" => at!(api::CFStringGetCStringPtr),
        "CFStringGetBytes" => at!(api::CFStringGetBytes),
        "CFStringCompare" => at!(api::CFStringCompare),
        "CFStringCreateMutable" => at!(api::CFStringCreateMutable),
        "CFStringAppend" => at!(api::CFStringAppend),
        "CFStringAppendCString" => at!(api::CFStringAppendCString),

        "CFArrayGetTypeID" => at!(api::CFArrayGetTypeID),
        "CFArrayCreate" => at!(api::CFArrayCreate),
        "CFArrayGetCount" => at!(api::CFArrayGetCount),
        "CFArrayGetValueAtIndex" => at!(api::CFArrayGetValueAtIndex),

        "CFDictionaryGetTypeID" => at!(api::CFDictionaryGetTypeID),
        "CFDictionaryCreate" => at!(api::CFDictionaryCreate),
        "CFDictionaryGetCount" => at!(api::CFDictionaryGetCount),
        "CFDictionaryGetValue" => at!(api::CFDictionaryGetValue),
        "CFDictionaryContainsKey" => at!(api::CFDictionaryContainsKey),
        "CFDictionaryGetKeysAndValues" => at!(api::CFDictionaryGetKeysAndValues),

        "CFNumberGetTypeID" => at!(api::CFNumberGetTypeID),
        "CFNumberCreate" => at!(api::CFNumberCreate),
        "CFNumberGetValue" => at!(api::CFNumberGetValue),
        "CFNumberIsFloatType" => at!(api::CFNumberIsFloatType),
        "CFNumberGetByteSize" => at!(api::CFNumberGetByteSize),

        "CFDataGetTypeID" => at!(api::CFDataGetTypeID),
        "CFDataCreate" => at!(api::CFDataCreate),
        "CFDataGetLength" => at!(api::CFDataGetLength),
        "CFDataGetBytePtr" => at!(api::CFDataGetBytePtr),

        "CFRunLoopGetCurrent" => at!(api::CFRunLoopGetCurrent),
        "CFRunLoopGetMain" => at!(api::CFRunLoopGetMain),
        "CFRunLoopRunInMode" => at!(api::CFRunLoopRunInMode),
        "CFRunLoopRun" => at!(api::CFRunLoopRun),
        "CFRunLoopStop" => at!(api::CFRunLoopStop),
        "CFRunLoopWakeUp" => at!(api::CFRunLoopWakeUp),
        "CFRunLoopSourceCreate" => at!(api::CFRunLoopSourceCreate),
        "CFRunLoopSourceSignal" => at!(api::CFRunLoopSourceSignal),
        "CFRunLoopSourceInvalidate" => at!(api::CFRunLoopSourceInvalidate),
        "CFRunLoopSourceIsValid" => at!(api::CFRunLoopSourceIsValid),
        "CFRunLoopAddSource" => at!(api::CFRunLoopAddSource),
        "CFRunLoopRemoveSource" => at!(api::CFRunLoopRemoveSource),
        "CFRunLoopContainsSource" => at!(api::CFRunLoopContainsSource),
        "CFAbsoluteTimeGetCurrent" => at!(api::CFAbsoluteTimeGetCurrent),
        "CFRunLoopTimerCreate" => at!(api::CFRunLoopTimerCreate),
        "CFRunLoopAddTimer" => at!(api::CFRunLoopAddTimer),
        "CFRunLoopRemoveTimer" => at!(api::CFRunLoopRemoveTimer),
        "CFRunLoopTimerInvalidate" => at!(api::CFRunLoopTimerInvalidate),
        "CFRunLoopTimerIsValid" => at!(api::CFRunLoopTimerIsValid),

        // SystemConfiguration, which is a framework of its own and is here
        // because its objects are CoreFoundation's - see [`sysconfig`].
        other => return sysconfig::symbol(other),
    })
}

/// Every name [`symbol`] answers.
///
/// A second list rather than a generated one, because the match above is what a
/// reader checks against the headers and a macro would hide it.
pub const NAMES: &[&str] = &[
    "CFRetain",
    "CFRelease",
    "CFAutorelease",
    "CFGetRetainCount",
    "CFGetTypeID",
    "CFEqual",
    "CFHash",
    "CFStringGetTypeID",
    "CFStringCreateWithCString",
    "CFStringCreateWithCStringNoCopy",
    "CFStringCreateWithBytes",
    "CFStringCreateCopy",
    "CFStringGetLength",
    "CFStringGetMaximumSizeForEncoding",
    "CFStringGetCString",
    "CFStringGetCStringPtr",
    "CFStringGetBytes",
    "CFStringCompare",
    "CFStringCreateMutable",
    "CFStringAppend",
    "CFStringAppendCString",
    "CFArrayGetTypeID",
    "CFArrayCreate",
    "CFArrayGetCount",
    "CFArrayGetValueAtIndex",
    "CFDictionaryGetTypeID",
    "CFDictionaryCreate",
    "CFDictionaryGetCount",
    "CFDictionaryGetValue",
    "CFDictionaryContainsKey",
    "CFDictionaryGetKeysAndValues",
    "CFNumberGetTypeID",
    "CFNumberCreate",
    "CFNumberGetValue",
    "CFNumberIsFloatType",
    "CFNumberGetByteSize",
    "CFDataGetTypeID",
    "CFDataCreate",
    "CFDataGetLength",
    "CFDataGetBytePtr",
    "CFRunLoopGetCurrent",
    "CFRunLoopGetMain",
    "CFRunLoopRunInMode",
    "CFRunLoopRun",
    "CFRunLoopStop",
    "CFRunLoopWakeUp",
    "CFRunLoopSourceCreate",
    "CFRunLoopSourceSignal",
    "CFRunLoopSourceInvalidate",
    "CFRunLoopSourceIsValid",
    "CFRunLoopAddSource",
    "CFRunLoopRemoveSource",
    "CFRunLoopContainsSource",
    "CFAbsoluteTimeGetCurrent",
    "CFRunLoopTimerCreate",
    "CFRunLoopAddTimer",
    "CFRunLoopRemoveTimer",
    "CFRunLoopTimerInvalidate",
    "CFRunLoopTimerIsValid",
];

/// The names [`symbol`] answers, `sysconfig`'s included.
pub fn names() -> Vec<&'static str> {
    NAMES.iter().chain(sysconfig::NAMES).copied().collect()
}

/// A mode name, as a string that lives for the process.
///
/// `kCFRunLoopDefaultMode` is a `CFStringRef` a guest reads once and passes back
/// into every `CFRunLoopAddSource`, so it has to be a real string object and it
/// has to be the same one every time.
fn mode_name_object(name: &'static str) -> u64 {
    static NAMES: std::sync::Mutex<Vec<(&'static str, u64)>> = std::sync::Mutex::new(Vec::new());
    let mut names = NAMES.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((_, addr)) = names.iter().find(|(n, _)| *n == name) {
        return *addr;
    }
    // Retained forever: a global a guest may release is one it can make dangle.
    let object = string::from_bytes(name.as_bytes(), false);
    objc::arc::retain(object as objc::object::Id);
    let addr = object as usize as u64;
    names.push((name, addr));
    addr
}


/// The symbols that are **data**, as `(Mach-O name, address)`.
///
/// `___CFConstantStringClassReference` is the whole reason a guest's `@"literal"`
/// works: the four words the compiler laid out in `__DATA` become a live object
/// of this runtime the moment that one symbol is bound, with nothing copied.
///
/// Two shapes, and mixing them up is a segfault at address zero. A class
/// reference and a callbacks table are the *thing itself*, and the guest uses
/// the symbol's address as the value. Every `kCF...` pointer global is a
/// **variable**: the guest loads the symbol's address and then loads the object
/// out of it, so those go through [`base::global_cell`].
///
/// `kCFAllocatorDefault` and its relatives are cells holding null. Null *is* the
/// default allocator in Apple's CoreFoundation, and this runtime has only the
/// one - so a guest comparing against it, or passing it along, gets the right
/// answer.
pub fn data_symbols() -> Vec<(String, u64)> {
    bootstrap();
    let mut out = vec![
        (
            "___CFConstantStringClassReference".to_string(),
            objc::class::ptr(string::constant_class()) as usize as u64,
        ),
        (
            "_kCFTypeArrayCallBacks".to_string(),
            &callbacks::TYPE_VALUE_CALLBACKS as *const _ as usize as u64,
        ),
        (
            "_kCFTypeDictionaryValueCallBacks".to_string(),
            &callbacks::TYPE_VALUE_CALLBACKS as *const _ as usize as u64,
        ),
        (
            "_kCFTypeDictionaryKeyCallBacks".to_string(),
            &callbacks::TYPE_KEY_CALLBACKS as *const _ as usize as u64,
        ),
    ];
    for name in [
        "_kCFAllocatorDefault",
        "_kCFAllocatorSystemDefault",
        "_kCFAllocatorMalloc",
        "_kCFAllocatorNull",
    ] {
        out.push((name.to_string(), base::global_cell(name, 0)));
    }
    // The run loop's mode names, which a guest passes straight back in and
    // which `CFRunLoopAddSource` compares as strings.
    for (name, mode) in [
        ("_kCFRunLoopDefaultMode", runloop::DEFAULT_MODE),
        ("_kCFRunLoopCommonModes", runloop::COMMON_MODES),
        // Foundation spells the default mode this way, and it is the same mode:
        // a guest that schedules with one and runs with the other has to be
        // talking about the same thing or nothing ever fires.
        ("_NSDefaultRunLoopMode", runloop::DEFAULT_MODE),
    ] {
        out.push((name.to_string(), base::global_cell(name, mode_name_object(mode))));
    }
    // The string-valued globals, each of which is exactly its own name without
    // the `k` and the `Key`. Interned so that a guest comparing one against a
    // key it read back gets identity as well as equality.
    for (symbol, text) in KEY_STRINGS {
        out.push((
            symbol.to_string(),
            base::global_cell(symbol, mode_name_object(text)),
        ));
    }
    for (symbol, value) in [
        ("_kCFBooleanTrue", collections::boolean(true)),
        ("_kCFBooleanFalse", collections::boolean(false)),
    ] {
        out.push((
            symbol.to_string(),
            base::global_cell(symbol, value as usize as u64),
        ));
    }
    out.push((
        "_kCFNull".to_string(),
        base::global_cell("_kCFNull", null_object()),
    ));
    // A `double` variable, not a pointer: the guest loads the symbol's address
    // and reads a floating-point number out of it. 1600 is the Big Sur value,
    // which is the floor every version test in a program of this vintage asks
    // about.
    out.push((
        "_kCFCoreFoundationVersionNumber".to_string(),
        base::global_cell(
            "_kCFCoreFoundationVersionNumber",
            (1600.0f64).to_bits(),
        ),
    ));
    out
}

/// The `CFStringRef` globals whose value is a fixed name.
///
/// Each is a documented constant string rather than something this runtime
/// chooses, which is what makes publishing them safe: a guest using one as a
/// dictionary key gets the key Apple's CoreFoundation would have given it.
///
/// They matter more than they look. An unresolved *function* import is refused
/// by name at the call; an unresolved **data** import binds to zero, and the
/// guest's very next instruction dereferences it - which is a segfault at
/// address zero with nothing anywhere to say which symbol it was.
static KEY_STRINGS: &[(&str, &str)] = &[
    ("_kCFBundleIdentifierKey", "CFBundleIdentifier"),
    ("_kCFBundleNameKey", "CFBundleName"),
    ("_kCFBundleVersionKey", "CFBundleVersion"),
    ("_kCFBundleExecutableKey", "CFBundleExecutable"),
    ("_kCFBundleLocalizationsKey", "CFBundleLocalizations"),
    ("_kCFBundleDevelopmentRegionKey", "CFBundleDevelopmentRegion"),
    ("_kCFBundleInfoDictionaryVersionKey", "CFBundleInfoDictionaryVersion"),
];

/// `kCFNull`, the singleton `NSNull` also answers for.
fn null_object() -> u64 {
    let class = objc::class::lookup(c"NSNull");
    let Some(class) = class else {
        // Foundation registers `NSNull`, and a process using CoreFoundation
        // without it has no null to publish. Zero here is the same as not
        // publishing the symbol, which is what it was before.
        return 0;
    };
    let sel = objc::sel::get("null");
    // SAFETY: `+null` is `id (Class, SEL)` and hands back a singleton kept for
    // the process.
    let null = unsafe {
        let f: unsafe extern "C" fn(objc::object::Id, objc::sel::Sel) -> objc::object::Id =
            std::mem::transmute(objc::msg::entry());
        f(objc::class::ptr(class) as objc::object::Id, sel)
    };
    null as usize as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use base::CFTypeRef;
    // `c_char` is signed on Darwin and unsigned on aarch64 Linux; a buffer
    // spelled `[i8; N]` compiles on one host and not the other.
    use std::ffi::c_char;

    #[test]
    fn every_listed_name_resolves() {
        let missing: Vec<&str> = NAMES.iter().copied().filter(|n| symbol(n).is_none()).collect();
        assert!(missing.is_empty(), "listed but unresolved: {missing:?}");
    }

    /// The fixture in `rosetta3/tests/cfstring.rs`, run against this crate
    /// directly: create, measure, size a buffer, convert, release.
    #[test]
    fn a_string_round_trips_the_way_the_guest_fixture_does() {
        bootstrap();
        let text = c"fmodstudio";
        // SAFETY: a NUL-terminated literal and a real encoding.
        unsafe {
            let s = api::CFStringCreateWithCString(
                std::ptr::null(),
                text.as_ptr(),
                string::encoding::UTF8,
            );
            assert!(!s.is_null());
            let len = api::CFStringGetLength(s);
            assert_eq!(len, 10);
            let max = api::CFStringGetMaximumSizeForEncoding(len, string::encoding::UTF8);
            assert_eq!(max, 30, "three bytes a UTF-16 unit, as Apple's answers");

            let mut buf = [0 as c_char; 256];
            let ok = api::CFStringGetCString(
                s,
                buf.as_mut_ptr(),
                max + 1,
                string::encoding::UTF8,
            );
            assert_eq!(ok, 1, "the conversion must succeed");
            assert_eq!(
                std::ffi::CStr::from_ptr(buf.as_ptr()).to_bytes(),
                b"fmodstudio"
            );
            api::CFRelease(s);
        }
    }

    /// A buffer one byte too small is refused rather than truncated.
    #[test]
    fn a_buffer_that_does_not_fit_is_refused() {
        bootstrap();
        // SAFETY: as above.
        unsafe {
            let s = api::CFStringCreateWithCString(
                std::ptr::null(),
                c"abcde".as_ptr(),
                string::encoding::UTF8,
            );
            let mut buf = [0 as c_char; 8];
            // Five characters plus a NUL needs six.
            assert_eq!(
                api::CFStringGetCString(s, buf.as_mut_ptr(), 5, string::encoding::UTF8),
                0
            );
            assert_eq!(
                api::CFStringGetCString(s, buf.as_mut_ptr(), 6, string::encoding::UTF8),
                1
            );
            api::CFRelease(s);
        }
    }

    /// A constant string is one of this runtime's objects with nothing copied,
    /// and releasing it does not free the image's `__DATA`.
    #[test]
    fn a_constant_string_is_read_in_place_and_never_freed() {
        bootstrap();
        // The four words the compiler lays out, built here by hand: the class
        // reference the image would import, flags, the bytes, and the length.
        #[repr(C)]
        struct Literal {
            isa: u64,
            flags: u64,
            str: *const u8,
            length: i64,
        }
        let bytes = c"a host literal";
        let literal = Literal {
            isa: objc::class::ptr(string::constant_class()) as usize as u64,
            flags: 0x7c8,
            str: bytes.as_ptr() as *const u8,
            length: 14,
        };
        let object = &literal as *const Literal as CFTypeRef;

        // SAFETY: `object` is exactly the layout `__NSCFConstantString` has.
        unsafe {
            assert_eq!(api::CFStringGetLength(object), 14);
            assert_eq!(api::CFGetTypeID(object), api::CFStringGetTypeID());
            let ptr = api::CFStringGetCStringPtr(object, string::encoding::UTF8);
            assert_eq!(ptr as *const u8, bytes.as_ptr() as *const u8, "not in place");

            // Released as many times as anyone likes, and still there.
            api::CFRelease(object);
            api::CFRelease(object);
            assert_eq!(api::CFStringGetLength(object), 14);
        }
    }

    /// `CFEqual` on two strings is about the characters, not the pointers -
    /// which is the first thing toll-free bridging has to get right.
    #[test]
    fn two_strings_with_the_same_characters_are_equal() {
        bootstrap();
        // SAFETY: real strings and a real encoding.
        unsafe {
            let a = api::CFStringCreateWithCString(
                std::ptr::null(),
                c"same".as_ptr(),
                string::encoding::UTF8,
            );
            let b = api::CFStringCreateWithCString(
                std::ptr::null(),
                c"same".as_ptr(),
                string::encoding::UTF8,
            );
            let c = api::CFStringCreateWithCString(
                std::ptr::null(),
                c"other".as_ptr(),
                string::encoding::UTF8,
            );
            assert_ne!(a, b, "two separate objects");
            assert_eq!(api::CFEqual(a, b), 1);
            assert_eq!(api::CFEqual(a, c), 0);
            assert_eq!(api::CFHash(a), api::CFHash(b));
            api::CFRelease(a);
            api::CFRelease(b);
            api::CFRelease(c);
        }
    }
}

/// The collections, checked against what a guest actually does with them.
///
/// `libYoYoGamepad` builds an array of dictionaries of numbers to hand IOKit as
/// HID matching criteria, then reads numbers back out of the properties IOKit
/// returns - which is every one of these in one shape.
#[cfg(test)]
mod collection_tests {
    use super::*;
    use base::CFTypeRef;
    use collections::number_type;
    use std::ffi::c_void;

    fn number(kind: i32, value: i64) -> CFTypeRef {
        // SAFETY: `value` outlives the call and is wider than any `kind` here.
        unsafe {
            api::CFNumberCreate(
                std::ptr::null(),
                kind,
                &value as *const i64 as *const c_void,
            )
        }
    }

    fn string(text: &std::ffi::CStr) -> CFTypeRef {
        // SAFETY: a NUL-terminated literal.
        unsafe {
            api::CFStringCreateWithCString(std::ptr::null(), text.as_ptr(), string::encoding::UTF8)
        }
    }

    #[test]
    fn a_matching_dictionary_reads_back_the_way_iokit_would_build_one() {
        bootstrap();
        let page = string(c"DeviceUsagePage");
        let usage = string(c"DeviceUsage");
        let (one, four) = (number(number_type::SINT32, 1), number(number_type::SINT32, 4));

        // SAFETY: real objects and the type callbacks this crate exports.
        unsafe {
            let keys = [page, usage];
            let values = [one, four];
            let dict = api::CFDictionaryCreate(
                std::ptr::null(),
                keys.as_ptr(),
                values.as_ptr(),
                2,
                &callbacks::TYPE_KEY_CALLBACKS,
                &callbacks::TYPE_VALUE_CALLBACKS,
            );
            assert!(!dict.is_null());
            assert_eq!(api::CFDictionaryGetCount(dict), 2);
            assert_eq!(api::CFGetTypeID(dict), api::CFDictionaryGetTypeID());

            // Looked up by a *different* string object with the same
            // characters, which is what a guest always does.
            let same = string(c"DeviceUsage");
            assert_ne!(same, usage, "a distinct object, or this proves nothing");
            let found = api::CFDictionaryGetValue(dict, same);
            assert_eq!(found, four);

            let mut read: i32 = 0;
            assert_eq!(
                api::CFNumberGetValue(
                    found,
                    number_type::SINT32,
                    &mut read as *mut i32 as *mut c_void
                ),
                1
            );
            assert_eq!(read, 4);

            // And in an array, as the matching call takes it.
            let array = api::CFArrayCreate(
                std::ptr::null(),
                [dict].as_ptr(),
                1,
                &callbacks::TYPE_VALUE_CALLBACKS,
            );
            assert_eq!(api::CFArrayGetCount(array), 1);
            assert_eq!(api::CFArrayGetValueAtIndex(array, 0), dict);
            assert!(api::CFArrayGetValueAtIndex(array, 1).is_null(), "past the end");

            api::CFRelease(same);
            api::CFRelease(array);
            api::CFRelease(dict);
            api::CFRelease(page);
            api::CFRelease(usage);
            api::CFRelease(one);
            api::CFRelease(four);
        }
    }

    /// A collection retains what it holds, so an element released by its
    /// creator is still there.
    #[test]
    fn a_collection_holds_a_reference_of_its_own() {
        bootstrap();
        let element = string(c"held");
        // SAFETY: real objects and the type callbacks.
        unsafe {
            let array = api::CFArrayCreate(
                std::ptr::null(),
                [element].as_ptr(),
                1,
                &callbacks::TYPE_VALUE_CALLBACKS,
            );
            assert_eq!(api::CFGetRetainCount(element), 2);
            api::CFRelease(element);
            // Still readable: the array's reference is the one left.
            assert_eq!(api::CFStringGetLength(api::CFArrayGetValueAtIndex(array, 0)), 4);
            api::CFRelease(array);
        }
    }

    /// Null callbacks mean "these are not objects": nothing is retained and
    /// equality is by pointer. A guest that puts integers in an array relies on
    /// it.
    #[test]
    fn null_callbacks_leave_the_elements_alone() {
        bootstrap();
        // SAFETY: the values are never dereferenced, which is the claim.
        unsafe {
            let raw = [7 as CFTypeRef, 9 as CFTypeRef];
            let array = api::CFArrayCreate(std::ptr::null(), raw.as_ptr(), 2, std::ptr::null());
            assert_eq!(api::CFArrayGetCount(array), 2);
            assert_eq!(api::CFArrayGetValueAtIndex(array, 1), 9 as CFTypeRef);
            api::CFRelease(array);
        }
    }

    /// A `SInt64` past the range of a `double` comes back exactly. Holding
    /// every number as an `f64` would lose the low bits of a HID cookie or a
    /// serial number with nothing to say so.
    #[test]
    fn a_wide_integer_survives_the_round_trip() {
        bootstrap();
        let big = (1i64 << 62) + 12345;
        let n = number(number_type::SINT64, big);
        // SAFETY: a real number and a matching out-parameter.
        unsafe {
            let mut read: i64 = 0;
            assert_eq!(
                api::CFNumberGetValue(n, number_type::SINT64, &mut read as *mut i64 as *mut c_void),
                1
            );
            assert_eq!(read, big);
            assert_eq!(api::CFNumberIsFloatType(n), 0);

            // And asked for as a `SInt32` it says so rather than lying.
            let mut narrow: i32 = 0;
            assert_eq!(
                api::CFNumberGetValue(
                    n,
                    number_type::SINT32,
                    &mut narrow as *mut i32 as *mut c_void
                ),
                0,
                "a value that does not fit must report that it does not"
            );
            api::CFRelease(n);
        }
    }

    /// A negative `SInt8` is negative, which sign extension is the whole of.
    #[test]
    fn a_narrow_signed_number_is_sign_extended() {
        bootstrap();
        let byte: i8 = -3;
        // SAFETY: a one-byte value read as `kCFNumberSInt8Type`.
        unsafe {
            let n = api::CFNumberCreate(
                std::ptr::null(),
                number_type::SINT8,
                &byte as *const i8 as *const c_void,
            );
            let mut read: i64 = 0;
            assert_eq!(
                api::CFNumberGetValue(n, number_type::SINT64, &mut read as *mut i64 as *mut c_void),
                1
            );
            assert_eq!(read, -3);
            api::CFRelease(n);
        }
    }
}
