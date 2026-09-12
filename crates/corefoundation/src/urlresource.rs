//! The `CFURL` resource-property API: what a file is, asked through its URL.
//!
//! `CFURLCopyResourcePropertyForKey` is how a Mac program asks the questions a
//! `stat` does not answer - is this hidden, is this directory really a document,
//! is this file an alias - and Qt asks three of them for *every* file it stats,
//! from `QFileSystemEngine::fillMetaData`. So these are not a file-dialog nicety:
//! an unbound key here binds to zero and Qt passes address zero as the key while
//! listing a directory during startup.
//!
//! Where a question has no answer on this host, the property is **absent** rather
//! than false, and the two are different: `CFURLCopyResourcePropertyForKey`
//! returning true with a false value says "this file is definitely not hidden",
//! and returning false says "this volume cannot tell you". A caller that asked
//! about a flag no filesystem here has is entitled to the second.
//!
//! The keys are spelled with their `NSURL` values, because on a Mac they *are*
//! the `NSURL` constants - `kCFURLIsHiddenKey` and `NSURLIsHiddenKey` are one
//! symbol with one string behind it, so a guest that reads a property dictionary
//! with the CoreFoundation key and looks it up with the Foundation one has to
//! find it.

// The C spellings are the interface.
#![allow(non_snake_case)]

use crate::base::{CFIndex, CFTypeRef};

/// `Boolean`, which is a byte.
type Boolean = u8;

/// The resource-property keys, as `(Mach-O name, value)`.
///
/// Both spellings of each, because a program may import either and they are one
/// constant on a Mac. Every one is a `CFStringRef` **variable**, so each goes
/// through [`crate::base::global_cell`].
pub const KEYS: &[(&str, &str)] = &[
    ("_kCFURLNameKey", NAME),
    ("_NSURLNameKey", NAME),
    ("_kCFURLLocalizedNameKey", LOCALIZED_NAME),
    ("_NSURLLocalizedNameKey", LOCALIZED_NAME),
    ("_kCFURLIsHiddenKey", IS_HIDDEN),
    ("_NSURLIsHiddenKey", IS_HIDDEN),
    ("_kCFURLIsPackageKey", IS_PACKAGE),
    ("_NSURLIsPackageKey", IS_PACKAGE),
    ("_kCFURLIsAliasFileKey", IS_ALIAS_FILE),
    ("_NSURLIsAliasFileKey", IS_ALIAS_FILE),
    ("_kCFURLIsDirectoryKey", IS_DIRECTORY),
    ("_NSURLIsDirectoryKey", IS_DIRECTORY),
    ("_kCFURLIsRegularFileKey", IS_REGULAR_FILE),
    ("_NSURLIsRegularFileKey", IS_REGULAR_FILE),
    ("_kCFURLIsSymbolicLinkKey", IS_SYMBOLIC_LINK),
    ("_NSURLIsSymbolicLinkKey", IS_SYMBOLIC_LINK),
    ("_kCFURLFileSizeKey", FILE_SIZE),
    ("_NSURLFileSizeKey", FILE_SIZE),
    ("_kCFURLVolumeURLKey", VOLUME_URL),
    ("_NSURLVolumeURLKey", VOLUME_URL),
    ("_kCFURLVolumeTotalCapacityKey", VOLUME_TOTAL_CAPACITY),
    ("_NSURLVolumeTotalCapacityKey", VOLUME_TOTAL_CAPACITY),
    (
        "_kCFURLVolumeAvailableCapacityKey",
        VOLUME_AVAILABLE_CAPACITY,
    ),
    (
        "_NSURLVolumeAvailableCapacityKey",
        VOLUME_AVAILABLE_CAPACITY,
    ),
    ("_kCFURLVolumeIdentifierKey", VOLUME_IDENTIFIER),
    ("_NSURLVolumeIdentifierKey", VOLUME_IDENTIFIER),
];

