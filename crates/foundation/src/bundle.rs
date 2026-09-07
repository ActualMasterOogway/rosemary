//! `NSBundle`: where the program's own files are.
//!
//! This is the module that retires `rosetta3::bundle`'s re-exec. That hack
//! exists because the *host's* `NSBundle` answers about the running process, so
//! the only way to make it name the guest's `.app` was to become a process
//! inside one. Here `NSBundle` is ours, so it is simply told - see [`set_main`].
//!
//! The layout is recognised by *shape* rather than by name, exactly as
//! `rosetta3::bundle::enclosing` does it: an executable whose parent is `MacOS`,
//! whose grandparent is `Contents`, and whose `Contents/Info.plist` is a file.
//! A directory called `Foo.app` with none of that inside is not a bundle, and a
//! bundle whose directory is called something else still is.

use std::path::{Path, PathBuf};
use std::sync::RwLock;

use crate::plist::{self, Value};

/// A `.app` on disk, and what its `Info.plist` says.
#[derive(Debug)]
pub struct Layout {
    /// `.../DELTARUNE.app`
    pub root: PathBuf,
    /// `.../DELTARUNE.app/Contents`
    pub contents: PathBuf,
    pub resources: PathBuf,
    pub frameworks: PathBuf,
    pub executable: PathBuf,
    pub info: Value,
}

impl Layout {
    /// The bundle `executable` is inside, or `None` for a loose binary.
    pub fn enclosing(executable: &Path) -> Option<Layout> {
        // Absolute, always. Apple's `-bundlePath` is, and a program that joins
        // it onto another directory - which is exactly what a game does to find
        // its data beside its save files - turns a relative one into a path
        // that does not exist. `canonicalize` also resolves the symlinks a
        // `.app` in a library folder is often reached through.
        let absolute = executable.canonicalize().ok();
        let executable = absolute.as_deref().unwrap_or(executable);
        let macos = executable.parent()?;
        if macos.file_name()? != "MacOS" {
            return None;
        }
        let contents = macos.parent()?;
        if contents.file_name()? != "Contents" {
            return None;
        }
        let info_path = contents.join("Info.plist");
        if !info_path.is_file() {
            return None;
        }
        let info = plist::parse(&std::fs::read(&info_path).ok()?).ok()?;
        Some(Layout {
            root: contents.parent()?.to_path_buf(),
            resources: contents.join("Resources"),
            frameworks: contents.join("Frameworks"),
            executable: executable.to_path_buf(),
            contents: contents.to_path_buf(),
            info,
        })
    }

    /// The bundle *at* `root`, which is how `+bundleWithPath:` asks.
    ///
    /// The same shape [`Layout::enclosing`] recognises, entered from the other
    /// end: a directory whose `Contents/Info.plist` is a file. The executable is
    /// named by `CFBundleExecutable` rather than found by looking, because a
    /// `.bundle` with a resource beside its binary in `MacOS` has no other way
    /// to say which one it is.
    pub fn at(root: &Path) -> Option<Layout> {
        let absolute = root.canonicalize().ok();
        let root = absolute.as_deref().unwrap_or(root);
        let contents = root.join("Contents");
        let info_path = contents.join("Info.plist");
        if !info_path.is_file() {
            return None;
        }
        let info = plist::parse(&std::fs::read(&info_path).ok()?).ok()?;
        let executable = info
            .get("CFBundleExecutable")
            .and_then(|name| name.as_str())
            .map(|name| contents.join("MacOS").join(name))
            .unwrap_or_default();
        Some(Layout {
            root: root.to_path_buf(),
            resources: contents.join("Resources"),
            frameworks: contents.join("Frameworks"),
            executable,
            contents,
            info,
        })
    }

    /// An `Info.plist` string, which is how every question below is asked.
    pub fn info_string(&self, key: &str) -> Option<&str> {
        self.info.get(key)?.as_str()
    }

    pub fn identifier(&self) -> Option<&str> {
        self.info_string("CFBundleIdentifier")
    }

    /// The name to show a person, which is not the executable's.
    ///
    /// `CFBundleDisplayName` first, because `CFBundleName` is so often the
    /// executable instead: DELTARUNE's is `Mac_Runner` and its display name is
    /// the game's.
    pub fn display_name(&self) -> Option<&str> {
        self.info_string("CFBundleDisplayName")
            .or_else(|| self.info_string("CFBundleName"))
    }

    /// The version to show a person, which is the marketing one where a bundle
    /// distinguishes it from the build number.
    pub fn version(&self) -> Option<&str> {
        self.info_string("CFBundleShortVersionString")
            .or_else(|| self.info_string("CFBundleVersion"))
    }

