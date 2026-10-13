// The C spellings are the interface, as they are in `api`.
#![allow(non_snake_case)]

use objc::object::Id;

use crate::api::{self, CFAllocatorRef};
use crate::base::{CFIndex, CFTypeRef};
use crate::callbacks::Policy;
use crate::collections::{self, Number};
use crate::plist::Value;

/// `CFPropertyListMutabilityOptions`.
mod mutability {
    pub const MUTABLE_CONTAINERS: u64 = 1;
    pub const MUTABLE_CONTAINERS_AND_LEAVES: u64 = 2;
}

/// `CFPropertyListFormat`, as `*format` reports it.
mod format {
    pub const XML_V1_0: i64 = 100;
    pub const BINARY_V1_0: i64 = 200;
}

/// `NSCocoaErrorDomain`'s `NSPropertyListReadCorruptError`, which is what Apple's
/// reader reports for every malformed document.
const READ_CORRUPT: i64 = 3840;

/// How deep a document may nest, since the input is whatever a guest was handed.
/// A refusal is an ordinary parse failure where a stack overflow is not.
const MAX_DEPTH: usize = 512;

/// `value` as a retained object, or what could not be represented.
///
/// Rejection over approximation: a UID is an archive's reference and not a
/// value, so it is refused rather than turned into a number.
fn build(value: &Value, mutability: u64, depth: usize) -> Result<CFTypeRef, String> {
    if depth > MAX_DEPTH {
        return Err(format!("the document nests deeper than {MAX_DEPTH} levels"));
    }
    let containers = mutability >= mutability::MUTABLE_CONTAINERS;
    Ok(match value {
        Value::String(text) => {
            let string = crate::string::from_bytes(text.as_bytes(), false);
            if mutability == mutability::MUTABLE_CONTAINERS_AND_LEAVES {
                // SAFETY: a string this function just made and owns.
                let copy = unsafe { api::CFStringCreateMutableCopy(std::ptr::null(), 0, string) };
                unsafe { api::CFRelease(string) };
                copy
            } else {
                string
            }
        }
        Value::Integer(v) => collections::new_number(Number::Integer(*v)),
        Value::Real(v) => collections::new_number(Number::Float(*v)),
        Value::Bool(v) => {
            // The boolean singletons are immortal, but a caller releases what
            // it is given, so the reference is taken here.
            let object = collections::boolean(*v);
            // SAFETY: the process-lifetime singleton.
            unsafe { api::CFRetain(object) }
        }
        Value::Data(bytes) => collections::new_data(bytes.clone()),
        Value::Date(interval) => date(*interval)?,
        Value::Null => {
            return Err("the document holds a null, which is not a property list type".into());
        }
        Value::Array(values) => {
            let elements = values
                .iter()
                .map(|v| build(v, mutability, depth + 1))
                .collect::<Result<Vec<_>, _>>();
            let elements = elements?;
            let array =
                collections::new_array(&elements, Policy::from_type_callbacks(), containers);
            release_each(&elements);
            array
        }
        Value::Dictionary(entries) => {
            let mut pairs = Vec::with_capacity(entries.len());
            let mut failure = None;
            for (key, value) in entries {
                match build(value, mutability, depth + 1) {
                    Ok(value) => {
                        let key = crate::string::from_bytes(key.as_bytes(), false);
                        pairs.push((key, value));
                    }
                    Err(why) => {
                        failure = Some(why);
                        break;
                    }
                }
            }
            if let Some(why) = failure {
                for (key, value) in &pairs {
                    release_each(&[*key, *value]);
                }
                return Err(why);
            }
            let dictionary = collections::new_dictionary(
                &pairs,
                Policy::from_key_callbacks(),
                Policy::from_type_callbacks(),
                containers,
            );
            for (key, value) in &pairs {
                release_each(&[*key, *value]);
            }
            dictionary
        }
        Value::Uid(_) => {
            return Err("the document holds a CF$UID, which is an archive's reference".into());
        }
        Value::Unsupported(marker) => {
            return Err(format!(
                "the document holds a value with marker {marker:#x}"
            ));
        }
    })
}

fn release_each(objects: &[CFTypeRef]) {
    for object in objects {
        // SAFETY: each is an object this module made and owns a reference to.
        unsafe { api::CFRelease(*object) };
    }
}

/// A retained `CFDate`.
///
/// The date class is Foundation's, registered when it loads, so it is reached
/// through the runtime and not linked: CoreFoundation does not depend on it.
fn date(interval: f64) -> Result<CFTypeRef, String> {
    let Some(class) = objc::class::lookup(c"NSDate") else {
        return Err("the document holds a date, and no date class is registered".into());
    };
    let sel = objc::sel::get("dateWithTimeIntervalSinceReferenceDate:");
    // SAFETY: `+dateWithTimeIntervalSinceReferenceDate:` is
    // `id (Class, SEL, NSTimeInterval)`, installed by Foundation.
    let object = unsafe {
        let f: unsafe extern "C" fn(Id, objc::sel::Sel, f64) -> Id =
            std::mem::transmute(objc::msg::entry());
        f(objc::class::ptr(class) as Id, sel, interval)
    };
    // The constructor autoreleases; the caller of this module owns a reference.
    Ok(objc::arc::retain(object) as CFTypeRef)
}

