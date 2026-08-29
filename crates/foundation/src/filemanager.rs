//! `NSFileManager`, over `std::fs`.
//!
//! Everything here answers about the *host's* filesystem, because that is the
//! one the guest's files are on: this runtime maps a guest's paths straight
//! through, so `-fileExistsAtPath:` asks the same question the guest's own
//! `stat` would.
//!
//! The error out-parameters are written as nil rather than as an `NSError`.
//! There is no `NSError` in this tree, and a method that reported failure
//! *without* filling one in would be indistinguishable from one that succeeded
//! to a caller checking the error rather than the return value - so the return
//! value is always the truth, and a caller that dereferences the error on
//! failure is refused by name at the send rather than handed something invented.

use objc::object::Id;
use objc::sel::Sel;

use crate::define::{class_methods, methods};

/// The one manager, which `+defaultManager` must return every time.
static DEFAULT: std::sync::OnceLock<usize> = std::sync::OnceLock::new();

unsafe extern "C" fn default_manager(class: Id, _sel: Sel) -> Id {
    *DEFAULT.get_or_init(|| {
        // SAFETY: `class` is `NSFileManager`, which this crate registered.
        let manager = unsafe {
            objc::object::create_instance(
                objc::class::from_ptr(class as *const objc::class::Class).expect("a class"),
                0,
            )
        };
        // Retained forever: a singleton a guest can release is one it can make
        // dangle by releasing what it never owned.
        objc::arc::retain(manager);
        manager as usize
    }) as Id
}

/// A path argument as a `String`, or `None` for one that is not readable text.
///
/// # Safety
///
/// `path` must be null or an `NSString`.
unsafe fn path_of(path: Id) -> Option<String> {
    // SAFETY: the caller's obligation.
    String::from_utf8(unsafe { crate::string::bytes(path) }).ok()
}

/// Write nil through an `NSError **`, which is what "no error" looks like.
///
/// # Safety
///
/// `slot` must be null or writable.
unsafe fn no_error(slot: *mut Id) {
    if !slot.is_null() {
        // SAFETY: the caller's obligation.
        unsafe { *slot = std::ptr::null_mut() };
    }
}

unsafe extern "C" fn file_exists(_receiver: Id, _sel: Sel, path: Id) -> u8 {
    // SAFETY: an `NSString`, or nil.
    unsafe { path_of(path) }.is_some_and(|p| std::fs::metadata(p).is_ok()) as u8
}

unsafe extern "C" fn file_exists_is_directory(
    _receiver: Id,
    _sel: Sel,
    path: Id,
    is_directory: *mut u8,
) -> u8 {
    // SAFETY: an `NSString`, or nil.
    let Some(path) = (unsafe { path_of(path) }) else {
        return 0;
    };
    let Ok(metadata) = std::fs::metadata(&path) else {
        return 0;
    };
    if !is_directory.is_null() {
        // SAFETY: the caller passed a writable `BOOL *`, as the method declares.
        unsafe { *is_directory = metadata.is_dir() as u8 };
    }
    1
}

unsafe extern "C" fn create_directory(
    _receiver: Id,
    _sel: Sel,
    path: Id,
    intermediates: u8,
    _attributes: Id,
    error: *mut Id,
) -> u8 {
    // SAFETY: null or writable.
    unsafe { no_error(error) };
    // SAFETY: an `NSString`, or nil.
    let Some(path) = (unsafe { path_of(path) }) else {
        return 0;
    };
    let made = match intermediates != 0 {
        true => std::fs::create_dir_all(&path),
        false => std::fs::create_dir(&path),
    };
    // Apple's succeeds when the directory is already there and intermediates
    // were asked for, which `create_dir_all` also does - so the two agree
    // without a check here.
    made.is_ok() as u8
}

unsafe extern "C" fn remove_item(_receiver: Id, _sel: Sel, path: Id, error: *mut Id) -> u8 {
    // SAFETY: null or writable.
    unsafe { no_error(error) };
    // SAFETY: an `NSString`, or nil.
    let Some(path) = (unsafe { path_of(path) }) else {
        return 0;
    };
    let Ok(metadata) = std::fs::symlink_metadata(&path) else {
        return 0;
    };
    let removed = match metadata.is_dir() {
        true => std::fs::remove_dir_all(&path),
        false => std::fs::remove_file(&path),
    };
    removed.is_ok() as u8
}