    pub fn copyright(&self) -> Option<&str> {
        self.info_string("NSHumanReadableCopyright")
    }

    /// `LSApplicationCategoryType`, as the uniform type identifier the plist
    /// spells it - `public.app-category.action-games` and the rest.
    pub fn category(&self) -> Option<&str> {
        self.info_string("LSApplicationCategoryType")
    }

    /// The development region's `.lproj`, which is where a nib lives.
    fn localizations(&self) -> Vec<PathBuf> {
        let mut out = Vec::new();
        if let Some(region) = self.info_string("CFBundleDevelopmentRegion") {
            out.push(self.resources.join(format!("{region}.lproj")));
        }
        // `Base.lproj` is where a modern Xcode puts an unlocalized nib, and
        // `English.lproj` is where an old one did. Both, in that order, because
        // a bundle may have either and a few have both.
        out.push(self.resources.join("Base.lproj"));
        out.push(self.resources.join("English.lproj"));
        out
    }

    /// Localization identifiers declared by the bundle and present as
    /// `Resources/*.lproj` directories.
    pub fn localization_names(&self) -> Vec<String> {
        let mut names = self
            .info
            .get("CFBundleLocalizations")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect::<Vec<_>>();
        if let Ok(entries) = std::fs::read_dir(&self.resources) {
            names.extend(entries.filter_map(|entry| {
                let entry = entry.ok()?;
                entry.file_type().ok()?.is_dir().then_some(())?;
                let name = entry.file_name();
                let name = name.to_str()?;
                name.strip_suffix(".lproj").map(str::to_owned)
            }));
        }
        names.sort();
        names.dedup();
        names
    }

    /// `-pathForResource:ofType:`, searching `Resources` and then each
    /// localization.
    ///
    /// `extension` may be empty, which means the name already has one - the
    /// spelling `[NSBundle pathForResource:@"MainMenu" ofType:@"nib"]` and
    /// `...ofType:@""` both reach.
    pub fn resource(&self, name: &str, extension: &str) -> Option<PathBuf> {
        let leaf = match extension.is_empty() {
            true => name.to_string(),
            false => format!("{name}.{extension}"),
        };
        std::iter::once(self.resources.clone())
            .chain(self.localizations())
            .map(|dir| dir.join(&leaf))
            .find(|p| p.exists())
    }

    /// The main nib, if the `Info.plist` names one and it is there.
    ///
    /// `.nib` and `.storyboardc` are both directories in a modern bundle and a
    /// file in an older one; the name is taken from the plist either way.
    pub fn main_nib(&self) -> Option<PathBuf> {
        let name = self.info_string("NSMainNibFile")?;
        self.resource(name, "nib")
            .or_else(|| self.resource(name, "storyboardc"))
            .or_else(|| self.resource(name, ""))
    }

    /// The `.icns` the `Info.plist` names, if it is there.
    ///
    /// `CFBundleIconFile` is written both ways - with the extension and without
    /// it - and Apple accepts either, so both spellings are tried. A bundle
    /// whose icon is an asset catalog names it with `CFBundleIconName` and has
    /// no file to find, which is a `None` here rather than a diagnostic.
    pub fn icon(&self) -> Option<PathBuf> {
        let name = self.info_string("CFBundleIconFile")?;
        self.resource(name, "icns")
            .or_else(|| self.resource(name, ""))
    }

    /// `NSPrincipalClass`, which `NSApplicationMain` instantiates instead of
    /// `NSApplication`.
    pub fn principal_class(&self) -> Option<&str> {
        self.info_string("NSPrincipalClass")
    }
}

/// `CFBundleRef` is `NSBundle *`, so the CoreFoundation spellings live beside
/// the class rather than in `corefoundation` - which is underneath this crate
/// and has no bundle to answer with.
///
/// `CFBundleCopyResourceURL` is here for what it does when the resource is
/// *absent*, which is the answer a bundle usually has to give: A Hat in Time
/// asks for a `RadioEffectUnit` its own `Resources` does not contain, and a null
/// lets it skip the whole Carbon component path instead of being refused at the
/// import.
pub mod cf {
    // The C spellings are the interface; renaming them would only hide it.
    #![allow(non_snake_case)]

    use corefoundation::base::CFTypeRef;
    use objc::object::Id;

    /// `CFBundleRef CFBundleGetMainBundle(void)`
    ///
    /// A `Get`, so the caller does not own it - which is what the singleton
    /// already is: retained forever, and never handed out with a reference.
    unsafe extern "C" fn CFBundleGetMainBundle() -> Id {
        super::class::main_object()
    }

