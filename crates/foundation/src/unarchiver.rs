//! Reading an `NSKeyedArchiver` archive.
//!
//! A nib is one of these: `bplist00` on the outside, with `$objects` a flat
//! table and every reference into it a `CF$UID`. So [`crate::plist`] already
//! reads the file, and what is left is the indirection - which is what this
//! module is.
//!
//! Deliberately not a decoder that hands back a tree. A nib's object graph has
//! cycles in it: an `NSNibOutletConnector` names a source, and the source's own
//! connection list names the connector back. A tree walk would not terminate, so
//! references stay [`Uid`]s and the caller follows them when it means to.
//!
//! Nothing here builds an object. Turning an entry into an instance needs a
//! class and a runtime, which is `appkit::nib`'s business; this module only says
//! what the file contains.

use std::collections::BTreeMap;

use crate::plist::{self, Value};

/// An index into the archive's object table.
///
/// Zero is `$null`, which is how the format spells `nil`, so [`Uid::is_null`]
/// is the check a caller makes before following one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Uid(pub usize);

impl Uid {
    pub fn is_null(self) -> bool {
        self.0 == 0
    }
}

#[derive(Debug)]
pub struct Archive {
    objects: Vec<Value>,
    top: BTreeMap<String, Uid>,
}

/// What went wrong, as a sentence. [`plist::Error`] widened, not wrapped: a
/// caller reporting one reports the other the same way.
pub type Error = plist::Error;

fn bad(what: impl std::fmt::Display) -> Error {
    plist::Error(what.to_string())
}

impl Archive {
    pub fn parse(bytes: &[u8]) -> Result<Archive, Error> {
        Archive::from_plist(plist::parse(bytes)?)
    }

    pub fn from_plist(root: Value) -> Result<Archive, Error> {
        let archiver = root.get("$archiver").and_then(Value::as_str);
        if archiver != Some("NSKeyedArchiver") {
            return Err(bad(format!(
                "not a keyed archive: $archiver is {}",
                archiver.unwrap_or("absent")
            )));
        }
        let objects = root
            .get("$objects")
            .and_then(Value::as_array)
            .ok_or_else(|| bad("a keyed archive with no $objects table"))?
            .to_vec();
        let mut top = BTreeMap::new();
        for (key, value) in root
            .get("$top")
            .and_then(Value::as_dictionary)
            .ok_or_else(|| bad("a keyed archive with no $top"))?
        {
            if let Some(uid) = as_uid(value) {
                top.insert(key.clone(), uid);
            }
        }
        Ok(Archive { objects, top })
    }

    /// The entry `$top` files under `key`, which for a nib is `"root"`.
    pub fn top(&self, key: &str) -> Option<Uid> {
        self.top.get(key).copied()
    }

    pub fn root(&self) -> Option<Uid> {
        self.top("root")
    }

    /// The raw entry at `uid`, or `None` for one past the end of the table.
    pub fn value(&self, uid: Uid) -> Option<&Value> {
        self.objects.get(uid.0)
    }

    /// The class chain of the object at `uid`, most-derived first.
    ///
    /// Empty for an entry that is not an archived object - a string, a number,
    /// or `$null` - which is how a caller tells the two apart.
    pub fn classes(&self, uid: Uid) -> &[Value] {
        let Some(class) = self.field_uid(uid, "$class") else {
            return &[];
        };
        self.value(class)
            .and_then(|c| c.get("$classes"))
            .and_then(Value::as_array)
            .unwrap_or(&[])
    }

    /// The name the archive recorded for the object's own class.
    pub fn class_name(&self, uid: Uid) -> Option<&str> {
        let class = self.field_uid(uid, "$class")?;
        self.value(class)?.get("$classname")?.as_str()
    }

    /// Whether the object at `uid` is a `name`, or descends from one.
    ///
    /// The archive's own chain rather than the runtime's: an archived
    /// `NSMutableArray` records `NSArray` above it whether or not this process
    /// has ever registered either class.
    pub fn is_kind_of(&self, uid: Uid, name: &str) -> bool {
        self.classes(uid)
            .iter()
            .filter_map(Value::as_str)
            .any(|c| c == name)
    }

    /// A keyed field of the object at `uid`, undereferenced.
    pub fn field(&self, uid: Uid, key: &str) -> Option<&Value> {
        self.value(uid)?.get(key)
    }

    /// A keyed field that is a reference.
    pub fn field_uid(&self, uid: Uid, key: &str) -> Option<Uid> {
        as_uid(self.field(uid, key)?)
    }

    /// A keyed field that is a reference, with `$null` read as absent.
    ///
    /// The distinction matters exactly once per nib: an `NSCustomObject`'s
    /// `NSObject` field is the instance it was already given, and `$null` there
    /// means "instantiate one", not "the field is missing".
    pub fn field_object(&self, uid: Uid, key: &str) -> Option<Uid> {
        self.field_uid(uid, key).filter(|u| !u.is_null())
    }

    /// The text of a string, whether it was archived as an object or written
    /// into the table directly.
    ///
    /// Both occur in one nib: a short key is a plain entry, and an `NSString`
    /// the graph refers to more than once is an object with `NS.string`.
    pub fn string(&self, uid: Uid) -> Option<&str> {
        match self.value(uid)? {
            Value::String(s) => Some(s),
            _ => self
                .field(uid, "NS.string")
                .or_else(|| self.field(uid, "NS.bytes"))
                .and_then(Value::as_str),
        }
    }