unsafe extern "C" fn copy_item(
    _receiver: Id,
    _sel: Sel,
    from: Id,
    to: Id,
    error: *mut Id,
) -> u8 {
    // SAFETY: null or writable.
    unsafe { no_error(error) };
    // SAFETY: two `NSString`s, or nil.
    let (Some(from), Some(to)) = (unsafe { path_of(from) }, unsafe { path_of(to) }) else {
        return 0;
    };
    // Files only. Copying a directory tree is a walk with its own failure modes,
    // and answering "yes" to one this did not copy would leave the guest
    // reading files that are not there.
    match std::fs::symlink_metadata(&from) {
        Ok(metadata) if metadata.is_dir() => objc::fail!(
            "-[NSFileManager copyItemAtPath:toPath:error:]: this runtime copies files, not \
             directory trees"
        ),
        Ok(_) => std::fs::copy(&from, &to).is_ok() as u8,
        Err(_) => 0,
    }
}

unsafe extern "C" fn move_item(_receiver: Id, _sel: Sel, from: Id, to: Id, error: *mut Id) -> u8 {
    // SAFETY: null or writable.
    unsafe { no_error(error) };
    // SAFETY: two `NSString`s, or nil.
    let (Some(from), Some(to)) = (unsafe { path_of(from) }, unsafe { path_of(to) }) else {
        return 0;
    };
    std::fs::rename(&from, &to).is_ok() as u8
}

unsafe extern "C" fn contents_of_directory(
    _receiver: Id,
    _sel: Sel,
    path: Id,
    error: *mut Id,
) -> Id {
    // SAFETY: null or writable.
    unsafe { no_error(error) };
    // SAFETY: an `NSString`, or nil.
    let Some(path) = (unsafe { path_of(path) }) else {
        return std::ptr::null_mut();
    };
    let Ok(entries) = std::fs::read_dir(&path) else {
        return std::ptr::null_mut();
    };
    // Names, not paths, and without `.` and `..` - which is what Apple's
    // shallow enumeration gives and what a caller joining them back onto the
    // directory expects.
    let mut names: Vec<String> = entries
        .filter_map(|e| Some(e.ok()?.file_name().to_string_lossy().into_owned()))
        .collect();
    names.sort();
    let objects: Vec<corefoundation::base::CFTypeRef> = names
        .iter()
        .map(|n| corefoundation::string::from_bytes(n.as_bytes(), false))
        .collect();
    let array = corefoundation::collections::new_array(
        &objects,
        corefoundation::callbacks::Policy::from_type_callbacks(),
        false,
    );
    for object in objects {
        // SAFETY: a string this function made and the array has retained.
        unsafe { corefoundation::api::CFRelease(object) };
    }
    // SAFETY: a fresh array.
    unsafe { corefoundation::api::CFAutorelease(array) as Id }
}

/// `-attributesOfItemAtPath:error:`, with the two keys a program actually reads.
///
/// `NSFileSize` and `NSFileType`. A dictionary with only some of an item's
/// attributes is the shape this tree refused `-[NSBundle infoDictionary]` over,
/// and the reason it is allowed here is that these are *this runtime's* answers
/// to a fixed question rather than a file's own contents: a key that is absent
/// is one nothing in this tree can answer, and adding one later cannot change
/// what the present ones mean.
unsafe extern "C" fn attributes_of_item(
    _receiver: Id,
    _sel: Sel,
    path: Id,
    error: *mut Id,
) -> Id {
    // SAFETY: null or writable.
    unsafe { no_error(error) };
    // SAFETY: an `NSString`, or nil.
    let Some(path) = (unsafe { path_of(path) }) else {
        return std::ptr::null_mut();
    };
    let Ok(metadata) = std::fs::metadata(&path) else {
        return std::ptr::null_mut();
    };
    let kind = match metadata.is_dir() {
        true => "NSFileTypeDirectory",
        false => "NSFileTypeRegular",
    };
    let pairs = [
        (
            corefoundation::string::from_bytes(b"NSFileSize", false),
            corefoundation::collections::new_number(corefoundation::collections::Number::Integer(
                metadata.len() as i64,
            )),
        ),
        (
            corefoundation::string::from_bytes(b"NSFileType", false),
            corefoundation::string::from_bytes(kind.as_bytes(), false),
        ),
    ];
    let dictionary = corefoundation::collections::new_dictionary(
        &pairs,
        corefoundation::callbacks::Policy::from_key_callbacks(),
        corefoundation::callbacks::Policy::from_type_callbacks(),
        false,
    );
    for (key, value) in pairs {
        // SAFETY: objects this function made and the dictionary has retained.
        unsafe {
            corefoundation::api::CFRelease(key);
            corefoundation::api::CFRelease(value);
        }
    }
    // SAFETY: a fresh dictionary.
    unsafe { corefoundation::api::CFAutorelease(dictionary) as Id }
}