const NAME: &str = "NSURLNameKey";
const LOCALIZED_NAME: &str = "NSURLLocalizedNameKey";
const IS_HIDDEN: &str = "NSURLIsHiddenKey";
const IS_PACKAGE: &str = "NSURLIsPackageKey";
const IS_ALIAS_FILE: &str = "NSURLIsAliasFileKey";
const IS_DIRECTORY: &str = "NSURLIsDirectoryKey";
const IS_REGULAR_FILE: &str = "NSURLIsRegularFileKey";
const IS_SYMBOLIC_LINK: &str = "NSURLIsSymbolicLinkKey";
const FILE_SIZE: &str = "NSURLFileSizeKey";
const VOLUME_URL: &str = "NSURLVolumeURLKey";
const VOLUME_TOTAL_CAPACITY: &str = "NSURLVolumeTotalCapacityKey";
const VOLUME_AVAILABLE_CAPACITY: &str = "NSURLVolumeAvailableCapacityKey";
const VOLUME_IDENTIFIER: &str = "NSURLVolumeIdentifierKey";

/// The extensions that make a *directory* a package - a bundle the user sees as
/// one file rather than as a folder.
///
/// A list rather than a type query, because the type query is Launch Services
/// asking whether a UTI conforms to `com.apple.package`, and there is none here.
/// These are the extensions that conform to it in practice, and the ones a file
/// dialog has to get right: an `.app` shown as a folder is a file dialog the user
/// cannot open an application from.
const PACKAGE_EXTENSIONS: &[&str] = &[
    "app",
    "bundle",
    "framework",
    "plugin",
    "kext",
    "pkg",
    "mpkg",
    "docset",
    "xcodeproj",
    "xcworkspace",
    "playground",
    "rtfd",
    "scptd",
    "download",
    "photoslibrary",
    "fcpbundle",
    "logicx",
    "band",
];

/// Whether `path` names a package: a directory whose extension says the user
/// should see it as one file.
fn is_package(path: &std::path::Path) -> bool {
    if !path.is_dir() {
        return false;
    }
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| PACKAGE_EXTENSIONS.iter().any(|p| e.eq_ignore_ascii_case(p)))
}

/// Whether `path` is hidden.
///
/// A leading dot, which is what "hidden" means on every filesystem this host has.
/// Apple also honours a per-file Finder flag in the volume's metadata; ext4 has
/// nowhere to keep one, so a file hidden *only* that way on a Mac is not hidden
/// here - and there is no way for it to be.
fn is_hidden(path: &std::path::Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with('.') && n != "." && n != "..")
}

/// The mount point `path` is on, found by walking up until the device changes.
///
/// The device number rather than a mount table, because that is the question:
/// the volume is the outermost directory still on the same filesystem, and
/// `st_dev` is what says so on either host.
fn volume_of(path: &std::path::Path) -> std::path::PathBuf {
    let Some(device) = device_of(path) else {
        return std::path::PathBuf::from("/");
    };
    let mut at = path.to_path_buf();
    while let Some(parent) = at.parent() {
        if device_of(parent) != Some(device) {
            break;
        }
        at = parent.to_path_buf();
    }
    at
}

fn device_of(path: &std::path::Path) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path).ok().map(|m| m.dev())
}

/// The total and available bytes of the filesystem holding `path`.
fn capacity(path: &std::path::Path) -> Option<(u64, u64)> {
    let text = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).ok()?;
    // SAFETY: a NUL-terminated path and a `statvfs` for the call to fill in.
    let stat = unsafe {
        let mut stat: libc::statvfs = std::mem::zeroed();
        (libc::statvfs(text.as_ptr(), &mut stat) == 0).then_some(stat)?
    };
    let unit = match stat.f_frsize {
        0 => stat.f_bsize,
        frsize => frsize,
    } as u64;
    // `f_bavail` rather than `f_bfree`: the blocks *this user* may fill, which is
    // what a program checking whether a save will fit needs.
    Some((stat.f_blocks as u64 * unit, stat.f_bavail as u64 * unit))
}

