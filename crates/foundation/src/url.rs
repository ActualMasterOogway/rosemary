//! `NSURL`'s methods. The object itself is `corefoundation::url`, because
//! `CFURLRef` is `NSURL *` and the storage cannot be two things.

use objc::object::Id;
use objc::sel::Sel;

use crate::define::{class_methods, methods};

/// The URL a receiver holds, as text.
///
/// # Safety
///
/// `receiver` must be null or an object this runtime allocated.
pub unsafe fn text_of(receiver: Id) -> Option<String> {
    // SAFETY: the caller's obligation.
    unsafe { corefoundation::url::as_url(receiver as corefoundation::base::CFTypeRef) }
        .map(|u| u.text.clone())
}

/// `+URLWithString:`, autoreleased.
///
/// nil for text that is not a URL, which is what Apple's answers and what a
/// caller checks for. "Not a URL" is only the empty string here: a scheme this
/// runtime does not recognise is still a URL, and refusing it would be this
/// module deciding what the transport is allowed to be asked for.
unsafe extern "C" fn url_with_string(_class: Id, _sel: Sel, text: Id) -> Id {
    // SAFETY: an `NSString`, or nil.
    let bytes = unsafe { crate::string::bytes(text) };
    let Ok(text) = std::str::from_utf8(&bytes) else {
        return std::ptr::null_mut();
    };
    if text.is_empty() {
        return std::ptr::null_mut();
    }
    // SAFETY: a URL this call just made.
    unsafe { corefoundation::api::CFAutorelease(corefoundation::url::new(text)) as Id }
}

/// `+fileURLWithPath:` and `+fileURLWithPath:isDirectory:`.
unsafe extern "C" fn file_url_with_path(_class: Id, _sel: Sel, path: Id) -> Id {
    // SAFETY: an `NSString`, or nil.
    let bytes = unsafe { crate::string::bytes(path) };
    let Ok(path) = std::str::from_utf8(&bytes) else {
        return std::ptr::null_mut();
    };
    // Whether it is a directory is asked of the file system, which is what
    // Apple's single-argument form does too.
    let directory = std::path::Path::new(path).is_dir();
    // SAFETY: a URL this call just made.
    unsafe {
        corefoundation::api::CFAutorelease(corefoundation::url::new_file_url(path, directory)) as Id
    }
}

unsafe extern "C" fn file_url_with_path_is_directory(
    _class: Id,
    _sel: Sel,
    path: Id,
    directory: u8,
) -> Id {
    // SAFETY: an `NSString`, or nil.
    let bytes = unsafe { crate::string::bytes(path) };
    let Ok(path) = std::str::from_utf8(&bytes) else {
        return std::ptr::null_mut();
    };
    // SAFETY: a URL this call just made.
    unsafe {
        corefoundation::api::CFAutorelease(corefoundation::url::new_file_url(path, directory != 0))
            as Id
    }
}

/// `+fileURLWithPath:relativeToURL:`.
///
/// The base is where a *relative* path is measured from and is ignored for an
/// absolute one, which is the rule for every one of Apple's relative-URL
/// constructors.
unsafe extern "C" fn file_url_with_path_relative_to_url(
    class: Id,
    sel: Sel,
    path: Id,
    base: Id,
) -> Id {
    // SAFETY: an `NSString`, or nil.
    let bytes = unsafe { crate::string::bytes(path) };
    let Ok(path) = std::str::from_utf8(&bytes) else {
        return std::ptr::null_mut();
    };
    // SAFETY: null or an `NSURL`.
    let base = unsafe { corefoundation::url::as_url(base as corefoundation::base::CFTypeRef) }
        .map(|u| u.path())
        .filter(|p| !p.is_empty());
    let resolved = match base {
        Some(base) if !path.starts_with('/') => format!("{}/{path}", base.trim_end_matches('/')),
        _ => path.to_string(),
    };
    let resolved = crate::string::from_str(&resolved);
    // SAFETY: `+fileURLWithPath:` is this module's own, called with the class it
    // was installed on and a string this call just made.
    let url = unsafe { file_url_with_path(class, sel, resolved) };
    objc::arc::release(resolved);
    url
}