    /// `CFURLRef CFBundleCopyResourceURL(CFBundleRef, CFStringRef name,
    /// CFStringRef type, CFStringRef subDirName)`
    ///
    /// The subdirectory is ignored for the reason
    /// `-pathForResource:ofType:inDirectory:` ignores it.
    unsafe extern "C" fn CFBundleCopyResourceURL(
        _bundle: Id,
        name: Id,
        kind: Id,
        _subdirectory: Id,
    ) -> Id {
        // SAFETY: the caller's obligation - each is an `NSString` or nil.
        let (name, kind) = unsafe { (crate::string::bytes(name), crate::string::bytes(kind)) };
        let (Ok(name), Ok(kind)) = (std::str::from_utf8(&name), std::str::from_utf8(&kind)) else {
            return std::ptr::null_mut();
        };
        let Some(path) = super::main().and_then(|b| b.resource(name, kind)) else {
            return std::ptr::null_mut();
        };
        file_url(&path, false)
    }

    /// `CFURLRef CFBundleCopyExecutableURL(CFBundleRef)`
    unsafe extern "C" fn CFBundleCopyExecutableURL(_bundle: Id) -> Id {
        match super::main() {
            Some(bundle) => file_url(&bundle.executable, false),
            None => std::ptr::null_mut(),
        }
    }

    /// `CFURLRef CFBundleCopyBundleURL(CFBundleRef)`
    unsafe extern "C" fn CFBundleCopyBundleURL(_bundle: Id) -> Id {
        match super::main() {
            Some(bundle) => file_url(&bundle.root, true),
            None => std::ptr::null_mut(),
        }
    }

    /// `CFURLRef CFBundleCopyResourcesDirectoryURL(CFBundleRef)`
    ///
    /// The directory rather than a file in it, so the trailing slash a caller
    /// appends a name onto is there.
    unsafe extern "C" fn CFBundleCopyResourcesDirectoryURL(_bundle: Id) -> Id {
        match super::main() {
            Some(bundle) => file_url(&bundle.resources, true),
            None => std::ptr::null_mut(),
        }
    }

    /// `CFURLRef CFBundleCopyPrivateFrameworksURL(CFBundleRef)`
    ///
    /// Answered whether or not the directory exists, which is Apple's behaviour:
    /// the URL is where a private framework *would* be, and a caller that finds
    /// nothing there is entitled to that answer rather than to a null.
    unsafe extern "C" fn CFBundleCopyPrivateFrameworksURL(_bundle: Id) -> Id {
        match super::main() {
            Some(bundle) => file_url(&bundle.frameworks, true),
            None => std::ptr::null_mut(),
        }
    }

    /// `CFArrayRef CFBundleCopyBundleLocalizations(CFBundleRef)`.
    unsafe extern "C" fn CFBundleCopyBundleLocalizations(_bundle: Id) -> Id {
        let Some(bundle) = super::main() else {
            return std::ptr::null_mut();
        };
        let strings = bundle
            .localization_names()
            .iter()
            .map(|name| crate::string::from_str(name) as CFTypeRef)
            .collect::<Vec<_>>();
        let array = corefoundation::collections::new_array(
            &strings,
            corefoundation::callbacks::Policy::from_type_callbacks(),
            false,
        );
        for string in strings {
            // The array retained each newly-created string and now owns it.
            // SAFETY: each is the live object made immediately above.
            unsafe { corefoundation::api::CFRelease(string) };
        }
        array as Id
    }

    /// `CFArrayRef CFBundleCopyPreferredLocalizationsFromArray(CFArrayRef)`.
    unsafe extern "C" fn CFBundleCopyPreferredLocalizationsFromArray(localizations: Id) -> Id {
        // SAFETY: the caller's obligation - a CFArray or null.
        let Some(array) =
            (unsafe { corefoundation::collections::array(localizations as CFTypeRef) })
        else {
            return std::ptr::null_mut();
        };
        let language = crate::locale::language();
        let preferred = array.values.iter().copied().find(|value| {
            // SAFETY: localization arrays contain CFStrings.
            let bytes = unsafe { crate::string::bytes(*value as Id) };
            std::str::from_utf8(&bytes).is_ok_and(|name| {
                name.eq_ignore_ascii_case(&language)
                    || name
                        .split(['-', '_'])
                        .next()
                        .is_some_and(|base| base.eq_ignore_ascii_case(&language))
            })
        });
        let values = preferred
            .or_else(|| array.values.first().copied())
            .into_iter()
            .collect::<Vec<_>>();
        corefoundation::collections::new_array(
            &values,
            corefoundation::callbacks::Policy::from_type_callbacks(),
            false,
        ) as Id
    }