/// The value of one property of `path`, or `None` for a key this host cannot
/// answer - which is not the same as a false answer. See the module note.
///
/// The returned object is owned by the caller.
fn property(path: &std::path::Path, key: &str) -> Option<CFTypeRef> {
    let number = |value: u64| {
        crate::collections::new_number(crate::collections::Number::Integer(value as i64))
    };
    // `symlink_metadata`, because every question here is about the name the URL
    // holds rather than about what a symlink points at.
    let metadata = || std::fs::symlink_metadata(path).ok();
    Some(match key {
        NAME | LOCALIZED_NAME => {
            // No localised display name: a `.localized` folder's strings are
            // Finder data this tree does not read, so the name is the name.
            let name = path.file_name()?.to_str()?;
            crate::string::from_bytes(name.as_bytes(), false)
        }
        IS_HIDDEN => crate::collections::boolean(is_hidden(path)),
        IS_PACKAGE => crate::collections::boolean(is_package(path)),
        // A Finder alias is a file whose *contents* are a bookmark, and nothing
        // on this host writes one - so this is false rather than absent: the
        // answer is known, and it is no.
        IS_ALIAS_FILE => crate::collections::boolean(false),
        IS_DIRECTORY => crate::collections::boolean(metadata()?.is_dir()),
        IS_REGULAR_FILE => crate::collections::boolean(metadata()?.is_file()),
        IS_SYMBOLIC_LINK => crate::collections::boolean(metadata()?.is_symlink()),
        FILE_SIZE => number(metadata()?.len()),
        VOLUME_URL => crate::url::new_file_url(&volume_of(path).to_string_lossy(), true),
        VOLUME_TOTAL_CAPACITY => number(capacity(path)?.0),
        VOLUME_AVAILABLE_CAPACITY => number(capacity(path)?.1),
        // The volume's device number, which is exactly what the key is for:
        // telling whether two URLs are on the same volume.
        VOLUME_IDENTIFIER => number(device_of(path)?),
        _ => return None,
    })
}

/// The path a URL names, for a caller that has the URL.
///
/// # Safety
///
/// `url` must be null or an object this runtime allocated.
unsafe fn path_of(url: CFTypeRef) -> Option<std::path::PathBuf> {
    // SAFETY: the caller's obligation.
    let path = unsafe { crate::url::as_url(url) }?.file_path()?;
    Some(std::path::PathBuf::from(path))
}

/// The text of a `CFStringRef`.
///
/// # Safety
///
/// As [`path_of`].
unsafe fn text_of(value: CFTypeRef) -> Option<String> {
    // SAFETY: the caller's obligation.
    let bytes = unsafe { crate::string::as_string(value) }?.utf8();
    String::from_utf8(bytes).ok()
}

/// `Boolean CFURLCopyResourcePropertyForKey(CFURLRef, CFStringRef key, void
/// *propertyValueTypeRefPtr, CFErrorRef *error)`
///
/// True with the value written through `out` when the property is known, false
/// with `*out` left nil when it is not. `error` is left alone: a property this
/// host cannot answer is not a failure to report, and a caller that passed a
/// pointer for one is entitled to find no error in it.
///
/// # Safety
///
/// `url` and `key` are null or objects this runtime allocated; `out` is null or
/// writable for one pointer, and `error` is null or writable for one pointer.
pub unsafe extern "C" fn CFURLCopyResourcePropertyForKey(
    url: CFTypeRef,
    key: CFTypeRef,
    out: *mut CFTypeRef,
    _error: *mut CFTypeRef,
) -> Boolean {
    if !out.is_null() {
        // Cleared first, so a caller that reads it after a false return finds
        // nil rather than whatever was on its stack.
        // SAFETY: the caller states it is writable.
        unsafe { *out = std::ptr::null() };
    }
    // SAFETY: the caller's obligation.
    let (Some(path), Some(key)) = (unsafe { path_of(url) }, unsafe { text_of(key) }) else {
        return 0;
    };
    let Some(value) = property(&path, &key) else {
        return 0;
    };
    match out.is_null() {
        // A caller asking only whether the property exists: the value it would
        // have been given has to be released rather than leaked.
        // SAFETY: an object this function owns the only reference to.
        true => unsafe { crate::api::CFRelease(value) },
        // SAFETY: the caller states it is writable, and takes the reference.
        false => unsafe { *out = value },
    }
    1
}

