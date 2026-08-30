//! `NSSearchPathForDirectoriesInDomains` and the two directory functions beside
//! it, which is how a program finds where to write.
//!
//! Answered from `$HOME` rather than from the host's own Foundation: under
//! `ROSETTA3_OBJC=builtin` the array and its strings have to be objects of this
//! runtime, and Apple's would be a foreign structure read as a native one.

use objc::object::Id;

/// The directory a search-path constant names, relative to a domain's root.
///
/// `None` for a constant this tree cannot place. Named rather than guessed at:
/// a wrong directory here is a save file written somewhere the guest will not
/// look for it again.
fn relative(directory: u64) -> Option<&'static str> {
    Some(match directory {
        1 => "Applications",
        3 => "Developer/Applications",
        4 => "Applications/Utilities",
        5 => "Library",
        6 => "Developer",
        8 => "Library/Documentation",
        9 => "Documents",
        11 => "Library/Autosave Information",
        12 => "Desktop",
        13 => "Library/Caches",
        14 => "Library/Application Support",
        15 => "Downloads",
        16 => "Library/Input Methods",
        17 => "Movies",
        18 => "Music",
        19 => "Pictures",
        20 => "Library/PrinterDescriptions",
        22 => "Library/PreferencePanes",
        _ => return None,
    })
}

/// The root of each domain in `mask`, in the order Apple returns them.
///
/// `expand` false keeps the user domain's tilde, which is the whole point of
/// that argument: a caller passing false means to store the path, not open it.
fn roots(mask: u64, expand: bool) -> Vec<String> {
    let mut out = Vec::new();
    if mask & 1 != 0 {
        out.push(match expand {
            true => crate::home_directory(),
            false => "~".to_string(),
        });
    }
    if mask & 2 != 0 {
        out.push(String::new());
    }
    if mask & 4 != 0 {
        out.push("/Network".to_string());
    }
    if mask & 8 != 0 {
        out.push("/System".to_string());
    }
    out
}

/// `NSArray *NSSearchPathForDirectoriesInDomains(NSSearchPathDirectory,
/// NSSearchPathDomainMask, BOOL)`.
///
/// A directory this tree cannot place gives an empty array, which is what Apple
/// gives for one that does not exist in the requested domain - a caller already
/// has to handle it, and it leaves the program to say what it wanted rather than
/// sending it to the wrong place.
unsafe extern "C" fn search_path(directory: u64, mask: u64, expand: u8) -> Id {
    let paths: Vec<String> = match relative(directory) {
        Some(relative) => roots(mask, expand != 0)
            .into_iter()
            .map(|root| format!("{root}/{relative}"))
            .collect(),
        None => Vec::new(),
    };
    let objects: Vec<corefoundation::base::CFTypeRef> = paths
        .iter()
        .map(|p| corefoundation::string::from_bytes(p.as_bytes(), false))
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

unsafe extern "C" fn home_directory() -> Id {
    crate::string::from_str(&crate::home_directory())
}

unsafe extern "C" fn temporary_directory() -> Id {
    // Apple's ends in a separator, and a caller that concatenates rather than
    // using `-stringByAppendingPathComponent:` depends on it.
    let path = std::env::temp_dir();
    let path = path.to_string_lossy();
    crate::string::from_str(&match path.ends_with('/') {
        true => path.into_owned(),
        false => format!("{path}/"),
    })
}

/// One entry point, as an address.
macro_rules! at {
    ($f:expr) => {
        $f as *const () as usize as u64
    };
}

pub fn symbol(name: &str) -> Option<u64> {
    Some(match name {
        "NSSearchPathForDirectoriesInDomains" => at!(search_path),
        "NSHomeDirectory" => at!(home_directory),
        "NSTemporaryDirectory" => at!(temporary_directory),
        _ => return None,
    })
}

/// Every name [`symbol`] answers.
pub const NAMES: &[&str] = &[
    "NSSearchPathForDirectoriesInDomains",
    "NSHomeDirectory",
    "NSTemporaryDirectory",
];
