//! Instantiating a nib.
//!
//! A `MainMenu.nib` is where a Cocoa program's application delegate is wired,
//! and usually the only place: `NSApplicationMain` cannot reach
//! `applicationDidFinishLaunching:` without reading one. What it holds is an
//! `NSIBObjectData` - a list of objects, and a list of connections between them.
//!
//! Three kinds of entry matter here, and the rest is M2's:
//!
//! * `NSCustomObject` names a class by string, which is how a program's own
//!   delegate gets built;
//! * `NSNibOutletConnector` is `[source setValue:destination forKey:label]`,
//!   which is why key-value coding had to exist before this did;
//! * `NSNibControlConnector` is a target and an action, which needs a control to
//!   set them on.
//!
//! Everything else - `NSWindowTemplate`, `NSTextView`, the thirty-odd view
//! classes an Interface Builder document carries - is instantiated as an object
//! of a class with no methods, and **named in the report**. That is not the same
//! as skipping it: the object exists, the graph is whole, and the first message
//! a program sends one is refused by the runtime with the class and the
//! selector. A nib that a program only reads its delegate out of therefore
//! loads, and a nib whose window it goes on to use says which class it needs.

use std::collections::BTreeMap;
use std::path::Path;

use foundation::unarchiver::{Archive, Uid};
use objc::object::Id;

/// What a load produced.
#[derive(Debug)]
pub struct Loaded {
    /// The objects the nib built, by their place in the archive.
    pub objects: BTreeMap<Uid, Id>,
    /// Class names the nib held that nothing implements, deduplicated.
    pub placeholders: Vec<String>,
    /// Connections that were not made, each as a sentence naming why.
    pub unwired: Vec<String>,
}

impl Loaded {
    /// Print what could not be built, which is the queue for the next milestone.
    ///
    /// Unconditional rather than behind a trace variable. A nib that half-loaded
    /// is the single most likely cause of the next thing that goes wrong, and a
    /// program is loaded once.
    pub fn report(&self, path: &Path) {
        if !self.placeholders.is_empty() {
            eprintln!(
                "rosetta3: appkit: {}: {} class(es) with no implementation: {}",
                path.display(),
                self.placeholders.len(),
                self.placeholders.join(", ")
            );
        }
        for line in &self.unwired {
            eprintln!("rosetta3: appkit: {}: {line}", path.display());
        }
    }
}

/// Read the nib at `path` and wire it, with `owner` standing in for File's
/// Owner.
///
/// `path` may be a file or a `.nib` directory, which is how a modern Xcode
/// writes one. In the directory case the `keyedobjects.nib` inside it is the
/// archive; a sibling `keyedobjects-NNNNNN.nib` is the same document in the
/// `NIBArchive` format, which is a different format entirely and is not read
/// here.
pub fn load(path: &Path, owner: Id) -> Result<Loaded, String> {
    let file = match path.is_dir() {
        true => path.join("keyedobjects.nib"),
        false => path.to_path_buf(),
    };
    let bytes = std::fs::read(&file).map_err(|e| format!("{}: {e}", file.display()))?;
    let archive = Archive::parse(&bytes).map_err(|e| e.to_string())?;

    // `IB.objectdata` is where Interface Builder files the object data; `root`
    // is what an archive made any other way calls its top object. Both, because
    // a nib is only one of the two and a caller cannot tell which from outside.
    let root = archive
        .top("IB.objectdata")
        .or_else(|| archive.root())
        .ok_or("the archive names no top-level object")?;
    if !archive.is_kind_of(root, "NSIBObjectData") {
        return Err(format!(
            "the archive's top object is a {}, not an NSIBObjectData",
            archive.class_name(root).unwrap_or("<untyped entry>")
        ));
    }

    let mut builder = Builder {
        archive: &archive,
        owner,
        file_owner: archive.field_object(root, "NSRoot"),
        loaded: Loaded {
            objects: BTreeMap::new(),
            placeholders: Vec::new(),
            unwired: Vec::new(),
        },
    };
    builder.connect(root);
    Ok(builder.loaded)
}

/// Whether something implements a class of this name.
///
/// Not the same question as whether one is registered: a class invented for an
/// earlier entry is registered and implements nothing, and treating it as real
/// is how a second connection to the same window template would come to be
/// wired to an object with no methods.
fn implemented(name: &str) -> bool {
    !crate::classes::is_placeholder(name)
        && std::ffi::CString::new(name)
            .ok()
            .and_then(|n| objc::class::lookup(&n))
            .is_some()
}