    /// The elements of an archived `NSArray` or `NSSet`.
    pub fn array(&self, uid: Uid) -> Vec<Uid> {
        let entries = match self.value(uid) {
            Some(Value::Array(a)) => a.as_slice(),
            _ => self
                .field(uid, "NS.objects")
                .and_then(Value::as_array)
                .unwrap_or(&[]),
        };
        entries.iter().filter_map(as_uid).collect()
    }

    /// The `(key, value)` pairs of an archived `NSDictionary`, in stored order.
    pub fn dictionary(&self, uid: Uid) -> Vec<(Uid, Uid)> {
        let keys = self
            .field(uid, "NS.keys")
            .and_then(Value::as_array)
            .unwrap_or(&[]);
        let values = self
            .field(uid, "NS.objects")
            .and_then(Value::as_array)
            .unwrap_or(&[]);
        keys.iter()
            .zip(values)
            .filter_map(|(k, v)| Some((as_uid(k)?, as_uid(v)?)))
            .collect()
    }

    pub fn integer(&self, uid: Uid) -> Option<i64> {
        match self.value(uid)? {
            Value::Integer(v) => Some(*v),
            Value::Bool(b) => Some(*b as i64),
            _ => None,
        }
    }

    /// How many entries the table has, for a caller reporting on a whole file.
    pub fn len(&self) -> usize {
        self.objects.len()
    }

    pub fn is_empty(&self) -> bool {
        self.objects.is_empty()
    }
}

/// A reference, in either of the two spellings a `CF$UID` has.
///
/// The binary format has a marker of its own; the XML one - what `plutil
/// -convert xml1` writes, and what a hand-edited archive is - has no such
/// marker and renders it as a one-key dictionary instead.
fn as_uid(value: &Value) -> Option<Uid> {
    match value {
        Value::Uid(v) => Some(Uid(*v as usize)),
        Value::Dictionary(d) => match d.get("CF$UID")? {
            Value::Integer(v) if *v >= 0 => Some(Uid(*v as usize)),
            _ => None,
        },
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What `plutil -convert xml1` writes for
    /// `@{@"name": @"outlet", @"objects": @[@1, @"three"]}` archived by Apple's
    /// `NSKeyedArchiver`, trimmed to the entries the assertions read.
    const ARCHIVE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0">
<dict>
	<key>$archiver</key>
	<string>NSKeyedArchiver</string>
	<key>$objects</key>
	<array>
		<string>$null</string>
		<dict>
			<key>$class</key>
			<dict><key>CF$UID</key><integer>8</integer></dict>
			<key>NS.keys</key>
			<array>
				<dict><key>CF$UID</key><integer>2</integer></dict>
				<dict><key>CF$UID</key><integer>3</integer></dict>
			</array>
			<key>NS.objects</key>
			<array>
				<dict><key>CF$UID</key><integer>4</integer></dict>
				<dict><key>CF$UID</key><integer>5</integer></dict>
			</array>
		</dict>
		<string>name</string>
		<string>objects</string>
		<string>outlet</string>
		<dict>
			<key>$class</key>
			<dict><key>CF$UID</key><integer>7</integer></dict>
			<key>NS.objects</key>
			<array>
				<dict><key>CF$UID</key><integer>6</integer></dict>
			</array>
		</dict>
		<string>three</string>
		<dict>
			<key>$classes</key>
			<array><string>NSArray</string><string>NSObject</string></array>
			<key>$classname</key>
			<string>NSArray</string>
		</dict>
		<dict>
			<key>$classes</key>
			<array><string>NSDictionary</string><string>NSObject</string></array>
			<key>$classname</key>
			<string>NSDictionary</string>
		</dict>
	</array>
	<key>$top</key>
	<dict>
		<key>root</key>
		<dict><key>CF$UID</key><integer>1</integer></dict>
	</dict>
	<key>$version</key>
	<integer>100000</integer>
</dict>
</plist>"#;

    fn archive() -> Archive {
        Archive::parse(ARCHIVE.as_bytes()).expect("a keyed archive")
    }

    #[test]
    fn a_reference_is_followed_to_its_entry() {
        let a = archive();
        let root = a.root().expect("$top has a root");
        assert_eq!(a.class_name(root), Some("NSDictionary"));
        let pairs = a.dictionary(root);
        let keys: Vec<&str> = pairs.iter().filter_map(|(k, _)| a.string(*k)).collect();
        assert_eq!(keys, ["name", "objects"]);
    }

    #[test]
    fn the_class_chain_is_the_archives_own() {
        let a = archive();
        let root = a.root().expect("a root");
        assert!(a.is_kind_of(root, "NSObject"));
        assert!(!a.is_kind_of(root, "NSArray"));
    }

    #[test]
    fn a_nested_array_reads_as_references() {
        let a = archive();
        let root = a.root().expect("a root");
        let (_, objects) = a
            .dictionary(root)
            .into_iter()
            .find(|(k, _)| a.string(*k) == Some("objects"))
            .expect("the objects key");
        let elements = a.array(objects);
        assert_eq!(elements.len(), 1);
        assert_eq!(a.string(elements[0]), Some("three"));
    }

    /// `$null` is a reference like any other, and the one a caller must not
    /// follow: entry zero is the string `$null`, not the object it stands for.
    #[test]
    fn null_is_a_reference_that_is_not_an_object() {
        let a = archive();
        assert!(Uid(0).is_null());
        assert_eq!(a.class_name(Uid(0)), None);
        assert!(a.classes(Uid(0)).is_empty());
    }

    #[test]
    fn a_plist_that_is_not_an_archive_says_so() {
        let err = Archive::parse(b"<plist version=\"1.0\"><dict></dict></plist>")
            .expect_err("not an archive");
        assert!(err.0.contains("$archiver"), "{}", err.0);
    }
}