/// `CFDictionaryRef CFURLCopyResourcePropertiesForKeys(CFURLRef, CFArrayRef
/// keys, CFErrorRef *error)`
///
/// A key with no answer is left *out* of the dictionary rather than entered as
/// nil, which is Apple's behaviour and the one a caller can act on: a dictionary
/// that holds the key says the filesystem answered.
///
/// # Safety
///
/// As [`CFURLCopyResourcePropertyForKey`], with `keys` a `CFArray` or null.
pub unsafe extern "C" fn CFURLCopyResourcePropertiesForKeys(
    url: CFTypeRef,
    keys: CFTypeRef,
    _error: *mut CFTypeRef,
) -> CFTypeRef {
    // SAFETY: the caller's obligation.
    let Some(path) = (unsafe { path_of(url) }) else {
        return std::ptr::null();
    };
    // SAFETY: as above - a `CFArray` or null.
    let Some(keys) = (unsafe { crate::collections::array(keys) }) else {
        return std::ptr::null();
    };
    let mut pairs: Vec<(CFTypeRef, CFTypeRef)> = Vec::new();
    for key in keys.values.clone() {
        // SAFETY: a value from the caller's array.
        let Some(name) = (unsafe { text_of(key) }) else {
            continue;
        };
        if let Some(value) = property(&path, &name) {
            pairs.push((key, value));
        }
    }
    let dictionary = crate::collections::new_dictionary(
        &pairs,
        crate::callbacks::Policy::from_key_callbacks(),
        crate::callbacks::Policy::from_type_callbacks(),
        false,
    );
    for (_, value) in pairs {
        // The dictionary retained each value; these are this call's references.
        // SAFETY: a live object `property` handed over.
        unsafe { crate::api::CFRelease(value) };
    }
    dictionary
}

/// `CFURLRef CFURLCreateFromFileSystemRepresentation(CFAllocatorRef, const UInt8
/// *buffer, CFIndex length, Boolean isDirectory)`
///
/// # Safety
///
/// `buffer` points to at least `length` readable bytes.
pub unsafe extern "C" fn CFURLCreateFromFileSystemRepresentation(
    _allocator: CFTypeRef,
    buffer: *const u8,
    length: CFIndex,
    is_directory: Boolean,
) -> CFTypeRef {
    if buffer.is_null() || length < 0 {
        return std::ptr::null();
    }
    // SAFETY: the caller states the bytes are readable.
    let bytes = unsafe { std::slice::from_raw_parts(buffer, length as usize) };
    // A path is bytes, not text: anything not UTF-8 is kept as it is rather than
    // being refused, because it is a name that exists on disk.
    let path = String::from_utf8_lossy(bytes);
    crate::url::new_file_url(&path, is_directory != 0)
}

/// `CFURLRef CFURLCreateWithBytes(CFAllocatorRef, const UInt8 *, CFIndex,
/// CFStringEncoding, CFURLRef baseURL)`
///
/// The encoding is ignored for the reason every other text-from-bytes call here
/// ignores it: a URL is ASCII with everything else percent-escaped, so the bytes
/// are already the URL whatever encoding they were declared as.
///
/// # Safety
///
/// As [`CFURLCreateFromFileSystemRepresentation`]; `base` is null or a `CFURL`.
pub unsafe extern "C" fn CFURLCreateWithBytes(
    _allocator: CFTypeRef,
    buffer: *const u8,
    length: CFIndex,
    _encoding: u32,
    base: CFTypeRef,
) -> CFTypeRef {
    if buffer.is_null() || length < 0 {
        return std::ptr::null();
    }
    // SAFETY: the caller states the bytes are readable.
    let bytes = unsafe { std::slice::from_raw_parts(buffer, length as usize) };
    let text = String::from_utf8_lossy(bytes).into_owned();
    // SAFETY: the caller's obligation - a `CFURL` or null.
    let base = unsafe { crate::url::as_url(base) };
    match base {
        // Relative to a base only when it is relative: a URL with its own scheme
        // is absolute and the base says nothing about it.
        Some(base) if !text.contains("://") && !text.starts_with('/') => {
            let mut joined = base.text.clone();
            if !joined.ends_with('/') {
                joined.push('/');
            }
            joined.push_str(&text);
            crate::url::new(&joined)
        }
        _ => crate::url::new(&text),
    }
}