/// One accessor, given what it reads off the contents.
macro_rules! accessor {
    ($name:ident, $read:expr) => {
        unsafe extern "C" fn $name(receiver: Id, _sel: Sel) -> Id {
            // SAFETY: the receiver is an `NSURL`.
            let Some(url) = (unsafe {
                corefoundation::url::as_url(receiver as corefoundation::base::CFTypeRef)
            }) else {
                return std::ptr::null_mut();
            };
            let read: fn(&corefoundation::url::Contents) -> Option<String> = $read;
            match read(url) {
                Some(text) => autoreleased(&text),
                None => std::ptr::null_mut(),
            }
        }
    };
}

accessor!(absolute_string, |u| Some(u.text.clone()));
accessor!(path, |u| Some(u.path()).filter(|p| !p.is_empty()));
accessor!(scheme, |u| Some(u.scheme()).filter(|s| !s.is_empty()));

/// `-fileSystemRepresentation`, which is a `const char *` into storage the URL
/// keeps alive - so it is autoreleased as an `NSString` and its bytes handed
/// back, which is where the guest reads them from until the pool drains.
unsafe extern "C" fn file_system_representation(
    receiver: Id,
    _sel: Sel,
) -> *const std::ffi::c_char {
    // SAFETY: the receiver is an `NSURL`.
    let Some(url) =
        (unsafe { corefoundation::url::as_url(receiver as corefoundation::base::CFTypeRef) })
    else {
        return std::ptr::null();
    };
    let Some(path) = url.file_path() else {
        return std::ptr::null();
    };
    let string = crate::string::from_str(&path);
    // SAFETY: a fresh string, autoreleased so its bytes outlive this call by as
    // long as the pool does; `-UTF8String` is `const char * (id, SEL)`.
    unsafe {
        corefoundation::api::CFAutorelease(string as corefoundation::base::CFTypeRef);
        let sel = objc::sel::get("UTF8String");
        match objc::object::class_of(string).and_then(|c| c.method(sel)) {
            Some(method) => {
                let f: unsafe extern "C" fn(Id, Sel) -> *const std::ffi::c_char =
                    std::mem::transmute(method.implementation());
                f(string, sel)
            }
            None => std::ptr::null(),
        }
    }
}

unsafe extern "C" fn is_file_url(receiver: Id, _sel: Sel) -> u8 {
    // SAFETY: the receiver is an `NSURL`.
    let is_file =
        unsafe { corefoundation::url::as_url(receiver as corefoundation::base::CFTypeRef) }
            .is_some_and(|u| u.is_file_url());
    u8::from(is_file)
}

fn autoreleased(text: &str) -> Id {
    let string = crate::string::from_str(text);
    // SAFETY: a string this call just created.
    unsafe { corefoundation::api::CFAutorelease(string as corefoundation::base::CFTypeRef) as Id }
}

/// The file path a URL stands for, for the path arithmetic below.
///
/// `None` for a URL that is not a file URL: appending a path component to an
/// `https:` URL is a different operation - the query and fragment come after
/// the path and would end up inside it - and nothing here needs it, so it is
/// refused rather than approximated.
///
/// # Safety
///
/// `receiver` is null or an `NSURL`.
unsafe fn file_path_of(receiver: Id) -> Option<String> {
    // SAFETY: the caller's obligation.
    unsafe { corefoundation::url::as_url(receiver as corefoundation::base::CFTypeRef) }?.file_path()
}

fn autoreleased_file_url(path: &str, directory: bool) -> Id {
    let url = corefoundation::url::new_file_url(path, directory);
    // SAFETY: a URL this call just made.
    unsafe { corefoundation::api::CFAutorelease(url) as Id }
}

/// Collapse the path-only components Foundation standardizes without consulting
/// the host filesystem. Following symlinks here would make `..` mean something
/// different from the spelling in the URL.
pub(crate) fn standardized_file_path(path: &str) -> String {
    let absolute = path.starts_with('/');
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." if parts.last().is_some_and(|part| *part != "..") => {
                parts.pop();
            }
            ".." if absolute => {}
            other => parts.push(other),
        }
    }

    let mut standardized = parts.join("/");
    if absolute {
        standardized.insert(0, '/');
    }
    if standardized.is_empty() {
        standardized.push(if absolute { '/' } else { '.' });
    }
    standardized
}

