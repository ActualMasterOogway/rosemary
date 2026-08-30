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
        self.resource(name, "icns").or_else(|| self.resource(name, ""))
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
/// Only the two a program in this tree reaches. `CFBundleCopyResourceURL` is
/// here for what it does when the resource is *absent*, which is the answer a
/// bundle usually has to give: A Hat in Time asks for a `RadioEffectUnit` its
/// own `Resources` does not contain, and a null lets it skip the whole Carbon
/// component path instead of being refused at the import.
pub mod cf {
    // The C spellings are the interface; renaming them would only hide it.
    #![allow(non_snake_case)]

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
        // There is no `CFURL` in this tree. Refused by name rather than answered
        // with the path as a string: a caller passes what it gets here to
        // `CFURLGetFSRef` or `CFURLGetFileSystemRepresentation`, and a string in
        // that slot is a wrong type read as the right one.
        objc::fail!(
            "CFBundleCopyResourceURL: {} is in the bundle, and this runtime has no CFURL to \
             return it as",
            path.display()
        );
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
            _ => return None,
        })
    }

    /// Every name [`symbol`] answers.
    pub const NAMES: &[&str] = &["CFBundleGetMainBundle", "CFBundleCopyResourceURL"];
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
    ///
    /// A bundle object with no state: everything it answers comes from
    /// `super::main()`, so there is nothing to carry.
    fn of(class: &'static objc::class::Class) -> Id {
        *MAIN_BUNDLE.get_or_init(|| {
            let object = objc::object::create_instance(class, 0);
            objc::arc::retain(object);
            object as usize
        }) as Id
    }

    /// The main bundle, for a caller that has no class object in hand.
    pub(crate) fn main_object() -> Id {
        match objc::class::lookup(c"NSBundle") {
            Some(class) => of(class),
            None => std::ptr::null_mut(),
        }
    }

    /// An `Info.plist` string as an autoreleased `NSString`, or nil.
    fn info(key: &str) -> Id {
        match super::main().and_then(|b| b.info_string(key)) {
            Some(text) => autoreleased(text),
            None => std::ptr::null_mut(),
        }
    }

    fn autoreleased(text: &str) -> Id {
        let string = crate::string::from_str(text);
        // SAFETY: a string this call just created.
        unsafe { corefoundation::api::CFAutorelease(string as corefoundation::base::CFTypeRef) as Id }
    }

    fn path(p: Option<std::path::PathBuf>) -> Id {
        match p {
            Some(p) => autoreleased(&p.to_string_lossy()),
            None => std::ptr::null_mut(),
        }
    }

    unsafe extern "C" fn bundle_path(_receiver: Id, _sel: Sel) -> Id {
        path(super::main().map(|b| b.root.clone()))
    }

    unsafe extern "C" fn resource_path(_receiver: Id, _sel: Sel) -> Id {
        path(super::main().map(|b| b.resources.clone()))
    }

    unsafe extern "C" fn executable_path(_receiver: Id, _sel: Sel) -> Id {
        path(super::main().map(|b| b.executable.clone()))
    }

    unsafe extern "C" fn private_frameworks_path(_receiver: Id, _sel: Sel) -> Id {
        path(super::main().map(|b| b.frameworks.clone()))
    }

    unsafe extern "C" fn bundle_identifier(_receiver: Id, _sel: Sel) -> Id {
        info("CFBundleIdentifier")
    }

    unsafe extern "C" fn object_for_info_key(_receiver: Id, _sel: Sel, key: Id) -> Id {
        // SAFETY: the key is an `NSString`.
        let key = unsafe { crate::string::bytes(key) };
        match std::str::from_utf8(&key) {
            Ok(key) => info(key),
            Err(_) => std::ptr::null_mut(),
        }
    }

    unsafe extern "C" fn path_for_resource(
        _receiver: Id,
        _sel: Sel,
        name: Id,
        kind: Id,
    ) -> Id {
        // SAFETY: both are `NSString`s or nil.
        let (name, kind) = unsafe { (crate::string::bytes(name), crate::string::bytes(kind)) };
        let (Ok(name), Ok(kind)) = (std::str::from_utf8(&name), std::str::from_utf8(&kind)) else {
            return std::ptr::null_mut();
        };
        path(super::main().and_then(|b| b.resource(name, kind)))
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
    unsafe extern "C" fn info_dictionary(_receiver: Id, _sel: Sel) -> Id {
        match super::main() {
            Some(layout) => crate::objectify::object(&layout.info, "-[NSBundle infoDictionary]"),
            None => std::ptr::null_mut(),
        }
    }

    unsafe extern "C" fn executable_name(_receiver: Id, _sel: Sel) -> *const c_char {
        std::ptr::null()
    }

    pub(crate) fn install() {
        let class = corefoundation::base::register(
            c"NSBundle",
            None,
            std::mem::size_of::<usize>(),
            false,
        );
        methods!(class, {
            "bundlePath" => bundle_path, c"@16@0:8";
            "resourcePath" => resource_path, c"@16@0:8";
            "executablePath" => executable_path, c"@16@0:8";
            "privateFrameworksPath" => private_frameworks_path, c"@16@0:8";
            "builtInPlugInsPath" => private_frameworks_path, c"@16@0:8";
            "bundleIdentifier" => bundle_identifier, c"@16@0:8";
            "objectForInfoDictionaryKey:" => object_for_info_key, c"@24@0:8@16";
            "pathForResource:ofType:" => path_for_resource, c"@32@0:8@16@24";
            "pathForResource:ofType:inDirectory:" => path_for_resource_in_directory, c"@40@0:8@16@24@32";
            "infoDictionary" => info_dictionary, c"@16@0:8";
            "executableName" => executable_name, c"*16@0:8"
        });
        class_methods!(class, {
            "mainBundle" => main_bundle, c"@16@0:8"
        });
    }
}