    /// `CFStringRef CFBundleGetIdentifier(CFBundleRef)`
    ///
    /// A `Get`, so the string is not the caller's to release - which is why it is
    /// interned rather than made fresh each time.
    unsafe extern "C" fn CFBundleGetIdentifier(_bundle: Id) -> Id {
        let Some(identifier) = super::main().and_then(|b| b.identifier()) else {
            return std::ptr::null_mut();
        };
        crate::notification::name_object(identifier) as Id
    }

    fn info_dictionary() -> Id {
        static INFO: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
        *INFO.get_or_init(|| {
            let Some(bundle) = super::main() else {
                return 0;
            };
            let object = crate::objectify::object(&bundle.info, "CFBundleGetInfoDictionary");
            // A Get result lives with the bundle rather than with an autorelease
            // pool, so keep the objectified plist for the process.
            objc::arc::retain(object);
            object as usize
        }) as Id
    }

    unsafe extern "C" fn CFBundleGetInfoDictionary(_bundle: Id) -> Id {
        info_dictionary()
    }

    unsafe extern "C" fn CFBundleGetValueForInfoDictionaryKey(_bundle: Id, key: Id) -> Id {
        let dictionary = info_dictionary();
        if dictionary.is_null() || key.is_null() {
            return std::ptr::null_mut();
        }
        // SAFETY: the cached object is a dictionary and `key` is the caller's
        // live CFString.
        unsafe {
            corefoundation::api::CFDictionaryGetValue(dictionary as CFTypeRef, key as CFTypeRef)
                as Id
        }
    }

    /// `UInt32 CFBundleGetVersionNumber(CFBundleRef)`
    ///
    /// Apple's is a packed BCD `vvvv.rr.ss` word. Only the major version is
    /// filled in: a caller compares the whole word against a constant of its
    /// own, and a fabricated revision would make that comparison say something
    /// the `Info.plist` never did.
    unsafe extern "C" fn CFBundleGetVersionNumber(_bundle: Id) -> u32 {
        let Some(version) = super::main().and_then(|b| b.version()) else {
            return 0;
        };
        let Some(major) = version
            .split('.')
            .next()
            .and_then(|major| major.parse::<u32>().ok())
        else {
            return 0;
        };
        // The major version occupies the top four BCD digits.
        let mut packed = 0u32;
        for (shift, digit) in format!("{major:0>4}").bytes().rev().enumerate().take(4) {
            packed |= u32::from(digit - b'0') << (16 + 4 * shift);
        }
        packed
    }

    /// `CFBundleRef CFBundleCreate(CFAllocatorRef, CFURLRef bundleURL)`
    ///
    /// Only the main bundle, and only when the URL names it. A guest asking about
    /// its own `.app` by URL - which is what a program that already called
    /// `CFBundleCopyExecutableURL` does - gets the bundle it means; anything else
    /// is a bundle this runtime has not loaded, and a null says so where an
    /// invented one would answer questions about the wrong directory.
    unsafe extern "C" fn CFBundleCreate(_allocator: *const std::ffi::c_void, url: Id) -> Id {
        // SAFETY: the caller's obligation - a `CFURL` or null.
        let Some(url) = (unsafe { corefoundation::url::as_url(url as CFTypeRef) }) else {
            return std::ptr::null_mut();
        };
        let Some(bundle) = super::main() else {
            return std::ptr::null_mut();
        };
        // A bundle is named by a file URL and by nothing else: a `https:` URL
        // has no directory for this to be about.
        let Some(path) = url.file_path() else {
            return std::ptr::null_mut();
        };
        if std::path::Path::new(&path) != bundle.root {
            return std::ptr::null_mut();
        }
        // A `Create`, so the caller owns a reference to the singleton.
        let object = super::class::main_object();
        objc::arc::retain(object);
        object
    }

    /// A path as a `CFURL` the caller owns.
    fn file_url(path: &std::path::Path, directory: bool) -> Id {
        match path.to_str() {
            Some(path) => corefoundation::url::new_file_url(path, directory) as Id,
            None => std::ptr::null_mut(),
        }
    }

    /// One entry point, as an address.
    macro_rules! at {
        ($f:expr) => {
            $f as *const () as usize as u64
        };
    }