/// `-URLByStandardizingPath` for file URLs.
unsafe extern "C" fn url_by_standardizing_path(receiver: Id, _sel: Sel) -> Id {
    // SAFETY: the receiver is an `NSURL`.
    let Some(url) =
        (unsafe { corefoundation::url::as_url(receiver as corefoundation::base::CFTypeRef) })
    else {
        return std::ptr::null_mut();
    };
    let Some(path) = url.file_path() else {
        // Path standardization is meaningful for file URLs. Preserve another
        // URL unchanged rather than corrupting its authority, query or fragment.
        return unsafe {
            corefoundation::api::CFAutorelease(corefoundation::api::CFRetain(
                receiver as corefoundation::base::CFTypeRef,
            )) as Id
        };
    };
    let directory = url.text.ends_with('/');
    autoreleased_file_url(&standardized_file_path(&path), directory)
}

/// `-URLByAppendingPathComponent:`, and the form that says what it is making.
///
/// The single-argument form asks the file system whether the result is a
/// directory, as Apple's does - the trailing slash is the only thing that
/// distinguishes the two URLs, and a caller comparing one against a URL it got
/// from `-URLsForDirectory:` needs them to agree.
unsafe extern "C" fn url_by_appending_path_component(receiver: Id, _sel: Sel, name: Id) -> Id {
    // SAFETY: the receiver is an `NSURL`, and `name` null or an `NSString`.
    let (Some(base), bytes) = (unsafe { file_path_of(receiver) }, unsafe {
        crate::string::bytes(name)
    }) else {
        return std::ptr::null_mut();
    };
    let Ok(name) = String::from_utf8(bytes) else {
        return std::ptr::null_mut();
    };
    let joined = format!("{}/{name}", base.trim_end_matches('/'));
    let directory = std::path::Path::new(&joined).is_dir();
    autoreleased_file_url(&joined, directory)
}

unsafe extern "C" fn url_by_appending_path_component_is_directory(
    receiver: Id,
    _sel: Sel,
    name: Id,
    directory: u8,
) -> Id {
    // SAFETY: as above.
    let (Some(base), bytes) = (unsafe { file_path_of(receiver) }, unsafe {
        crate::string::bytes(name)
    }) else {
        return std::ptr::null_mut();
    };
    let Ok(name) = String::from_utf8(bytes) else {
        return std::ptr::null_mut();
    };
    autoreleased_file_url(
        &format!("{}/{name}", base.trim_end_matches('/')),
        directory != 0,
    )
}

unsafe extern "C" fn url_by_appending_path_extension(receiver: Id, _sel: Sel, extension: Id) -> Id {
    // SAFETY: as above.
    let (Some(base), bytes) = (unsafe { file_path_of(receiver) }, unsafe {
        crate::string::bytes(extension)
    }) else {
        return std::ptr::null_mut();
    };
    let Ok(extension) = String::from_utf8(bytes) else {
        return std::ptr::null_mut();
    };
    autoreleased_file_url(&format!("{base}.{extension}"), false)
}

/// `-URLByDeletingLastPathComponent`, which always names a directory.
unsafe extern "C" fn url_by_deleting_last_path_component(receiver: Id, _sel: Sel) -> Id {
    // SAFETY: the receiver is an `NSURL`.
    let Some(base) = (unsafe { file_path_of(receiver) }) else {
        return std::ptr::null_mut();
    };
    let parent = match base.trim_end_matches('/').rsplit_once('/') {
        // The root's parent is the root, which is what Apple answers.
        Some(("", _)) | None => "/".to_string(),
        Some((head, _)) => head.to_string(),
    };
    autoreleased_file_url(&parent, true)
}

