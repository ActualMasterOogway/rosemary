//! `NSTimeZone`'s two class methods, over the host's zoneinfo database.
//!
//! The object itself is `corefoundation`'s, because `CFTimeZoneCopySystem` is
//! answered during startup and is underneath this crate. What is here is the
//! Objective-C spelling a guest reaches for: `+systemTimeZone`, and the list of
//! names `+timeZoneWithName:` accepts, which is the *host's* list rather than a
//! table baked in here - a guest that offers the user a zone and then sets `TZ`
//! from it must offer names the host can actually resolve.

use objc::object::Id;
use objc::sel::Sel;

use crate::define::class_methods;

/// The directories a zoneinfo database is found in, in the order `tzset` itself
/// looks: `TZDIR` overrides, and the rest are where the distributions put it -
/// NixOS uses `/etc/zoneinfo`, most others `/usr/share/zoneinfo`.
fn database() -> Option<std::path::PathBuf> {
    std::env::var("TZDIR")
        .ok()
        .filter(|dir| !dir.is_empty())
        .map(std::path::PathBuf::from)
        .into_iter()
        .chain(
            ["/usr/share/zoneinfo", "/etc/zoneinfo", "/usr/lib/zoneinfo"]
                .into_iter()
                .map(std::path::PathBuf::from),
        )
        .find(|dir| dir.is_dir())
}

/// Whether a path relative to the database root names a zone.
///
/// A zone is `Area/Location`, which is what having a directory component says.
/// The database also holds the compiled-rule subtrees, the `.tab` and `.zi`
/// sources and a handful of generated files, none of which are zones a guest
/// should show a user; `UTC` and `GMT` are the two zones that have no area, and
/// Apple lists both.
fn is_zone(name: &str) -> bool {
    if name.contains('.') || name.starts_with("posix/") || name.starts_with("right/") {
        return false;
    }
    match name.split('/').next() {
        Some("Factory") | Some("localtime") | Some("posixrules") => false,
        _ => name.contains('/') || matches!(name, "UTC" | "GMT"),
    }
}

/// Every zone the host's database names, sorted, or empty when it has none.
///
/// Empty rather than a guessed list: an empty array says "this host publishes no
/// zones", which a guest can present, where invented names would be offered to
/// the user and then fail to resolve.
fn known_names() -> Vec<String> {
    let Some(root) = database() else {
        return Vec::new();
    };
    let mut names = Vec::new();
    let mut pending = vec![root.clone()];
    while let Some(directory) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(relative) = path.strip_prefix(&root) else {
                continue;
            };
            let Some(name) = relative.to_str().map(str::to_owned) else {
                continue;
            };
            // `is_dir` follows the link, which is what is wanted: a linked zone
            // - `US/Eastern` - is a name the database resolves, and a linked
            // *directory* is one to descend into.
            match path.is_dir() {
                true if !name.starts_with("posix") && !name.starts_with("right") => {
                    pending.push(path)
                }
                true => {}
                false if is_zone(&name) => names.push(name),
                false => {}
            }
        }
    }
    names.sort();
    names.dedup();
    names
}

/// `+[NSTimeZone systemTimeZone]`, autoreleased as a `+` method's result is.
unsafe extern "C" fn system_time_zone(_class: Id, _sel: Sel) -> Id {
    // SAFETY: takes no argument and returns a fresh timezone.
    let zone = unsafe { corefoundation::timezone::copy_system() };
    // SAFETY: the reference `copy_system` handed over.
    unsafe { corefoundation::api::CFAutorelease(zone) as Id }
}

unsafe extern "C" fn known_time_zone_names(_class: Id, _sel: Sel) -> Id {
    let names = known_names()
        .iter()
        .map(|name| crate::string::from_str(name) as *const std::ffi::c_void)
        .collect::<Vec<_>>();
    let array = corefoundation::collections::new_array(
        &names,
        corefoundation::callbacks::Policy::from_type_callbacks(),
        false,
    );
    for name in names {
        // The array retained each string and now owns it.
        // SAFETY: each is the live object made immediately above.
        unsafe { corefoundation::api::CFRelease(name) };
    }
    // SAFETY: the fresh array this call owns.
    unsafe { corefoundation::api::CFAutorelease(array) as Id }
}

pub(crate) fn install() {
    let class = corefoundation::base::abstract_class(c"NSTimeZone", None);
    class_methods!(class, {
        "systemTimeZone" => system_time_zone, c"@16@0:8";
        "localTimeZone" => system_time_zone, c"@16@0:8";
        "defaultTimeZone" => system_time_zone, c"@16@0:8";
        "knownTimeZoneNames" => known_time_zone_names, c"@16@0:8";
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The structural rule, which is what keeps `zone.tab` and the compiled
    /// `posix/` tree out of a list a guest shows a user.
    #[test]
    fn a_zone_is_an_area_and_a_location() {
        assert!(is_zone("America/New_York"));
        assert!(is_zone("America/Argentina/Buenos_Aires"));
        assert!(is_zone("UTC"));
        assert!(!is_zone("zone.tab"));
        assert!(!is_zone("tzdata.zi"));
        assert!(!is_zone("leapseconds"));
        assert!(!is_zone("posix/America/New_York"));
        assert!(!is_zone("right/UTC"));
        assert!(!is_zone("Factory"));
    }

    /// Whatever the host publishes, every name in the answer is one
    /// `+timeZoneWithName:` resolves - which is the only promise the list makes.
    #[test]
    fn every_published_name_is_a_zone_the_host_has() {
        let Some(root) = database() else {
            return;
        };
        let names = known_names();
        assert!(names.windows(2).all(|pair| pair[0] < pair[1]), "sorted");
        for name in &names {
            assert!(root.join(name).exists(), "{name} is not in the database");
        }
        if names.iter().any(|name| name.starts_with("America/")) {
            assert!(
                names.contains(&"America/New_York".to_string()),
                "a database with an America area has its best-known zone"
            );
        }
    }

    /// The system zone is an object, and it is the one the CoreFoundation
    /// spelling answers with - not a second, unrelated timezone class.
    #[test]
    fn the_system_zone_is_a_timezone() {
        crate::bootstrap();
        // SAFETY: `+systemTimeZone` is `id (Class, SEL)`.
        let zone =
            unsafe { system_time_zone(std::ptr::null_mut(), objc::sel::get("systemTimeZone")) };
        assert!(!zone.is_null());
        let class = objc::class::lookup(c"NSTimeZone").expect("NSTimeZone");
        // SAFETY: the object made just above.
        assert!(
            unsafe { objc::object::class_of(zone) }
                .is_some_and(|c| c.chain().any(|x| std::ptr::eq(x, class))),
            "the system zone is an NSTimeZone"
        );
    }
}