pub const NAMES: &[&str] = &[
    "CFURLCopyResourcePropertyForKey",
    "CFURLCopyResourcePropertiesForKeys",
    "CFURLCreateFromFileSystemRepresentation",
    "CFURLCreateWithBytes",
];

pub fn symbol(name: &str) -> Option<u64> {
    let f: *const () = match name {
        "CFURLCopyResourcePropertyForKey" => CFURLCopyResourcePropertyForKey as *const (),
        "CFURLCopyResourcePropertiesForKeys" => CFURLCopyResourcePropertiesForKeys as *const (),
        "CFURLCreateFromFileSystemRepresentation" => {
            CFURLCreateFromFileSystemRepresentation as *const ()
        }
        "CFURLCreateWithBytes" => CFURLCreateWithBytes as *const (),
        _ => return None,
    };
    Some(f as usize as u64)
}

/// The data symbols this module publishes, for [`crate::data_symbols`].
pub fn data_symbols() -> Vec<(String, u64)> {
    crate::bootstrap();
    KEYS.iter()
        .map(|(symbol, text)| {
            (
                (*symbol).to_string(),
                crate::base::global_cell(symbol, interned(text) as usize as u64),
            )
        })
        .collect()
}

/// One interned string per key, so that the `kCF` and `NS` spellings of a key are
/// the same object - which is what makes a dictionary built with one readable
/// with the other.
fn interned(text: &str) -> CFTypeRef {
    static STRINGS: std::sync::Mutex<Vec<(String, usize)>> = std::sync::Mutex::new(Vec::new());
    let mut strings = STRINGS.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((_, object)) = strings.iter().find(|(t, _)| t == text) {
        return *object as CFTypeRef;
    }
    let object = crate::string::from_bytes(text.as_bytes(), false);
    // Retained forever: a global a guest may release is one it can make dangle.
    objc::arc::retain(object as objc::object::Id);
    strings.push((text.to_string(), object as usize));
    object
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_listed_name_resolves() {
        for name in NAMES {
            assert!(symbol(name).is_some(), "{name}");
        }
    }

    /// Both spellings of a key have to be the same object, or a dictionary built
    /// with the CoreFoundation key cannot be read with the Foundation one.
    #[test]
    fn the_two_spellings_of_a_key_are_one_string() {
        crate::bootstrap();
        let symbols = data_symbols();
        let at = |name: &str| {
            let (_, cell) = symbols.iter().find(|(s, _)| s == name).expect(name);
            // SAFETY: a cell this crate leaked, holding one pointer.
            unsafe { *(*cell as *const u64) }
        };
        assert_eq!(at("_kCFURLIsHiddenKey"), at("_NSURLIsHiddenKey"));
        assert_ne!(at("_kCFURLIsHiddenKey"), at("_kCFURLIsPackageKey"));
    }

    #[test]
    fn a_dotfile_is_hidden_and_an_app_is_a_package() {
        let dir = std::env::temp_dir().join("r3-urlresource");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("Thing.app")).expect("a package to look at");
        std::fs::write(dir.join(".secret"), b"x").expect("a dotfile to look at");
        std::fs::write(dir.join("plain.txt"), b"x").expect("a plain file to look at");

        assert!(is_hidden(&dir.join(".secret")));
        assert!(!is_hidden(&dir.join("plain.txt")));
        assert!(is_package(&dir.join("Thing.app")));
        // A *file* called `.app` is not a package, however it is spelled: the
        // rule is about directories the user sees as one file.
        assert!(!is_package(&dir.join("plain.txt")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The volume of any path under `/` is a directory that exists, and the
    /// capacity of it is not zero - the two answers `QStorageInfo` is made of.
    #[test]
    fn a_path_has_a_volume_with_a_capacity() {
        let volume = volume_of(&std::env::temp_dir());
        assert!(volume.is_dir(), "{volume:?}");
        let (total, available) = capacity(&volume).expect("a mounted filesystem has a size");
        assert!(total > 0, "a volume with no bytes at all");
        assert!(available <= total);
    }
}