    pub fn symbol(name: &str) -> Option<u64> {
        Some(match name {
            "CFBundleGetMainBundle" => at!(CFBundleGetMainBundle),
            "CFBundleCopyResourceURL" => at!(CFBundleCopyResourceURL),
            "CFBundleCopyExecutableURL" => at!(CFBundleCopyExecutableURL),
            "CFBundleCopyBundleURL" => at!(CFBundleCopyBundleURL),
            "CFBundleCopyResourcesDirectoryURL" => at!(CFBundleCopyResourcesDirectoryURL),
            "CFBundleCopyPrivateFrameworksURL" => at!(CFBundleCopyPrivateFrameworksURL),
            "CFBundleCopyBundleLocalizations" => at!(CFBundleCopyBundleLocalizations),
            "CFBundleCopyPreferredLocalizationsFromArray" => {
                at!(CFBundleCopyPreferredLocalizationsFromArray)
            }
            "CFBundleGetIdentifier" => at!(CFBundleGetIdentifier),
            "CFBundleGetInfoDictionary" => at!(CFBundleGetInfoDictionary),
            "CFBundleGetValueForInfoDictionaryKey" => {
                at!(CFBundleGetValueForInfoDictionaryKey)
            }
            "CFBundleGetVersionNumber" => at!(CFBundleGetVersionNumber),
            "CFBundleCreate" => at!(CFBundleCreate),
            _ => return None,
        })
    }

    /// Every name [`symbol`] answers.
    pub const NAMES: &[&str] = &[
        "CFBundleGetMainBundle",
        "CFBundleCopyResourceURL",
        "CFBundleCopyExecutableURL",
        "CFBundleCopyBundleURL",
        "CFBundleCopyResourcesDirectoryURL",
        "CFBundleCopyPrivateFrameworksURL",
        "CFBundleCopyBundleLocalizations",
        "CFBundleCopyPreferredLocalizationsFromArray",
        "CFBundleGetIdentifier",
        "CFBundleGetInfoDictionary",
        "CFBundleGetValueForInfoDictionaryKey",
        "CFBundleGetVersionNumber",
        "CFBundleCreate",
    ];
}

/// The bundle the program is, once something has said which.
static MAIN: RwLock<Option<&'static Layout>> = RwLock::new(None);

/// Say which `.app` the guest is.
///
/// Called by the runtime once the program is loaded, with the *guest's*
/// executable path. Nothing here reads the host process's own path: that is the
/// whole difference between this and the host's `NSBundle`, and the reason no
/// re-exec is needed.
///
/// Returns whether a bundle was found, so a caller can report a loose binary
/// rather than leave `+mainBundle` quietly nil.
pub fn set_main(executable: &Path) -> bool {
    let Some(layout) = Layout::enclosing(executable) else {
        return false;
    };
    *MAIN.write().unwrap_or_else(|e| e.into_inner()) = Some(Box::leak(Box::new(layout)));
    true
}

/// The main bundle, or `None` for a program that is not in one.
pub fn main() -> Option<&'static Layout> {
    *MAIN.read().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a bundle-shaped directory, since the claim is about shape.
    fn scratch(name: &str, info: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("rosetta3-bundle-{name}"));
        let _ = std::fs::remove_dir_all(&root);
        let app = root.join("Thing.app");
        std::fs::create_dir_all(app.join("Contents/MacOS")).unwrap();
        std::fs::create_dir_all(app.join("Contents/Resources/English.lproj")).unwrap();
        std::fs::write(app.join("Contents/Info.plist"), info).unwrap();
        std::fs::write(app.join("Contents/MacOS/Thing"), b"").unwrap();
        std::fs::write(app.join("Contents/Resources/icon.icns"), b"").unwrap();
        std::fs::write(
            app.join("Contents/Resources/English.lproj/MainMenu.nib"),
            b"",
        )
        .unwrap();
        app.join("Contents/MacOS/Thing")
    }

    const INFO: &str = r#"<?xml version="1.0"?><plist version="1.0"><dict>
        <key>CFBundleExecutable</key><string>Thing</string>
        <key>CFBundleIdentifier</key><string>com.example.thing</string>
        <key>CFBundleDevelopmentRegion</key><string>English</string>
        <key>CFBundleIconFile</key><string>icon.icns</string>
        <key>NSMainNibFile</key><string>MainMenu</string>
        <key>NSPrincipalClass</key><string>ThingApplication</string>
        </dict></plist>"#;

    #[test]
    fn a_bundle_is_recognised_by_shape_and_read_through() {
        let exe = scratch("shape", INFO);
        let layout = Layout::enclosing(&exe).expect("a bundle");
        assert_eq!(layout.identifier(), Some("com.example.thing"));
        assert_eq!(layout.principal_class(), Some("ThingApplication"));
        assert_eq!(layout.root.file_name().unwrap(), "Thing.app");
        // Found in `Resources`, and in the localization under it.
        assert!(layout.resource("icon", "icns").is_some());
        assert!(layout.main_nib().is_some(), "the nib is in English.lproj");
        assert!(layout.resource("nothing", "txt").is_none());
        assert_eq!(layout.localization_names(), ["English"]);
    }

    /// `CFBundleIconFile` is written with the extension as often as without,
    /// and a bundle whose icon is named the other way still has to be found.
    #[test]
    fn the_icon_is_found_whichever_way_the_plist_spells_it() {
        let with_extension = scratch("icon-spelt-out", INFO);
        let layout = Layout::enclosing(&with_extension).expect("a bundle");
        assert_eq!(layout.icon(), Some(layout.resources.join("icon.icns")));

        let bare = INFO.replace("<string>icon.icns</string>", "<string>icon</string>");
        let layout = Layout::enclosing(&scratch("icon-bare", &bare)).expect("a bundle");
        assert_eq!(layout.icon(), Some(layout.resources.join("icon.icns")));

        let none = INFO.replace("<key>CFBundleIconFile</key><string>icon.icns</string>", "");
        let layout = Layout::enclosing(&scratch("icon-absent", &none)).expect("a bundle");
        assert_eq!(layout.icon(), None);
    }

    /// The shape is the whole of the test: the same files with the wrong
    /// directory names are not a bundle.
    #[test]
    fn a_directory_that_is_not_bundle_shaped_is_not_a_bundle() {
        let exe = scratch("wrong", INFO);
        let loose = exe.parent().unwrap().parent().unwrap().join("Thing");
        std::fs::write(&loose, b"").unwrap();
        assert!(Layout::enclosing(&loose).is_none(), "Contents/Thing");
        assert!(Layout::enclosing(Path::new("/bin/ls")).is_none());
    }

    /// An `Info.plist` that cannot be read is a bundle this runtime declines to
    /// recognise, rather than one with an empty identity.
    #[test]
    fn an_unreadable_info_plist_is_not_a_bundle() {
        let exe = scratch("broken", "not a plist at all");
        assert!(Layout::enclosing(&exe).is_none());
    }
}