struct Builder<'a> {
    archive: &'a Archive,
    owner: Id,
    /// The entry the archive calls `NSRoot`, which is File's Owner: the one
    /// object a nib does not build, because the caller supplies it.
    file_owner: Option<Uid>,
    loaded: Loaded,
}

impl Builder<'_> {
    /// Walk `NSConnections` and apply each one.
    fn connect(&mut self, root: Uid) {
        let Some(connections) = self.archive.field_object(root, "NSConnections") else {
            return;
        };
        for connector in self.archive.array(connections) {
            let kind = self.archive.class_name(connector).unwrap_or_default();
            let label = self
                .archive
                .field_uid(connector, "NSLabel")
                .and_then(|l| self.archive.string(l))
                .unwrap_or_default()
                .to_string();
            let Some(source) = self.archive.field_object(connector, "NSSource") else {
                self.loaded
                    .unwired
                    .push(format!("a {kind} for \"{label}\" names no source"));
                continue;
            };
            let source_class = self
                .archive
                .class_name(source)
                .unwrap_or("<untyped entry>")
                .to_string();
            match kind {
                "NSNibOutletConnector" | "NSNibOutletConnector2" => {
                    // An outlet with no destination is an outlet set to nil,
                    // which the object already is.
                    if let Some(destination) = self.archive.field_object(connector, "NSDestination")
                    {
                        self.outlet(source, destination, &label);
                    }
                }
                // A control connector is a target and an action on a control,
                // and a control is a view. Recorded rather than applied: the
                // source is a placeholder until M2, and `-setTarget:` on one
                // would be refused inside the loader rather than at the click
                // that used it. A null destination is not a fault here - it is
                // how a menu item says "send this up the responder chain".
                "NSNibControlConnector" => self.loaded.unwired.push(format!(
                    "the action \"{label}\" on a {source_class} is not connected until there \
                     are controls"
                )),
                other => self
                    .loaded
                    .unwired
                    .push(format!("a connector of an unknown kind, {other}")),
            }
        }
    }

    /// `[source setValue:destination forKey:label]`.
    ///
    /// Skipped, with a sentence, when the source is one of the classes this
    /// crate invented: key-value coding on an object with neither a setter nor
    /// an ivar of that name is an abort, and aborting inside the loader would
    /// replace the diagnostic with a stop.
    fn outlet(&mut self, source: Uid, destination: Uid, label: &str) {
        if let Some(class) = self.unimplemented(source) {
            self.loaded.unwired.push(format!(
                "the outlet \"{label}\" of a {class} is not set, because this runtime has no \
                 {class}"
            ));
            return;
        }
        let source_class = self.archive.class_name(source).unwrap_or("<untyped entry>");
        let (Some(from), Some(to)) = (self.instantiate(source), self.instantiate(destination))
        else {
            self.loaded
                .unwired
                .push(format!("the outlet \"{label}\" of a {source_class} has no object to set"));
            return;
        };
        let key = foundation::string::from_str(label);
        // SAFETY: `-setValue:forKey:` is `void (id, SEL, id, id)`, installed on
        // `NSObject` by `foundation::kvc`, so every object here answers it.
        unsafe {
            let f: unsafe extern "C" fn(Id, objc::sel::Sel, Id, Id) =
                std::mem::transmute(objc::msg::entry());
            f(from, objc::sel::get("setValue:forKey:"), to, key);
        }
    }

    /// The entry's class, when nothing in this process implements it, noted in
    /// the report on the way past.
    ///
    /// `None` for the owner - the caller supplied that object, so what the nib
    /// recorded about its class is a claim about the document rather than about
    /// this process - and for a plain entry, which is a string and needs no
    /// class at all.
    fn unimplemented(&mut self, uid: Uid) -> Option<String> {
        if Some(uid) == self.file_owner {
            return None;
        }
        let name = match self.archive.class_name(uid) {
            Some("NSCustomObject") => self.custom_class_name(uid)?,
            Some(other) => other.to_string(),
            None => return None,
        };
        if implemented(&name) {
            return None;
        }
        self.note(&name);
        Some(name)
    }

    /// Record a class name the nib held that nothing implements, once.
    fn note(&mut self, name: &str) {
        if !self.loaded.placeholders.iter().any(|p| p == name) {
            self.loaded.placeholders.push(name.to_string());
        }
    }

    fn custom_class_name(&self, uid: Uid) -> Option<String> {
        let name = self.archive.field_uid(uid, "NSClassName")?;
        Some(self.archive.string(name)?.to_string())
    }

    /// The object for an entry, built once and remembered.
    fn instantiate(&mut self, uid: Uid) -> Option<Id> {
        if let Some(existing) = self.loaded.objects.get(&uid) {
            return Some(*existing);
        }
        if Some(uid) == self.file_owner {
            self.loaded.objects.insert(uid, self.owner);
            return Some(self.owner);
        }

        let object = match self.archive.class_name(uid) {
            // A plain entry that is a string is a string, and the archive holds
            // its text rather than an object to build.
            None => foundation::string::from_str(self.archive.string(uid)?),
            Some("NSCustomObject") => {
                let name = self.custom_class_name(uid)?;
                self.build(&name)
            }
            Some(name) => {
                let name = name.to_string();
                self.build(&name)
            }
        };
        objc::arc::retain(object);
        self.loaded.objects.insert(uid, object);
        Some(object)
    }

    /// An instance of `name`, or of a class invented for it.
    ///
    /// `-init` is *sent* rather than skipped for a class this process has, since
    /// a guest's delegate is entitled to have one and a nib is how it is created.
    /// An invented class has no `-init` at all, so nothing is sent to one.
    fn build(&mut self, name: &str) -> Id {
        let known = implemented(name)
            .then(|| std::ffi::CString::new(name).ok().and_then(|n| objc::class::lookup(&n)))
            .flatten();
        let Some(class) = known else {
            self.note(name);
            let Some(class) = crate::classes::placeholder(name) else {
                return std::ptr::null_mut();
            };
            return objc::object::create_instance(class, 0);
        };
        let object = objc::object::create_instance(class, 0);
        if class.method(objc::sel::get("init")).is_some() {
            // SAFETY: `-init` is `id (id, SEL)` for every class that has one.
            unsafe {
                let f: unsafe extern "C" fn(Id, objc::sel::Sel) -> Id =
                    std::mem::transmute(objc::msg::entry());
                return f(object, objc::sel::get("init"));
            }
        }
        object
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A nib with the one connection that matters: File's Owner's `delegate`
    /// outlet, wired to a custom object the archive names by class.
    ///
    /// Written as the XML spelling of a keyed archive, which is what `plutil
    /// -convert xml1` makes of the binary one an Interface Builder document is,
    /// and read by the same code that reads the binary.
    const NIB: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0">
<dict>
	<key>$archiver</key>
	<string>NSKeyedArchiver</string>
	<key>$objects</key>
	<array>
		<string>$null</string>
		<dict>
			<key>$class</key><dict><key>CF$UID</key><integer>10</integer></dict>
			<key>NSConnections</key><dict><key>CF$UID</key><integer>4</integer></dict>
			<key>NSRoot</key><dict><key>CF$UID</key><integer>2</integer></dict>
		</dict>
		<dict>
			<key>$class</key><dict><key>CF$UID</key><integer>11</integer></dict>
			<key>NSClassName</key><dict><key>CF$UID</key><integer>3</integer></dict>
		</dict>
		<string>NSApplication</string>
		<dict>
			<key>$class</key><dict><key>CF$UID</key><integer>12</integer></dict>
			<key>NS.objects</key>
			<array>
				<dict><key>CF$UID</key><integer>5</integer></dict>
				<dict><key>CF$UID</key><integer>8</integer></dict>
			</array>
		</dict>
		<dict>
			<key>$class</key><dict><key>CF$UID</key><integer>13</integer></dict>
			<key>NSSource</key><dict><key>CF$UID</key><integer>2</integer></dict>
			<key>NSDestination</key><dict><key>CF$UID</key><integer>6</integer></dict>
			<key>NSLabel</key><dict><key>CF$UID</key><integer>7</integer></dict>
		</dict>
		<dict>
			<key>$class</key><dict><key>CF$UID</key><integer>11</integer></dict>
			<key>NSClassName</key><dict><key>CF$UID</key><integer>14</integer></dict>
		</dict>
		<string>delegate</string>
		<dict>
			<key>$class</key><dict><key>CF$UID</key><integer>13</integer></dict>
			<key>NSSource</key><dict><key>CF$UID</key><integer>9</integer></dict>
			<key>NSDestination</key><dict><key>CF$UID</key><integer>2</integer></dict>
			<key>NSLabel</key><dict><key>CF$UID</key><integer>7</integer></dict>
		</dict>
		<dict>
			<key>$class</key><dict><key>CF$UID</key><integer>15</integer></dict>
		</dict>
		<dict>
			<key>$classes</key><array><string>NSIBObjectData</string></array>
			<key>$classname</key><string>NSIBObjectData</string>
		</dict>
		<dict>
			<key>$classes</key><array><string>NSCustomObject</string></array>
			<key>$classname</key><string>NSCustomObject</string>
		</dict>
		<dict>
			<key>$classes</key><array><string>NSMutableArray</string><string>NSArray</string></array>
			<key>$classname</key><string>NSMutableArray</string>
		</dict>
		<dict>
			<key>$classes</key><array><string>NSNibOutletConnector</string></array>
			<key>$classname</key><string>NSNibOutletConnector</string>
		</dict>
		<string>R3TestNibDelegate</string>
		<dict>
			<key>$classes</key><array><string>NSWindowTemplate</string></array>
			<key>$classname</key><string>NSWindowTemplate</string>
		</dict>
	</array>
	<key>$top</key>
	<dict>
		<key>IB.objectdata</key><dict><key>CF$UID</key><integer>1</integer></dict>
	</dict>
	<key>$version</key>
	<integer>100000</integer>
</dict>
</plist>"#;

    /// The class the nib names as its custom object, registered so the loader
    /// finds one rather than inventing a placeholder for it.
    fn delegate_class() {
        crate::bootstrap();
        corefoundation::base::register(c"R3TestNibDelegate", None, size_of::<usize>(), false);
    }

    /// A file of this test's own: the tests run in one process and in parallel,
    /// so a shared path is two of them writing the same bytes and one of them
    /// deleting the other's.
    fn write(name: &str, nib: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("r3-nib-{}-{name}.nib", std::process::id()));
        std::fs::write(&path, nib).expect("writable");
        path
    }

    #[test]
    fn the_owners_outlet_is_wired_to_the_object_the_nib_names() {
        delegate_class();
        let owner = objc::object::create_instance(
            objc::class::lookup(c"NSApplication").expect("NSApplication"),
            0,
        );
        let path = write("outlet", NIB);
        let loaded = load(&path, owner).expect("a nib");
        let _ = std::fs::remove_file(&path);

        // SAFETY: `-delegate` is `id (id, SEL)` on `NSApplication`.
        let delegate = unsafe {
            let f: unsafe extern "C" fn(Id, objc::sel::Sel) -> Id =
                std::mem::transmute(objc::msg::entry());
            f(owner, objc::sel::get("delegate"))
        };
        assert!(!delegate.is_null(), "the delegate outlet was not set");
        // SAFETY: a live object this runtime allocated.
        let class = unsafe { objc::object::class_of(delegate) }.expect("a class");
        assert_eq!(class.name_str(), "R3TestNibDelegate");
        assert!(
            loaded.placeholders.contains(&"NSWindowTemplate".to_string()),
            "the window template was not reported: {:?}",
            loaded.placeholders
        );
    }

    /// The connection whose source is a class nothing implements is *named*,
    /// not silently dropped and not fatal.
    #[test]
    fn a_connection_from_an_unimplemented_class_is_reported() {
        delegate_class();
        let owner = objc::object::create_instance(
            objc::class::lookup(c"NSApplication").expect("NSApplication"),
            0,
        );
        let path = write("reported", NIB);
        let loaded = load(&path, owner).expect("a nib");
        let _ = std::fs::remove_file(&path);
        assert!(
            loaded
                .unwired
                .iter()
                .any(|u| u.contains("NSWindowTemplate") && u.contains("delegate")),
            "{:?}",
            loaded.unwired
        );
    }

    #[test]
    fn a_plist_that_is_not_a_nib_is_an_error_rather_than_a_stop() {
        crate::bootstrap();
        let path = write(
            "notanib",
            "<?xml version=\"1.0\"?><plist version=\"1.0\"><dict>\
             <key>$archiver</key><string>NSKeyedArchiver</string>\
             <key>$objects</key><array><string>$null</string></array>\
             <key>$top</key><dict><key>root</key><dict><key>CF$UID</key><integer>0</integer>\
             </dict></dict></dict></plist>",
        );
        let err = load(&path, std::ptr::null_mut()).expect_err("not a nib");
        let _ = std::fs::remove_file(&path);
        assert!(err.contains("NSIBObjectData"), "{err}");
    }
}