/// A retained `CFError`, or null when no error class is registered.
fn error_object(description: &str) -> CFTypeRef {
    let Some(class) = objc::class::lookup(c"NSError") else {
        return std::ptr::null();
    };
    let domain = crate::string::from_bytes(b"NSCocoaErrorDomain", false);
    let message = crate::string::from_bytes(description.as_bytes(), false);
    let key = crate::string::from_bytes(b"NSDebugDescription", false);
    let info = collections::new_dictionary(
        &[(key, message)],
        Policy::from_key_callbacks(),
        Policy::from_type_callbacks(),
        false,
    );
    let sel = objc::sel::get("errorWithDomain:code:userInfo:");
    // SAFETY: `+errorWithDomain:code:userInfo:` is `id (Class, SEL, id,
    // NSInteger, id)`, installed by Foundation.
    let object = unsafe {
        let f: unsafe extern "C" fn(Id, objc::sel::Sel, CFTypeRef, i64, CFTypeRef) -> Id =
            std::mem::transmute(objc::msg::entry());
        f(
            objc::class::ptr(class) as Id,
            sel,
            domain,
            READ_CORRUPT,
            info,
        )
    };
    let object = objc::arc::retain(object) as CFTypeRef;
    release_each(&[domain, key, message, info]);
    object
}

/// `CFPropertyListRef CFPropertyListCreateWithData(CFAllocatorRef, CFDataRef,
/// CFOptionFlags options, CFPropertyListFormat *format, CFErrorRef *error)`
///
/// XML and binary lists are read. `*format` reports which, and a document in
/// neither - the OpenStep spelling included - is an error rather than a guess.
/// `kCFPropertyListMutableContainersAndLeaves` makes strings mutable too; data
/// objects are one class here, so they are the same either way.
///
/// # Safety
///
/// `data` is null or an object this runtime allocated; `format` and `error` are
/// null or writable for one value of their type.
pub unsafe extern "C" fn CFPropertyListCreateWithData(
    _allocator: CFAllocatorRef,
    data: CFTypeRef,
    options: u64,
    format: *mut CFIndex,
    error: *mut CFTypeRef,
) -> CFTypeRef {
    if !error.is_null() {
        // SAFETY: the caller states it is writable.
        unsafe { *error = std::ptr::null() };
    }
    let fail = |why: &str| {
        if !error.is_null() {
            // SAFETY: as above.
            unsafe { *error = error_object(why) };
        }
        std::ptr::null()
    };
    // SAFETY: null or an object this runtime allocated; type-checked.
    let Some(bytes) = (unsafe { collections::data(data) }) else {
        return fail("the argument is not data");
    };
    let binary = bytes.starts_with(b"bplist00");
    let value = match crate::plist::parse(bytes) {
        Ok(value) => value,
        Err(why) => return fail(&why.to_string()),
    };
    match build(&value, options, 0) {
        Ok(object) => {
            if !format.is_null() {
                // SAFETY: the caller states it is writable.
                unsafe {
                    *format = if binary {
                        format::BINARY_V1_0
                    } else {
                        format::XML_V1_0
                    } as CFIndex
                };
            }
            object
        }
        Err(why) => fail(&why),
    }
}

exports::exports! {
    functions {
        "CFPropertyListCreateWithData" => CFPropertyListCreateWithData,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const XML: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict>
<key>name</key><string>rose</string>
<key>count</key><integer>3</integer>
<key>on</key><true/>
<key>list</key><array><string>a</string><real>1.5</real></array>
</dict></plist>"#;

    #[test]
    fn reads_an_xml_list_and_reports_its_format() {
        let data = collections::new_data(XML.to_vec());
        let mut format: CFIndex = 0;
        let mut error: CFTypeRef = std::ptr::null();
        // SAFETY: an object made above and writable locals.
        let list = unsafe {
            CFPropertyListCreateWithData(std::ptr::null(), data, 0, &mut format, &mut error)
        };
        assert!(!list.is_null() && error.is_null());
        assert_eq!(format, 100);
        // SAFETY: a dictionary this call returned.
        let dictionary = unsafe { collections::dictionary(list) }.expect("a dictionary");
        assert_eq!(dictionary.entries.len(), 4);
        assert!(!dictionary.mutable);
        unsafe { api::CFRelease(list) };
        unsafe { api::CFRelease(data) };
    }

    #[test]
    fn garbage_is_null_and_not_a_panic() {
        let data = collections::new_data(b"not a plist".to_vec());
        let mut error: CFTypeRef = std::ptr::null();
        // SAFETY: as above.
        let list = unsafe {
            CFPropertyListCreateWithData(
                std::ptr::null(),
                data,
                0,
                std::ptr::null_mut(),
                &mut error,
            )
        };
        assert!(list.is_null());
        unsafe { api::CFRelease(data) };
    }

    #[test]
    fn mutable_containers_are_honoured() {
        let data = collections::new_data(XML.to_vec());
        // SAFETY: as above.
        let list = unsafe {
            CFPropertyListCreateWithData(
                std::ptr::null(),
                data,
                1,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        let dictionary = unsafe { collections::dictionary(list) }.expect("a dictionary");
        assert!(dictionary.mutable);
        unsafe { api::CFRelease(list) };
        unsafe { api::CFRelease(data) };
    }
}