/// `NSBundle`, over the layout above.
///
/// One instance, the main one: nothing in this tree has yet loaded a second
/// bundle, and `+bundleWithPath:` would need a table keyed on the path. A guest
/// that asks for one gets the runtime's unrecognised-selector report with the
/// name in it, which is the work queue.
pub(crate) mod class {
    use std::ffi::c_char;

    use objc::object::Id;
    use objc::sel::Sel;

    use crate::define::{class_methods, methods};

    /// An `NSBundle`: the `isa`, and the layout it answers for.
    ///
    /// The main bundle's is null rather than a copy of `super::main()`, so that
    /// a bundle set up after the object was made still answers through it.
    #[repr(C)]
    struct Body {
        isa: *const objc::class::Class,
        layout: *const super::Layout,
    }

    /// The `NSBundle` singleton, created on the first `+mainBundle`.
    static MAIN_BUNDLE: std::sync::OnceLock<usize> = std::sync::OnceLock::new();

    unsafe extern "C" fn main_bundle(class: Id, _sel: Sel) -> Id {
        // SAFETY: `class` is `NSBundle`, which this crate registered.
        let class = match unsafe { objc::class::from_ptr(class as *const objc::class::Class) } {
            Some(class) => class,
            None => objc::fail!("+[NSBundle mainBundle] was sent to nil"),
        };
        of(class)
    }

    /// The one main-bundle object, which `+mainBundle` and
    /// `CFBundleGetMainBundle` both have to answer with.
    fn of(class: &'static objc::class::Class) -> Id {
        *MAIN_BUNDLE.get_or_init(|| {
            let object = objc::object::create_instance(class, 0);
            objc::arc::retain(object);
            object as usize
        }) as Id
    }

    /// `+bundleWithPath:`, autoreleased.
    unsafe extern "C" fn bundle_with_path(class: Id, _sel: Sel, path: Id) -> Id {
        // SAFETY: an `NSString`, or nil.
        let path = unsafe { crate::string::bytes(path) };
        let Ok(path) = std::str::from_utf8(&path) else {
            return std::ptr::null_mut();
        };
        let path = std::path::Path::new(path);
        if super::main().is_some_and(|main| main.root == path) {
            return main_object();
        }
        let Some(layout) = super::Layout::at(path) else {
            return std::ptr::null_mut();
        };
        // SAFETY: `class` is `NSBundle`, which this crate registered.
        let Some(class) = (unsafe { objc::class::from_ptr(class as *const objc::class::Class) })
        else {
            return std::ptr::null_mut();
        };
        let object = objc::object::create_instance(class, 0);
        // Leaked rather than owned by the object: a bundle is interned by Apple's
        // Foundation too, and a `-dealloc` freeing it would leave every string
        // this runtime handed out from it pointing at nothing.
        // SAFETY: a live instance whose second word is ours to fill in.
        unsafe { (*(object as *mut Body)).layout = Box::leak(Box::new(layout)) };
        objc::arc::autorelease(object)
    }