unsafe extern "C" fn last_path_component(receiver: Id, _sel: Sel) -> Id {
    // SAFETY: the receiver is an `NSURL`.
    let Some(base) = (unsafe { file_path_of(receiver) }) else {
        return std::ptr::null_mut();
    };
    let trimmed = base.trim_end_matches('/');
    autoreleased(match trimmed.rsplit_once('/') {
        Some((_, tail)) if !tail.is_empty() => tail,
        _ => "/",
    })
}

/// The value of one `NSURLResourceKey` for `path`, or `None` for a key this
/// tree has no answer for.
///
/// Absent rather than zero, and the distinction is the whole contract: Apple
/// leaves a key it cannot answer *out* of the dictionary, and a caller that
/// reads a size of zero for a file it just wrote would conclude the write
/// failed.
fn resource_value(path: &str, key: &str) -> Option<corefoundation::base::CFTypeRef> {
    let number = |value: i64| {
        corefoundation::collections::new_number(corefoundation::collections::Number::Integer(value))
    };
    let metadata = std::fs::symlink_metadata(path);
    Some(match key {
        "NSURLNameKey" | "NSURLLocalizedNameKey" => {
            let name = path.trim_end_matches('/').rsplit('/').next().unwrap_or("");
            corefoundation::string::from_bytes(name.as_bytes(), false)
        }
        "NSURLPathKey" => corefoundation::string::from_bytes(path.as_bytes(), false),
        "NSURLIsDirectoryKey" => number(i64::from(metadata.ok()?.is_dir())),
        "NSURLIsRegularFileKey" => number(i64::from(metadata.ok()?.is_file())),
        "NSURLIsSymbolicLinkKey" => number(i64::from(metadata.ok()?.file_type().is_symlink())),
        "NSURLFileSizeKey" => number(metadata.ok()?.len() as i64),
        // There is no backup daemon here, so nothing is ever excluded from one.
        // Answered rather than omitted because a caller reads it back to
        // confirm the exclusion it just asked for, and a missing key reads as a
        // failure it would then report.
        "NSURLIsExcludedFromBackupKey" => number(0),
        _ => return None,
    })
}

/// `-resourceValuesForKeys:error:`
///
/// # Safety
///
/// `keys` is null or an `NSArray` of `NSString`, and `error` null or writable.
unsafe extern "C" fn resource_values_for_keys(
    receiver: Id,
    _sel: Sel,
    keys: Id,
    error: *mut Id,
) -> Id {
    if !error.is_null() {
        // SAFETY: the caller states it is writable.
        unsafe { *error = std::ptr::null_mut() };
    }
    // SAFETY: the receiver is an `NSURL`.
    let Some(path) = (unsafe { file_path_of(receiver) }) else {
        return std::ptr::null_mut();
    };
    // SAFETY: null or an array.
    let Some(keys) =
        (unsafe { corefoundation::collections::array(keys as corefoundation::base::CFTypeRef) })
    else {
        return std::ptr::null_mut();
    };

    let mut pairs: Vec<(
        corefoundation::base::CFTypeRef,
        corefoundation::base::CFTypeRef,
    )> = Vec::new();
    for key in keys.values.clone() {
        // SAFETY: an element of a live array, which a caller of this method
        // fills with `NSString`s.
        let name =
            String::from_utf8(unsafe { crate::string::bytes(key as Id) }).unwrap_or_default();
        if let Some(value) = resource_value(&path, &name) {
            pairs.push((key, value));
        }
    }
    let dictionary = corefoundation::collections::new_dictionary(
        &pairs,
        corefoundation::callbacks::Policy::from_key_callbacks(),
        corefoundation::callbacks::Policy::from_type_callbacks(),
        false,
    );
    for (_, value) in &pairs {
        // SAFETY: an object this function made and the dictionary has retained.
        // The keys are the caller's and are not released here.
        unsafe { corefoundation::api::CFRelease(*value) };
    }
    // SAFETY: a fresh dictionary.
    unsafe { corefoundation::api::CFAutorelease(dictionary) as Id }
}

