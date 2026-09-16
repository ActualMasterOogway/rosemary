#![allow(
    clippy::cast_slice_from_raw_parts,
    clippy::chunks_exact_to_as_chunks,
    clippy::doc_lazy_continuation,
    clippy::manual_clamp,
    clippy::missing_safety_doc,
    clippy::not_unsafe_ptr_arg_deref
)]

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
mod diskarbitration;
/// Only on Darwin: nowhere else is there another CoreFoundation to meet.
#[cfg(target_os = "macos")]
pub mod foreign;
mod fsevents;
pub mod preferences;
pub mod runloop;
mod send;
mod socket;
pub mod string;
pub mod sysconfig;
mod timezone;
pub mod url;
pub mod urlresource;
pub mod uuid;

/// Install the classes this crate owns. Idempotent.
pub fn bootstrap() {
    objc::bootstrap();
    string::class();
    string::constant_class();
    collections::array_class();
    collections::set_class();
    collections::dictionary_class();
    collections::number_class();
    collections::boolean_class();
    collections::data_class();
    url::class();
    uuid::class();
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

        "CFAllocatorGetTypeID" => at!(api::CFAllocatorGetTypeID),
        "CFAllocatorGetDefault" => at!(api::CFAllocatorGetDefault),
        "CFAllocatorCreate" => at!(api::CFAllocatorCreate),

        "CFStringGetTypeID" => at!(api::CFStringGetTypeID),
        "CFStringCreateWithCString" => at!(api::CFStringCreateWithCString),
        "CFStringCreateWithFileSystemRepresentation" => {
            at!(api::CFStringCreateWithFileSystemRepresentation)
        }
        "CFStringCreateWithCStringNoCopy" => at!(api::CFStringCreateWithCStringNoCopy),
        "CFStringCreateWithBytes" => at!(api::CFStringCreateWithBytes),
        "CFStringCreateWithCharacters" => at!(api::CFStringCreateWithCharacters),
        "CFStringCreateCopy" => at!(api::CFStringCreateCopy),
        "CFStringCreateWithSubstring" => at!(api::CFStringCreateWithSubstring),
        "CFStringGetLength" => at!(api::CFStringGetLength),
        "CFStringGetIntValue" => at!(api::CFStringGetIntValue),
        "CFStringGetDoubleValue" => at!(api::CFStringGetDoubleValue),
        "CFStringGetMaximumSizeForEncoding" => at!(api::CFStringGetMaximumSizeForEncoding),
        "CFStringGetCString" => at!(api::CFStringGetCString),
        "CFStringGetCStringPtr" => at!(api::CFStringGetCStringPtr),
        "CFStringGetBytes" => at!(api::CFStringGetBytes),
        "CFStringCreateExternalRepresentation" => {
            at!(api::CFStringCreateExternalRepresentation)
        }
        "CFStringCompare" => at!(api::CFStringCompare),
        "CFStringCompareWithOptions" => at!(api::CFStringCompareWithOptions),
        "CFStringFindWithOptions" => at!(api::CFStringFindWithOptions),
        "CFStringCreateMutable" => at!(api::CFStringCreateMutable),
        "CFStringCreateMutableCopy" => at!(api::CFStringCreateMutableCopy),
        "CFStringUppercase" => at!(api::CFStringUppercase),
        "CFStringLowercase" => at!(api::CFStringLowercase),
        "CFStringGetCharacterAtIndex" => at!(api::CFStringGetCharacterAtIndex),
        "CFStringGetCharacters" => at!(api::CFStringGetCharacters),
        "CFStringGetCharactersPtr" => at!(api::CFStringGetCharactersPtr),
        "CFStringAppendCharacters" => at!(api::CFStringAppendCharacters),
        "CFStringAppend" => at!(api::CFStringAppend),
        "CFStringInsert" => at!(api::CFStringInsert),
        "CFStringDelete" => at!(api::CFStringDelete),
        "CFStringAppendCString" => at!(api::CFStringAppendCString),

        "CFArrayGetTypeID" => at!(api::CFArrayGetTypeID),
        "CFArrayCreate" => at!(api::CFArrayCreate),
        "CFArrayApplyFunction" => at!(api::CFArrayApplyFunction),
        "CFArrayGetCount" => at!(api::CFArrayGetCount),
        "CFArrayGetValueAtIndex" => at!(api::CFArrayGetValueAtIndex),
        "CFArrayCreateMutable" => at!(api::CFArrayCreateMutable),
        "CFArrayAppendValue" => at!(api::CFArrayAppendValue),

        "CFSetGetTypeID" => at!(api::CFSetGetTypeID),
        "CFSetApplyFunction" => at!(api::CFSetApplyFunction),
        "CFSetGetCount" => at!(api::CFSetGetCount),
        "CFSetGetValues" => at!(api::CFSetGetValues),

        "CFDictionaryGetTypeID" => at!(api::CFDictionaryGetTypeID),
        "CFDictionaryCreate" => at!(api::CFDictionaryCreate),
        "CFDictionaryGetCount" => at!(api::CFDictionaryGetCount),
        "CFDictionaryApplyFunction" => at!(api::CFDictionaryApplyFunction),
        "CFDictionaryGetValue" => at!(api::CFDictionaryGetValue),
        "CFDictionaryGetValueIfPresent" => at!(api::CFDictionaryGetValueIfPresent),
        "CFDictionaryContainsKey" => at!(api::CFDictionaryContainsKey),
        "CFDictionaryGetKeysAndValues" => at!(api::CFDictionaryGetKeysAndValues),
        "CFDictionaryCreateMutable" => at!(api::CFDictionaryCreateMutable),
        "CFDictionarySetValue" => at!(api::CFDictionarySetValue),
        "CFDictionaryAddValue" => at!(api::CFDictionaryAddValue),

        "CFNumberGetTypeID" => at!(api::CFNumberGetTypeID),
        "CFBooleanGetTypeID" => at!(api::CFBooleanGetTypeID),
        "CFBooleanGetValue" => at!(api::CFBooleanGetValue),
        "CFNumberCreate" => at!(api::CFNumberCreate),
        "CFNumberGetValue" => at!(api::CFNumberGetValue),
        "CFNumberIsFloatType" => at!(api::CFNumberIsFloatType),
        "CFNumberGetByteSize" => at!(api::CFNumberGetByteSize),

        "CFDataGetTypeID" => at!(api::CFDataGetTypeID),
        "CFDataCreate" => at!(api::CFDataCreate),
        "CFDataCreateWithBytesNoCopy" => at!(api::CFDataCreateWithBytesNoCopy),
        "CFDataCreateCopy" => at!(api::CFDataCreateCopy),
        "CFDataGetLength" => at!(api::CFDataGetLength),
        "CFDataGetBytePtr" => at!(api::CFDataGetBytePtr),

        "CFUUIDGetTypeID" => at!(api::CFUUIDGetTypeID),
        "CFUUIDCreate" => at!(api::CFUUIDCreate),
        "CFUUIDCreateString" => at!(api::CFUUIDCreateString),
        "CFUUIDGetConstantUUIDWithBytes" => at!(api::CFUUIDGetConstantUUIDWithBytes),

        "CFURLGetTypeID" => at!(api::CFURLGetTypeID),
        "CFTimeZoneCopyDefault" => at!(timezone::copy_default),
        "CFTimeZoneCopySystem" => at!(timezone::copy_system),
        "CFTimeZoneCreateWithName" => at!(timezone::create_with_name),
        "CFTimeZoneGetName" => at!(timezone::get_name),
        "CFURLCreateFromFSRef" => at!(api::CFURLCreateFromFSRef),
        "CFURLGetFSRef" => at!(api::CFURLGetFSRef),
        "CFURLCreateWithFileSystemPath" => at!(api::CFURLCreateWithFileSystemPath),
        "CFURLCreateWithFileSystemPathRelativeToBase" => {
            at!(api::CFURLCreateWithFileSystemPathRelativeToBase)
        }
        "CFURLCreateWithString" => at!(api::CFURLCreateWithString),
        "CFURLGetString" => at!(api::CFURLGetString),
        "CFURLCopyAbsoluteURL" => at!(api::CFURLCopyAbsoluteURL),
        "CFURLCopyPath" => at!(api::CFURLCopyPath),
        "CFURLCopyLastPathComponent" => at!(api::CFURLCopyLastPathComponent),
        "CFURLCopyScheme" => at!(api::CFURLCopyScheme),
        "CFURLCopyHostName" => at!(api::CFURLCopyHostName),
        "CFURLGetPortNumber" => at!(api::CFURLGetPortNumber),
        "CFURLHasDirectoryPath" => at!(api::CFURLHasDirectoryPath),
        "CFURLCreateCopyAppendingPathComponent" => {
            at!(api::CFURLCreateCopyAppendingPathComponent)
        }
        "CFURLCreateCopyDeletingLastPathComponent" => {
            at!(api::CFURLCreateCopyDeletingLastPathComponent)
        }
        "CFURLCreateCopyDeletingPathExtension" => {
            at!(api::CFURLCreateCopyDeletingPathExtension)
        }
        "CFURLCreateStringByAddingPercentEscapes" => {
            at!(api::CFURLCreateStringByAddingPercentEscapes)
        }
        "CFURLCreateStringByReplacingPercentEscapes" => {
            at!(api::CFURLCreateStringByReplacingPercentEscapes)
        }
        "CFURLCreateStringByReplacingPercentEscapesUsingEncoding" => {
            at!(api::CFURLCreateStringByReplacingPercentEscapesUsingEncoding)
        }
        "CFURLCopyFileSystemPath" => at!(api::CFURLCopyFileSystemPath),
        "CFURLGetFileSystemRepresentation" => at!(api::CFURLGetFileSystemRepresentation),

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
        "CFRunLoopObserverCreate" => at!(api::CFRunLoopObserverCreate),
        "CFRunLoopObserverInvalidate" => at!(api::CFRunLoopObserverInvalidate),
        "CFRunLoopObserverIsValid" => at!(api::CFRunLoopObserverIsValid),
        "CFRunLoopObserverGetOrder" => at!(api::CFRunLoopObserverGetOrder),
        "CFRunLoopAddObserver" => at!(api::CFRunLoopAddObserver),
        "CFRunLoopRemoveObserver" => at!(api::CFRunLoopRemoveObserver),
        "CFRunLoopContainsObserver" => at!(api::CFRunLoopContainsObserver),
        "CFRunLoopAddCommonMode" => at!(api::CFRunLoopAddCommonMode),
        "CFSocketCreateWithNative" => at!(socket::create_with_native),
        "CFSocketCreateRunLoopSource" => at!(socket::create_run_loop_source),
        "CFSocketGetNative" => at!(socket::get_native),
        "CFSocketIsValid" => at!(socket::is_valid),
        "CFSocketGetSocketFlags" => at!(socket::get_flags),
        "CFSocketSetSocketFlags" => at!(socket::set_flags),
        "CFSocketEnableCallBacks" => at!(socket::enable_callbacks),
        "CFSocketDisableCallBacks" => at!(socket::disable_callbacks),
        "CFSocketInvalidate" => at!(socket::invalidate),
        "CFRunLoopIsWaiting" => at!(api::CFRunLoopIsWaiting),
        "CFRunLoopTimerGetInterval" => at!(api::CFRunLoopTimerGetInterval),
        "CFRunLoopTimerSetNextFireDate" => at!(api::CFRunLoopTimerSetNextFireDate),
        "CFDataCreateMutable" => at!(api::CFDataCreateMutable),
        "CFDataGetBytes" => at!(api::CFDataGetBytes),
        "CFStringGetSystemEncoding" => at!(api::CFStringGetSystemEncoding),
        "CFArrayContainsValue" => at!(api::CFArrayContainsValue),
        "CFArrayAppendArray" => at!(api::CFArrayAppendArray),
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
        other => {
            return preferences::symbol(other)
                .or_else(|| diskarbitration::symbol(other))
                .or_else(|| fsevents::symbol(other))
                .or_else(|| urlresource::symbol(other))
                .or_else(|| sysconfig::symbol(other));
        }
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
    "CFAllocatorGetTypeID",
    "CFAllocatorGetDefault",
    "CFAllocatorCreate",
    "CFStringGetTypeID",
    "CFStringCreateWithCString",
    "CFStringCreateWithFileSystemRepresentation",
    "CFStringCreateWithCStringNoCopy",
    "CFStringCreateWithBytes",
    "CFStringCreateWithCharacters",
    "CFStringCreateCopy",
    "CFStringCreateWithSubstring",
    "CFStringGetLength",
    "CFStringGetIntValue",
    "CFStringGetDoubleValue",
    "CFStringGetMaximumSizeForEncoding",
    "CFStringGetCString",
    "CFStringGetCStringPtr",
    "CFStringGetBytes",
    "CFStringCreateExternalRepresentation",
    "CFStringCompare",
    "CFStringCompareWithOptions",
    "CFStringFindWithOptions",
    "CFStringCreateMutable",
    "CFStringCreateMutableCopy",
    "CFStringUppercase",
    "CFStringLowercase",
    "CFStringGetCharacterAtIndex",
    "CFStringGetCharacters",
    "CFStringGetCharactersPtr",
    "CFStringAppendCharacters",
    "CFStringAppend",
    "CFStringInsert",
    "CFStringDelete",
    "CFStringAppendCString",
    "CFArrayGetTypeID",
    "CFArrayCreate",
    "CFArrayApplyFunction",
    "CFArrayGetCount",
    "CFArrayGetValueAtIndex",
    "CFArrayCreateMutable",
    "CFArrayAppendValue",
    "CFSetGetTypeID",
    "CFSetApplyFunction",
    "CFSetGetCount",
    "CFSetGetValues",
    "CFDictionaryGetTypeID",
    "CFDictionaryCreate",
    "CFDictionaryGetCount",
    "CFDictionaryApplyFunction",
    "CFDictionaryGetValue",
    "CFDictionaryGetValueIfPresent",
    "CFDictionaryContainsKey",
    "CFDictionaryGetKeysAndValues",
    "CFDictionaryCreateMutable",
    "CFDictionarySetValue",
    "CFDictionaryAddValue",
    "CFNumberGetTypeID",
    "CFBooleanGetTypeID",
    "CFBooleanGetValue",
    "CFNumberCreate",
    "CFNumberGetValue",
    "CFNumberIsFloatType",
    "CFNumberGetByteSize",
    "CFDataGetTypeID",
    "CFDataCreate",
    "CFDataCreateWithBytesNoCopy",
    "CFDataCreateCopy",
    "CFDataGetLength",
    "CFDataGetBytePtr",
    "CFUUIDGetTypeID",
    "CFUUIDCreate",
    "CFUUIDCreateString",
    "CFUUIDGetConstantUUIDWithBytes",
    "CFURLGetTypeID",
    "CFTimeZoneCopyDefault",
    "CFTimeZoneCopySystem",
    "CFTimeZoneCreateWithName",
    "CFTimeZoneGetName",
    "CFURLCreateFromFSRef",
    "CFURLGetFSRef",
    "CFURLCreateWithFileSystemPath",
    "CFURLCreateWithFileSystemPathRelativeToBase",
    "CFURLCreateWithString",
    "CFURLGetString",
    "CFURLCopyAbsoluteURL",
    "CFURLCopyPath",
    "CFURLCopyLastPathComponent",
    "CFURLCopyScheme",
    "CFURLCopyHostName",
    "CFURLGetPortNumber",
    "CFURLHasDirectoryPath",
    "CFURLCreateCopyAppendingPathComponent",
    "CFURLCreateCopyDeletingLastPathComponent",
    "CFURLCreateCopyDeletingPathExtension",
    "CFURLCreateStringByAddingPercentEscapes",
    "CFURLCreateStringByReplacingPercentEscapes",
    "CFURLCreateStringByReplacingPercentEscapesUsingEncoding",
    "CFURLCopyFileSystemPath",
    "CFURLGetFileSystemRepresentation",
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
    "CFRunLoopObserverCreate",
    "CFRunLoopObserverInvalidate",
    "CFRunLoopObserverIsValid",
    "CFRunLoopObserverGetOrder",
    "CFRunLoopAddObserver",
    "CFRunLoopRemoveObserver",
    "CFRunLoopContainsObserver",
    "CFRunLoopAddCommonMode",
    "CFSocketCreateWithNative",
    "CFSocketCreateRunLoopSource",
    "CFSocketGetNative",
    "CFSocketIsValid",
    "CFSocketGetSocketFlags",
    "CFSocketSetSocketFlags",
    "CFSocketEnableCallBacks",
    "CFSocketDisableCallBacks",
    "CFSocketInvalidate",
    "CFRunLoopIsWaiting",
    "CFRunLoopTimerGetInterval",
    "CFRunLoopTimerSetNextFireDate",
    "CFDataCreateMutable",
    "CFDataGetBytes",
    "CFStringGetSystemEncoding",
    "CFArrayContainsValue",
    "CFArrayAppendArray",
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
    NAMES
        .iter()
        .chain(preferences::NAMES)
        .chain(diskarbitration::NAMES)
        .chain(fsevents::NAMES)
        .chain(urlresource::NAMES)
        .chain(sysconfig::NAMES)
        .copied()
        .collect()
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
        // ...and Foundation's spelling of the common-modes set, which is the
        // same set for the same reason.
        ("_NSRunLoopCommonModes", runloop::COMMON_MODES),
    ] {
        out.push((
            name.to_string(),
            base::global_cell(name, mode_name_object(mode)),
        ));
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
        base::global_cell("_kCFCoreFoundationVersionNumber", (1600.0f64).to_bits()),
    ));
    out.extend(preferences::data_symbols());
    out.extend(diskarbitration::data_symbols());
    out.extend(urlresource::data_symbols());
    out.extend(sysconfig::data_symbols());
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
    (
        "_kCFBundleDevelopmentRegionKey",
        "CFBundleDevelopmentRegion",
    ),
    (
        "_kCFBundleInfoDictionaryVersionKey",
        "CFBundleInfoDictionaryVersion",
    ),
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
        let missing: Vec<&str> = NAMES
            .iter()
            .copied()
            .filter(|n| symbol(n).is_none())
            .collect();
        assert!(missing.is_empty(), "listed but unresolved: {missing:?}");
    }

    #[test]
    fn custom_allocator_has_identity_but_default_is_null() {
        bootstrap();
        // SAFETY: this implementation does not inspect the callback context.
        unsafe {
            assert!(api::CFAllocatorGetDefault().is_null());
            let allocator = api::CFAllocatorCreate(std::ptr::null(), std::ptr::null());
            assert!(!allocator.is_null());
            assert_eq!(api::CFGetTypeID(allocator), api::CFAllocatorGetTypeID());
            api::CFRelease(allocator);
        }
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
            let ok = api::CFStringGetCString(s, buf.as_mut_ptr(), max + 1, string::encoding::UTF8);
            assert_eq!(ok, 1, "the conversion must succeed");
            assert_eq!(
                std::ffi::CStr::from_ptr(buf.as_ptr()).to_bytes(),
                b"fmodstudio"
            );
            api::CFRelease(s);
        }
    }

    /// The characters a caller passes are the characters it gets back, counted
    /// in code units rather than in the bytes they encode to.
    #[test]
    fn a_string_is_built_from_code_units_and_keeps_them() {
        bootstrap();
        // "hi" then U+00E9 and U+4E2D - one byte, two and three in UTF-8, so a
        // length in bytes and one in characters cannot agree.
        let text = "hi\u{00E9}\u{4E2D}";
        let units: Vec<u16> = text.encode_utf16().collect();
        // SAFETY: a live buffer described by the count that follows it.
        unsafe {
            let s = api::CFStringCreateWithCharacters(
                std::ptr::null(),
                units.as_ptr(),
                units.len() as base::CFIndex,
            );
            assert!(!s.is_null());
            assert_eq!(api::CFStringGetLength(s), 4, "characters, not bytes");
            let read = string::as_string(s).expect("a string");
            assert!(read.is_wide());
            assert_eq!(read.utf16(), units);
            assert_eq!(read.utf8(), text.as_bytes());

            // A copy is the same string, still measured in characters.
            let copy = api::CFStringCreateCopy(std::ptr::null(), s);
            assert_eq!(api::CFStringGetLength(copy), 4);
            assert_eq!(api::CFEqual(s, copy), 1);
            api::CFRelease(copy);
            api::CFRelease(s);
        }
    }

    /// No characters is the empty string; a negative count and a null pointer
    /// with characters to read are refused.
    #[test]
    fn a_string_of_no_characters_is_empty_rather_than_null() {
        bootstrap();
        // SAFETY: null with a zero count reads nothing.
        unsafe {
            let empty = api::CFStringCreateWithCharacters(std::ptr::null(), std::ptr::null(), 0);
            assert!(!empty.is_null());
            assert_eq!(api::CFStringGetLength(empty), 0);
            api::CFRelease(empty);
            assert!(
                api::CFStringCreateWithCharacters(std::ptr::null(), std::ptr::null(), 4).is_null(),
                "there is nothing to read at null"
            );
            let units = [b'a' as u16];
            assert!(
                api::CFStringCreateWithCharacters(std::ptr::null(), units.as_ptr(), -1).is_null(),
                "a negative count is not a length"
            );
        }
    }

    /// A mutable copy is a separate, writable string that starts out equal -
    /// and stays measured the way the original was.
    #[test]
    fn a_mutable_copy_is_independent_and_keeps_its_characters() {
        bootstrap();
        let text = "caf\u{00E9}";
        let units: Vec<u16> = text.encode_utf16().collect();
        // SAFETY: a live buffer, and strings this test made.
        unsafe {
            let original = api::CFStringCreateWithCharacters(
                std::ptr::null(),
                units.as_ptr(),
                units.len() as base::CFIndex,
            );
            let copy = api::CFStringCreateMutableCopy(std::ptr::null(), 0, original);
            assert!(!copy.is_null());
            assert_eq!(api::CFEqual(original, copy), 1);
            assert_eq!(api::CFStringGetLength(copy), 4, "characters, not bytes");

            api::CFStringAppendCString(copy, c" au lait".as_ptr(), string::encoding::UTF8);
            assert_eq!(
                api::CFStringGetLength(copy),
                12,
                "the append is counted in characters too"
            );
            assert_eq!(
                string::as_string(copy).unwrap().utf8(),
                "caf\u{00E9} au lait".as_bytes()
            );
            // The original is untouched: a copy shares no storage with it.
            assert_eq!(api::CFStringGetLength(original), 4);
            assert_eq!(api::CFEqual(original, copy), 0);
            api::CFRelease(copy);
            api::CFRelease(original);
        }
    }

    /// The 8-bit path, which is what almost every guest copies, and the two
    /// arguments that are not a string.
    #[test]
    fn a_mutable_copy_of_an_ordinary_string_appends() {
        bootstrap();
        // SAFETY: a literal, and null in place of a string.
        unsafe {
            let original = api::CFStringCreateWithCString(
                std::ptr::null(),
                c"abc".as_ptr(),
                string::encoding::UTF8,
            );
            // A maximum length is ignored, as `CFStringCreateMutable`'s is.
            let copy = api::CFStringCreateMutableCopy(std::ptr::null(), 4, original);
            api::CFStringAppendCString(copy, c"defgh".as_ptr(), string::encoding::UTF8);
            assert_eq!(string::as_string(copy).unwrap().utf8(), b"abcdefgh");
            api::CFRelease(copy);
            api::CFRelease(original);
            assert!(
                api::CFStringCreateMutableCopy(std::ptr::null(), 0, std::ptr::null()).is_null(),
                "there is nothing to copy"
            );
        }
    }

    /// Case mapping is Unicode's, in whichever representation the string has,
    /// and only a mutable string is rewritten.
    #[test]
    fn a_mutable_string_is_recased_in_place() {
        bootstrap();
        // SAFETY: strings this test made, and a null locale.
        unsafe {
            let s = api::CFStringCreateWithCString(
                std::ptr::null(),
                c"Stra\xc3\x9fe".as_ptr(),
                string::encoding::UTF8,
            );
            let copy = api::CFStringCreateMutableCopy(std::ptr::null(), 0, s);
            api::CFStringUppercase(copy, std::ptr::null());
            assert_eq!(
                string::as_string(copy).unwrap().utf8(),
                "STRASSE".as_bytes(),
                "the full mapping, so the sharp s becomes two letters"
            );
            api::CFStringLowercase(copy, std::ptr::null());
            assert_eq!(string::as_string(copy).unwrap().utf8(), b"strasse");

            // An immutable string is left alone rather than written through.
            api::CFStringUppercase(s, std::ptr::null());
            assert_eq!(
                string::as_string(s).unwrap().utf8(),
                "Stra\u{00DF}e".as_bytes()
            );
            api::CFRelease(copy);
            api::CFRelease(s);

            // The wide path, which stays wide and stays counted in characters.
            let units: Vec<u16> = "caf\u{00E9}".encode_utf16().collect();
            let wide = api::CFStringCreateWithCharacters(
                std::ptr::null(),
                units.as_ptr(),
                units.len() as base::CFIndex,
            );
            let wide = api::CFStringCreateMutableCopy(std::ptr::null(), 0, wide);
            api::CFStringUppercase(wide, std::ptr::null());
            let read = string::as_string(wide).unwrap();
            assert!(read.is_wide());
            assert_eq!(api::CFStringGetLength(wide), 4);
            assert_eq!(read.utf8(), "CAF\u{00C9}".as_bytes());
            api::CFRelease(wide);
        }
    }

    /// The characters come back the way they went in, and a range outside the
    /// string writes nothing rather than a short answer.
    #[test]
    fn the_characters_are_read_back_by_index_and_by_range() {
        bootstrap();
        let units: Vec<u16> = "caf\u{00E9}s".encode_utf16().collect();
        // SAFETY: a live buffer, and out-parameters sized below.
        unsafe {
            let s = api::CFStringCreateWithCharacters(
                std::ptr::null(),
                units.as_ptr(),
                units.len() as base::CFIndex,
            );
            assert_eq!(api::CFStringGetCharacterAtIndex(s, 3), 0x00E9);
            assert_eq!(api::CFStringGetCharacterAtIndex(s, 5), 0, "past the end");
            assert_eq!(api::CFStringGetCharacterAtIndex(s, -1), 0);

            let mut buffer = [0u16; 8];
            api::CFStringGetCharacters(s, 1, 3, buffer.as_mut_ptr());
            assert_eq!(&buffer[..3], &units[1..4]);
            buffer = [0u16; 8];
            api::CFStringGetCharacters(s, 3, 4, buffer.as_mut_ptr());
            assert_eq!(buffer, [0u16; 8], "a range past the end writes nothing");

            // The pointer is only handed out when it costs nothing.
            assert!(!api::CFStringGetCharactersPtr(s).is_null());
            let eight_bit = api::CFStringCreateWithCString(
                std::ptr::null(),
                c"abc".as_ptr(),
                string::encoding::UTF8,
            );
            assert!(api::CFStringGetCharactersPtr(eight_bit).is_null());

            // Appending characters keeps the target wide.
            let mutable = api::CFStringCreateMutableCopy(std::ptr::null(), 0, s);
            let tail: Vec<u16> = "!".encode_utf16().collect();
            api::CFStringAppendCharacters(mutable, tail.as_ptr(), 1);
            assert_eq!(api::CFStringGetLength(mutable), 6);
            assert!(string::as_string(mutable).unwrap().is_wide());

            api::CFRelease(mutable);
            api::CFRelease(eight_bit);
            api::CFRelease(s);
        }
    }

    /// Unreal hands `wchar_t` text over as UTF-32LE; a nil here was a nil
    /// window title.
    #[test]
    fn a_string_can_be_created_from_utf32_and_utf16_bytes() {
        bootstrap();
        let text = "A Hat in Time \u{1F3A9}";
        let read = |bytes: &[u8], encoding: u32| {
            // SAFETY: a live buffer; the string is released here.
            unsafe {
                let s = api::CFStringCreateWithBytes(
                    std::ptr::null(),
                    bytes.as_ptr(),
                    bytes.len() as base::CFIndex,
                    encoding,
                    0,
                );
                assert!(!s.is_null(), "encoding {encoding:#x} was refused");
                let utf8 = string::as_string(s).unwrap().utf8();
                api::CFRelease(s);
                String::from_utf8(utf8).unwrap()
            }
        };
        let le32: Vec<u8> = text
            .chars()
            .flat_map(|c| (c as u32).to_le_bytes())
            .collect();
        let be32: Vec<u8> = text
            .chars()
            .flat_map(|c| (c as u32).to_be_bytes())
            .collect();
        let be16: Vec<u8> = text.encode_utf16().flat_map(u16::to_be_bytes).collect();
        assert_eq!(read(&le32, string::encoding::UTF32_LE), text);
        assert_eq!(read(&be32, string::encoding::UTF32_BE), text);
        assert_eq!(read(&le32, string::encoding::UTF32), text);
        let marked: Vec<u8> = [0, 0, 0xFE, 0xFF].into_iter().chain(be32).collect();
        assert_eq!(read(&marked, string::encoding::UTF32), text);
        assert_eq!(read(&be16, string::encoding::UTF16_BE), text);
    }

    /// `CFStringGetBytes` in UTF-16: the byte order the encoding names, a mark
    /// only where Apple writes one, and a character count that is not the byte
    /// count.
    #[test]
    fn the_bytes_can_be_asked_for_as_utf16() {
        bootstrap();
        let units: Vec<u16> = "hi\u{00E9}".encode_utf16().collect();
        // SAFETY: a live buffer and out-parameters sized here.
        unsafe {
            let s = api::CFStringCreateWithCharacters(
                std::ptr::null(),
                units.as_ptr(),
                units.len() as base::CFIndex,
            );
            let mut buffer = [0u8; 16];
            let mut used = 0;
            let converted = api::CFStringGetBytes(
                s,
                0,
                3,
                string::encoding::UTF16_BE,
                0,
                0,
                buffer.as_mut_ptr(),
                buffer.len() as base::CFIndex,
                &mut used,
            );
            assert_eq!(converted, 3, "characters, not bytes");
            assert_eq!(used, 6);
            assert_eq!(&buffer[..6], &[0, b'h', 0, b'i', 0x00, 0xE9]);

            // The external representation of the order-less encoding carries a
            // byte-order mark, and the mark is not a character.
            let mut used = 0;
            let converted = api::CFStringGetBytes(
                s,
                0,
                3,
                string::encoding::UTF16,
                0,
                1,
                buffer.as_mut_ptr(),
                buffer.len() as base::CFIndex,
                &mut used,
            );
            assert_eq!(converted, 3);
            assert_eq!(used, 8);
            assert_eq!(&buffer[..4], &[0xFF, 0xFE, b'h', 0]);

            // A buffer that holds one and a half characters holds one.
            let mut used = 0;
            let converted = api::CFStringGetBytes(
                s,
                0,
                3,
                string::encoding::UTF16_LE,
                0,
                0,
                buffer.as_mut_ptr(),
                3,
                &mut used,
            );
            assert_eq!(converted, 1, "half a character is not a character");
            assert_eq!(used, 2);

            // A null buffer is the question of how much room is needed.
            let mut used = 0;
            api::CFStringGetBytes(
                s,
                0,
                3,
                string::encoding::UTF16,
                0,
                0,
                std::ptr::null_mut(),
                0,
                &mut used,
            );
            assert_eq!(used, 6);
            api::CFRelease(s);
        }
    }

    /// `-UTF8String` has to answer for a wide string too, where
    /// `CFStringGetCStringPtr` correctly does not.
    #[test]
    fn a_wide_string_still_answers_utf8_string() {
        bootstrap();
        let units: Vec<u16> = "caf\u{00E9}".encode_utf16().collect();
        // SAFETY: a live buffer, and a message to a string.
        unsafe {
            let s = api::CFStringCreateWithCharacters(
                std::ptr::null(),
                units.as_ptr(),
                units.len() as base::CFIndex,
            );
            assert!(
                api::CFStringGetCStringPtr(s, string::encoding::UTF8).is_null(),
                "the answer is not free, so there is none"
            );
            let text = send::send0_usize(s as objc::object::Id, objc::sel::get("UTF8String"));
            assert_ne!(text, 0, "but -UTF8String always has one");
            assert_eq!(
                std::ffi::CStr::from_ptr(text as *const c_char).to_bytes(),
                "caf\u{00E9}".as_bytes()
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
            assert_eq!(
                ptr as *const u8,
                bytes.as_ptr() as *const u8,
                "not in place"
            );

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
        let (one, four) = (
            number(number_type::SINT32, 1),
            number(number_type::SINT32, 4),
        );

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
            assert!(
                api::CFArrayGetValueAtIndex(array, 1).is_null(),
                "past the end"
            );

            api::CFRelease(same);
            api::CFRelease(array);
            api::CFRelease(dict);
            api::CFRelease(page);
            api::CFRelease(usage);
            api::CFRelease(one);
            api::CFRelease(four);
        }
    }

    #[test]
    fn dictionary_get_value_if_present_reports_and_writes_only_a_match() {
        bootstrap();
        let key = string(c"key");
        let missing = string(c"missing");
        let value = number(number_type::SINT32, 7);

        // SAFETY: real objects, callbacks, and a writable result pointer.
        unsafe {
            let dictionary = api::CFDictionaryCreate(
                std::ptr::null(),
                [key].as_ptr(),
                [value].as_ptr(),
                1,
                &callbacks::TYPE_KEY_CALLBACKS,
                &callbacks::TYPE_VALUE_CALLBACKS,
            );
            let mut result = missing;
            assert_eq!(
                api::CFDictionaryGetValueIfPresent(dictionary, key, &mut result),
                1
            );
            assert_eq!(result, value);

            result = value;
            assert_eq!(
                api::CFDictionaryGetValueIfPresent(dictionary, missing, &mut result),
                0
            );
            assert_eq!(result, value, "an absent key leaves the result unchanged");
            assert_eq!(
                api::CFDictionaryGetValueIfPresent(dictionary, key, std::ptr::null_mut()),
                1
            );

            api::CFRelease(dictionary);
            api::CFRelease(key);
            api::CFRelease(missing);
            api::CFRelease(value);
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
            assert_eq!(
                api::CFStringGetLength(api::CFArrayGetValueAtIndex(array, 0)),
                4
            );
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

    #[test]
    fn array_apply_function_visits_only_the_requested_range() {
        bootstrap();
        unsafe extern "C" fn collect(value: CFTypeRef, context: *mut c_void) {
            // SAFETY: the test passes a pointer to this exact vector.
            unsafe { (&mut *(context as *mut Vec<CFTypeRef>)).push(value) };
        }

        // SAFETY: the raw values are not objects and the callback context is
        // valid for the synchronous invocation.
        unsafe {
            let values = [
                3usize as CFTypeRef,
                5usize as CFTypeRef,
                7usize as CFTypeRef,
            ];
            let array = api::CFArrayCreate(std::ptr::null(), values.as_ptr(), 3, std::ptr::null());
            let mut seen = Vec::new();
            api::CFArrayApplyFunction(
                array,
                1,
                2,
                Some(collect),
                &mut seen as *mut Vec<CFTypeRef> as *mut c_void,
            );
            assert_eq!(seen, values[1..]);

            api::CFArrayApplyFunction(
                array,
                2,
                2,
                Some(collect),
                &mut seen as *mut Vec<CFTypeRef> as *mut c_void,
            );
            assert_eq!(seen, values[1..], "an out-of-bounds range is ignored");
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