    /// The layout a bundle object answers for: its own, or the main one.
    ///
    /// # Safety
    ///
    /// `receiver` must be an `NSBundle` this module made.
    unsafe fn layout_of(receiver: Id) -> Option<&'static super::Layout> {
        if receiver.is_null() {
            return None;
        }
        // SAFETY: the caller's obligation - every `NSBundle` is a `Body`.
        let layout = unsafe { (*(receiver as *const Body)).layout };
        match layout.is_null() {
            // SAFETY: the pointer was leaked by `bundle_with_path`.
            false => Some(unsafe { &*layout }),
            true => super::main(),
        }
    }

    /// The main bundle, for a caller that has no class object in hand.
    pub(crate) fn main_object() -> Id {
        match objc::class::lookup(c"NSBundle") {
            Some(class) => of(class),
            None => std::ptr::null_mut(),
        }
    }

    /// An `Info.plist` string as an autoreleased `NSString`, or nil.
    ///
    /// # Safety
    ///
    /// As [`layout_of`].
    unsafe fn info(receiver: Id, key: &str) -> Id {
        // SAFETY: the caller's obligation.
        match unsafe { layout_of(receiver) }.and_then(|b| b.info_string(key)) {
            Some(text) => autoreleased(text),
            None => std::ptr::null_mut(),
        }
    }

    fn autoreleased(text: &str) -> Id {
        let string = crate::string::from_str(text);
        // SAFETY: a string this call just created.
        unsafe {
            corefoundation::api::CFAutorelease(string as corefoundation::base::CFTypeRef) as Id
        }
    }

    fn path(p: Option<std::path::PathBuf>) -> Id {
        match p {
            Some(p) => autoreleased(&p.to_string_lossy()),
            None => std::ptr::null_mut(),
        }
    }

    unsafe extern "C" fn bundle_path(receiver: Id, _sel: Sel) -> Id {
        // SAFETY: the receiver is an `NSBundle`; so for each accessor below.
        path(unsafe { layout_of(receiver) }.map(|b| b.root.clone()))
    }

    /// The `file:` URL forms of the two paths above, which is what a program
    /// that hands the bundle to `NSWorkspace` or `NSTask` asks for.
    fn url(p: Option<std::path::PathBuf>) -> Id {
        let Some(p) = p else {
            return std::ptr::null_mut();
        };
        // SAFETY: a URL this call just made.
        unsafe {
            corefoundation::api::CFAutorelease(corefoundation::url::new_file_url(
                &p.to_string_lossy(),
                true,
            )) as Id
        }
    }

    fn resource_file_url(p: Option<std::path::PathBuf>) -> Id {
        let Some(p) = p else {
            return std::ptr::null_mut();
        };
        // SAFETY: a URL this call just made.
        unsafe {
            corefoundation::api::CFAutorelease(corefoundation::url::new_file_url(
                &p.to_string_lossy(),
                p.is_dir(),
            )) as Id
        }
    }

    unsafe extern "C" fn bundle_url(receiver: Id, _sel: Sel) -> Id {
        // SAFETY: the receiver is an `NSBundle`.
        url(unsafe { layout_of(receiver) }.map(|b| b.root.clone()))
    }

    unsafe extern "C" fn resource_url(receiver: Id, _sel: Sel) -> Id {
        // SAFETY: as above.
        url(unsafe { layout_of(receiver) }.map(|b| b.resources.clone()))
    }

    unsafe extern "C" fn executable_url(receiver: Id, _sel: Sel) -> Id {
        // SAFETY: as above. Not a directory: this one names the binary.
        let Some(path) = unsafe { layout_of(receiver) }.map(|b| b.executable.clone()) else {
            return std::ptr::null_mut();
        };
        // SAFETY: a URL this call just made.
        unsafe {
            corefoundation::api::CFAutorelease(corefoundation::url::new_file_url(
                &path.to_string_lossy(),
                false,
            )) as Id
        }
    }

    unsafe extern "C" fn resource_path(receiver: Id, _sel: Sel) -> Id {
        path(unsafe { layout_of(receiver) }.map(|b| b.resources.clone()))
    }

    unsafe extern "C" fn executable_path(receiver: Id, _sel: Sel) -> Id {
        path(unsafe { layout_of(receiver) }.map(|b| b.executable.clone()))
    }

    unsafe extern "C" fn private_frameworks_path(receiver: Id, _sel: Sel) -> Id {
        path(unsafe { layout_of(receiver) }.map(|b| b.frameworks.clone()))
    }

    unsafe extern "C" fn bundle_identifier(receiver: Id, _sel: Sel) -> Id {
        unsafe { info(receiver, "CFBundleIdentifier") }
    }

    unsafe extern "C" fn object_for_info_key(receiver: Id, _sel: Sel, key: Id) -> Id {
        // SAFETY: the key is an `NSString`.
        let key = unsafe { crate::string::bytes(key) };
        match std::str::from_utf8(&key) {
            // SAFETY: the receiver is an `NSBundle`.
            Ok(key) => unsafe { info(receiver, key) },
            Err(_) => std::ptr::null_mut(),
        }
    }

    unsafe extern "C" fn path_for_resource(receiver: Id, _sel: Sel, name: Id, kind: Id) -> Id {
        // SAFETY: both are `NSString`s or nil.
        let (name, kind) = unsafe { (crate::string::bytes(name), crate::string::bytes(kind)) };
        let (Ok(name), Ok(kind)) = (std::str::from_utf8(&name), std::str::from_utf8(&kind)) else {
            return std::ptr::null_mut();
        };
        // SAFETY: the receiver is an `NSBundle`.
        path(unsafe { layout_of(receiver) }.and_then(|b| b.resource(name, kind)))
    }

    /// `-URLForResource:withExtension:`, the URL form of
    /// `-pathForResource:ofType:`.
    unsafe extern "C" fn url_for_resource(receiver: Id, _sel: Sel, name: Id, kind: Id) -> Id {
        // SAFETY: both are `NSString`s or nil.
        let (name, kind) = unsafe { (crate::string::bytes(name), crate::string::bytes(kind)) };
        let (Ok(name), Ok(kind)) = (std::str::from_utf8(&name), std::str::from_utf8(&kind)) else {
            return std::ptr::null_mut();
        };
        resource_file_url(unsafe { layout_of(receiver) }.and_then(|b| b.resource(name, kind)))
    }

    unsafe extern "C" fn path_for_resource_in_directory(
        receiver: Id,
        sel: Sel,
        name: Id,
        kind: Id,
        _directory: Id,
    ) -> Id {
        // The subdirectory is ignored: this runtime searches `Resources` and its
        // localizations whole, so a name that is there is found either way. A
        // bundle with the same leaf in two subdirectories would be answered
        // wrongly, and none in this tree has one.
        // SAFETY: the two names are `NSString`s or nil.
        unsafe { path_for_resource(receiver, sel, name, kind) }
    }

    /// `-infoDictionary`: the whole `Info.plist` as objects.
    ///
    /// This was refused until `crate::objectify` existed, and the reason is
    /// worth keeping: a dictionary missing a key is indistinguishable from a
    /// plist that never had one, so a *partial* conversion is worse than none.
    /// What makes it answerable now is that the conversion is total - every
    /// plist type is either built or refused by name.
    unsafe extern "C" fn info_dictionary(receiver: Id, _sel: Sel) -> Id {
        // SAFETY: the receiver is an `NSBundle`.
        match unsafe { layout_of(receiver) } {
            Some(layout) => crate::objectify::object(&layout.info, "-[NSBundle infoDictionary]"),
            None => std::ptr::null_mut(),
        }
    }

    unsafe extern "C" fn executable_name(_receiver: Id, _sel: Sel) -> *const c_char {
        std::ptr::null()
    }

    pub(crate) fn install() {
        let class =
            corefoundation::base::register(c"NSBundle", None, std::mem::size_of::<Body>(), false);
        methods!(class, {
            "bundlePath" => bundle_path, c"@16@0:8";
            "bundleURL" => bundle_url, c"@16@0:8";
            "resourceURL" => resource_url, c"@16@0:8";
            "executableURL" => executable_url, c"@16@0:8";
            "resourcePath" => resource_path, c"@16@0:8";
            "executablePath" => executable_path, c"@16@0:8";
            "privateFrameworksPath" => private_frameworks_path, c"@16@0:8";
            "builtInPlugInsPath" => private_frameworks_path, c"@16@0:8";
            "bundleIdentifier" => bundle_identifier, c"@16@0:8";
            "objectForInfoDictionaryKey:" => object_for_info_key, c"@24@0:8@16";
            "pathForResource:ofType:" => path_for_resource, c"@32@0:8@16@24";
            "URLForResource:withExtension:" => url_for_resource, c"@32@0:8@16@24";
            "pathForResource:ofType:inDirectory:" => path_for_resource_in_directory, c"@40@0:8@16@24@32";
            "infoDictionary" => info_dictionary, c"@16@0:8";
            "executableName" => executable_name, c"*16@0:8"
        });
        class_methods!(class, {
            "mainBundle" => main_bundle, c"@16@0:8";
            "bundleWithPath:" => bundle_with_path, c"@24@0:8@16"
        });
    }
}