/// `-getResourceValue:forKey:error:`, the one-key form.
///
/// # Safety
///
/// `out` is a writable `id`, and `error` null or writable.
unsafe extern "C" fn get_resource_value(
    receiver: Id,
    _sel: Sel,
    out: *mut Id,
    key: Id,
    error: *mut Id,
) -> u8 {
    if !error.is_null() {
        // SAFETY: the caller states it is writable.
        unsafe { *error = std::ptr::null_mut() };
    }
    if out.is_null() {
        return 0;
    }
    // SAFETY: the caller states it is writable.
    unsafe { *out = std::ptr::null_mut() };
    // SAFETY: the receiver is an `NSURL`, and `key` null or an `NSString`.
    let (Some(path), name) = (unsafe { file_path_of(receiver) }, unsafe {
        crate::string::bytes(key)
    }) else {
        return 0;
    };
    let name = String::from_utf8(name).unwrap_or_default();
    // A key with no value is *success with nil*, which is what Apple documents:
    // "the property is not available" is not an error.
    if let Some(value) = resource_value(&path, &name) {
        // SAFETY: an object this function made, handed over autoreleased.
        unsafe { *out = corefoundation::api::CFAutorelease(value) as Id };
    }
    1
}

/// `-setResourceValue:forKey:error:`, which accepts and records nothing.
///
/// The only key a program sets here is `NSURLIsExcludedFromBackupKey`, and
/// there is no backup daemon on this host to exclude anything from - so the
/// request has already been granted. It succeeds rather than failing because a
/// caller treats a failure as "this directory is not usable" and gives up on
/// it.
unsafe extern "C" fn set_resource_value(
    _receiver: Id,
    _sel: Sel,
    _value: Id,
    _key: Id,
    error: *mut Id,
) -> u8 {
    if !error.is_null() {
        // SAFETY: the caller states it is writable.
        unsafe { *error = std::ptr::null_mut() };
    }
    1
}

pub(crate) fn install() {
    let class = corefoundation::url::class();
    // On `NSURL` rather than on the concrete class, so `+URLWithString:` reaches
    // the spelling a guest actually sends it to.
    let abstract_class = objc::class::lookup(c"NSURL").expect("registered by corefoundation");
    class_methods!(abstract_class, {
        "URLWithString:" => url_with_string, c"@24@0:8@16";
        "fileURLWithPath:" => file_url_with_path, c"@24@0:8@16";
        "fileURLWithPath:isDirectory:" => file_url_with_path_is_directory, c"@28@0:8@16c24";
        "fileURLWithPath:relativeToURL:" => file_url_with_path_relative_to_url,
            c"@32@0:8@16@24";
    });
    methods!(class, {
        "absoluteString" => absolute_string, c"@16@0:8";
        "description" => absolute_string, c"@16@0:8";
        "path" => path, c"@16@0:8";
        "scheme" => scheme, c"@16@0:8";
        "isFileURL" => is_file_url, c"c16@0:8";
        "fileSystemRepresentation" => file_system_representation, c"*16@0:8";
        "URLByAppendingPathComponent:" => url_by_appending_path_component, c"@24@0:8@16";
        "URLByAppendingPathComponent:isDirectory:"
            => url_by_appending_path_component_is_directory, c"@28@0:8@16c24";
        "URLByAppendingPathExtension:" => url_by_appending_path_extension, c"@24@0:8@16";
        "URLByDeletingLastPathComponent" => url_by_deleting_last_path_component, c"@16@0:8";
        "URLByStandardizingPath" => url_by_standardizing_path, c"@16@0:8";
        "lastPathComponent" => last_path_component, c"@16@0:8";
        "resourceValuesForKeys:error:" => resource_values_for_keys, c"@32@0:8@16^@24";
        "getResourceValue:forKey:error:" => get_resource_value, c"B40@0:8^@16@24^@32";
        "setResourceValue:forKey:error:" => set_resource_value, c"B40@0:8@16@24^@32";
    });
}

#[cfg(test)]
mod tests {
    use super::standardized_file_path;

    #[test]
    fn standardizes_file_path_lexically() {
        assert_eq!(standardized_file_path("/a//b/../c/."), "/a/c");
        assert_eq!(standardized_file_path("/../../a"), "/a");
        assert_eq!(standardized_file_path("a/../../b"), "../b");
        assert_eq!(standardized_file_path("/"), "/");
    }
}