unsafe extern "C" fn is_writable(_receiver: Id, _sel: Sel, path: Id) -> u8 {
    // SAFETY: an `NSString`, or nil.
    let Some(path) = (unsafe { path_of(path) }) else {
        return 0;
    };
    std::fs::metadata(&path).is_ok_and(|m| !m.permissions().readonly()) as u8
}

unsafe extern "C" fn current_directory(_receiver: Id, _sel: Sel) -> Id {
    let path = std::env::current_dir().unwrap_or_default();
    crate::string::from_str(&path.to_string_lossy())
}

pub(crate) fn install() {
    let class = corefoundation::base::register(
        c"NSFileManager",
        None,
        std::mem::size_of::<usize>(),
        false,
    );
    class_methods!(class, {
        "defaultManager" => default_manager, c"@16@0:8";
    });
    methods!(class, {
        "fileExistsAtPath:" => file_exists, c"B24@0:8@16";
        "fileExistsAtPath:isDirectory:" => file_exists_is_directory, c"B32@0:8@16^c24";
        "createDirectoryAtPath:withIntermediateDirectories:attributes:error:"
            => create_directory, c"B44@0:8@16c24@32^@40";
        "removeItemAtPath:error:" => remove_item, c"B32@0:8@16^@24";
        "copyItemAtPath:toPath:error:" => copy_item, c"B40@0:8@16@24^@32";
        "moveItemAtPath:toPath:error:" => move_item, c"B40@0:8@16@24^@32";
        "contentsOfDirectoryAtPath:error:" => contents_of_directory, c"@32@0:8@16^@24";
        "attributesOfItemAtPath:error:" => attributes_of_item, c"@32@0:8@16^@24";
        "isWritableFileAtPath:" => is_writable, c"B24@0:8@16";
        "isReadableFileAtPath:" => file_exists, c"B24@0:8@16";
        "currentDirectoryPath" => current_directory, c"@16@0:8";
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manager() -> Id {
        crate::bootstrap();
        let class = objc::class::lookup(c"NSFileManager").expect("NSFileManager");
        // SAFETY: `+defaultManager` is `id (Class, SEL)`.
        unsafe { default_manager(objc::class::ptr(class) as Id, objc::sel::get("defaultManager")) }
    }

    /// The three the runner uses, on a directory this test makes and removes.
    #[test]
    fn a_directory_is_created_seen_and_removed() {
        let manager = manager();
        let root = std::env::temp_dir().join(format!("r3-fm-{}", std::process::id()));
        let nested = root.join("a/b");
        let path = crate::string::from_str(&nested.to_string_lossy());

        // SAFETY: each prototype is the one its selector declares.
        unsafe {
            assert_eq!(
                create_directory(
                    manager,
                    objc::sel::get("createDirectoryAtPath:"),
                    path,
                    1,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                ),
                1,
                "intermediates were asked for"
            );
            assert_eq!(file_exists(manager, objc::sel::get("fileExistsAtPath:"), path), 1);

            let mut is_directory = 0u8;
            assert_eq!(
                file_exists_is_directory(
                    manager,
                    objc::sel::get("fileExistsAtPath:isDirectory:"),
                    path,
                    &mut is_directory,
                ),
                1
            );
            assert_eq!(is_directory, 1);

            let root_path = crate::string::from_str(&root.to_string_lossy());
            assert_eq!(
                remove_item(
                    manager,
                    objc::sel::get("removeItemAtPath:error:"),
                    root_path,
                    std::ptr::null_mut(),
                ),
                1
            );
            assert_eq!(file_exists(manager, objc::sel::get("fileExistsAtPath:"), path), 0);
        }
    }

    /// One manager, which is what `+defaultManager` promises.
    #[test]
    fn the_default_manager_is_one_object() {
        assert_eq!(manager(), manager());
    }
}
