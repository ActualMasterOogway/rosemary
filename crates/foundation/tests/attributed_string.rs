use corefoundation::callbacks::Policy;
use foundation::geometry::NSRange;
use objc::{object::Id, sel::Sel};

struct Owned(Id);

impl Drop for Owned {
    fn drop(&mut self) {
        objc::arc::release(self.0);
    }
}

struct Pool(*mut std::ffi::c_void);

impl Pool {
    fn new() -> Self {
        foundation::bootstrap();
        Self(objc::arc::pool_push())
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        objc::arc::pool_pop(self.0);
    }
}

macro_rules! send {
    ($receiver:expr, $name:literal $(, $arg:expr => $ty:ty)* ; $ret:ty) => {{
        // SAFETY: each invocation uses the declared Objective-C method signature.
        unsafe {
            let call: unsafe extern "C" fn(Id, Sel $(, $ty)*) -> $ret =
                std::mem::transmute(objc::msg::entry());
            call($receiver, objc::sel::get($name) $(, $arg)*)
        }
    }};
}

fn range(location: usize, length: usize) -> NSRange {
    NSRange { location, length }
}

fn ns(text: &str) -> Owned {
    Owned(foundation::string::from_str(text))
}

fn mutable(text: &str) -> Owned {
    let class = objc::class::lookup(c"NSMutableAttributedString").unwrap();
    let object = objc::object::create_instance(class, 0);
    let text = ns(text);
    Owned(send!(object, "initWithString:", text.0 => Id; Id))
}

fn units(object: Id) -> Vec<u16> {
    // SAFETY: callers pass live NSStrings.
    unsafe { corefoundation::string::as_string(object.cast()) }
        .unwrap()
        .units()
}

fn text(object: Id) -> String {
    String::from_utf16_lossy(&units(send!(object, "string"; Id)))
}

fn length(object: Id) -> usize {
    send!(object, "length"; usize)
}

fn add(object: Id, name: Id, value: Id, at: NSRange) {
    send!(object, "addAttribute:value:range:", name => Id, value => Id, at => NSRange; ());
}

fn attribute(object: Id, name: Id, index: usize) -> (Id, NSRange) {
    let mut effective = range(0, 0);
    let value = send!(object, "attribute:atIndex:effectiveRange:", name => Id, index => usize, &mut effective => *mut NSRange; Id);
    (value, effective)
}

fn dictionary(pairs: &[(Id, Id)], mutable: bool) -> Owned {
    let pairs: Vec<_> = pairs
        .iter()
        .map(|(k, v)| (k.cast_const(), v.cast_const()))
        .collect();
    Owned(corefoundation::collections::new_dictionary(
        &pairs,
        Policy::from_key_callbacks(),
        Policy::from_type_callbacks(),
        mutable,
    ) as Id)
}

#[test]
fn length_and_ranges_count_utf16_including_unpaired_surrogates() {
    let _pool = Pool::new();
    let string = mutable("A😀é");
    assert_eq!(length(string.0), 4);
    let sub = send!(string.0, "attributedSubstringFromRange:", range(1, 2) => NSRange; Id);
    assert_eq!(text(sub), "😀");

    let raw = Owned(corefoundation::string::from_utf16(&[0xd800, b'x' as u16]) as Id);
    let class = objc::class::lookup(c"NSAttributedString").unwrap();
    let object = objc::object::create_instance(class, 0);
    let object = Owned(send!(object, "initWithString:", raw.0 => Id; Id));
    assert_eq!(length(object.0), 2);
    assert_eq!(units(send!(object.0, "string"; Id)), [0xd800, b'x' as u16]);
    let proxy = send!(string.0, "mutableString"; Id);
    send!(proxy, "replaceCharactersInRange:withString:", range(1, 1) => NSRange, raw.0 => Id; ());
    assert_eq!(length(string.0), 5);
    assert_eq!(
        units(send!(string.0, "string"; Id)),
        [65, 0xd800, 120, 0xde00, 233]
    );
}

#[test]
fn new_and_inherited_factory_create_usable_mutable_instances() {
    let _pool = Pool::new();
    let class = objc::class::ptr(objc::class::lookup(c"NSMutableAttributedString").unwrap()) as Id;
    let string = Owned(send!(class, "new"; Id));
    assert_eq!(length(string.0), 0);
    let appended = mutable("hello");
    send!(string.0, "beginEditing"; ());
    send!(string.0, "appendAttributedString:", appended.0 => Id; ());
    send!(string.0, "endEditing"; ());
    assert_eq!(text(string.0), "hello");
    let value = ns("factory");
    let factory = send!(class, "attributedStringWithString:", value.0 => Id; Id);
    // SAFETY: a live object returned by the class factory.
    assert!(unsafe {
        corefoundation::base::is_kind_of(
            factory.cast(),
            objc::class::lookup(c"NSMutableAttributedString").unwrap(),
        )
    });
    send!(factory, "deleteCharactersInRange:", range(0, 3) => NSRange; ());
    assert_eq!(text(factory), "tory");
}

#[test]
fn attributes_are_snapshotted_and_edits_preserve_other_ranges() {
    let _pool = Pool::new();
    let (font, color, bold, red, blue) =
        (ns("font"), ns("color"), ns("bold"), ns("red"), ns("blue"));
    let attributes = dictionary(&[(font.0, bold.0)], true);
    let class = objc::class::lookup(c"NSMutableAttributedString").unwrap();
    let object = objc::object::create_instance(class, 0);
    let value = ns("abcde");
    let string =
        Owned(send!(object, "initWithString:attributes:", value.0 => Id, attributes.0 => Id; Id));
    send!(attributes.0, "setObject:forKey:", blue.0 => Id, font.0 => Id; ());
    assert_eq!(attribute(string.0, font.0, 0).0, bold.0);

    let attrs = dictionary(&[(color.0, red.0)], false);
    send!(string.0, "addAttributes:range:", attrs.0 => Id, range(1, 3) => NSRange; ());
    assert!(attribute(string.0, color.0, 0).0.is_null());
    assert_eq!(attribute(string.0, color.0, 2), (red.0, range(1, 3)));
    add(string.0, color.0, blue.0, range(2, 1));
    assert_eq!(attribute(string.0, color.0, 2), (blue.0, range(2, 1)));
    assert_eq!(attribute(string.0, font.0, 2), (bold.0, range(0, 5)));

    let mut effective = range(0, 0);
    let longest = send!(string.0, "attribute:atIndex:longestEffectiveRange:inRange:", font.0 => Id, 2 => usize, &mut effective => *mut NSRange, range(1, 3) => NSRange; Id);
    assert_eq!(longest, bold.0);
    assert_eq!(effective, range(1, 3));
    let dict = send!(string.0, "attributesAtIndex:effectiveRange:", 2 => usize, &mut effective => *mut NSRange; Id);
    assert_eq!(send!(dict, "count"; usize), 2);
    assert_eq!(effective, range(2, 1));
    send!(string.0, "removeAttribute:range:", color.0 => Id, range(1, 3) => NSRange; ());
    send!(string.0, "attributesAtIndex:longestEffectiveRange:inRange:", 2 => usize, &mut effective => *mut NSRange, range(1, 3) => NSRange; Id);
    assert_eq!(effective, range(1, 3));
    send!(string.0, "setAttributes:range:", attrs.0 => Id, range(2, 1) => NSRange; ());
    assert_eq!(attribute(string.0, color.0, 2).0, red.0);
    assert!(attribute(string.0, font.0, 2).0.is_null());
    assert_eq!(attribute(string.0, font.0, 1).0, bold.0);
    send!(string.0, "setAttributes:range:", std::ptr::null_mut() => Id, range(1, 3) => NSRange; ());
    assert!(attribute(string.0, font.0, 2).0.is_null());
    assert_eq!(attribute(string.0, font.0, 4).0, bold.0);
    let dict = send!(string.0, "attributesAtIndex:effectiveRange:", 2 => usize, std::ptr::null_mut() => *mut NSRange; Id);
    assert!(!dict.is_null());
    assert_eq!(send!(dict, "count"; usize), 0);
    add(string.0, font.0, bold.0, range(5, 0));
    assert_eq!(length(string.0), 5);
}

#[test]
fn plain_replacements_and_insertions_inherit_adjacent_attributes() {
    let _pool = Pool::new();
    let string = mutable("abc");
    let (key, first, second) = (ns("style"), ns("first"), ns("second"));
    add(string.0, key.0, first.0, range(0, 1));
    add(string.0, key.0, second.0, range(1, 2));
    let value = ns("XY");
    send!(string.0, "replaceCharactersInRange:withString:", range(0, 2) => NSRange, value.0 => Id; ());
    assert_eq!(text(string.0), "XYc");
    assert_eq!(attribute(string.0, key.0, 1).0, first.0);
    assert_eq!(attribute(string.0, key.0, 2).0, second.0);
    let value = ns("!");
    for index in [0, 3, 5] {
        send!(string.0, "replaceCharactersInRange:withString:", range(index, 0) => NSRange, value.0 => Id; ());
    }
    assert_eq!(text(string.0), "!XY!c!");
    assert_eq!(attribute(string.0, key.0, 0).0, first.0);
    assert_eq!(attribute(string.0, key.0, 3).0, first.0);
    assert_eq!(attribute(string.0, key.0, 5).0, second.0);
    send!(string.0, "deleteCharactersInRange:", range(1, 4) => NSRange; ());
    assert_eq!(text(string.0), "!!");
    assert_eq!(attribute(string.0, key.0, 1).0, second.0);
}

#[test]
fn attributed_edits_copies_and_substrings_preserve_runs() {
    let _pool = Pool::new();
    let string = mutable("ab");
    let other = mutable("XY");
    let (key, value) = (ns("style"), ns("special"));
    add(other.0, key.0, value.0, range(1, 1));
    send!(string.0, "insertAttributedString:atIndex:", other.0 => Id, 1 => usize; ());
    assert_eq!(text(string.0), "aXYb");
    assert!(attribute(string.0, key.0, 1).0.is_null());
    assert_eq!(attribute(string.0, key.0, 2).0, value.0);
    send!(string.0, "replaceCharactersInRange:withAttributedString:", range(0, 1) => NSRange, other.0 => Id; ());
    assert_eq!(text(string.0), "XYXYb");
    send!(string.0, "appendAttributedString:", string.0 => Id; ());
    assert_eq!(text(string.0), "XYXYbXYXYb");
    send!(string.0, "setAttributedString:", string.0 => Id; ());
    let copy =
        Owned(send!(string.0, "copyWithZone:", std::ptr::null_mut() => *mut std::ffi::c_void; Id));
    let mutable_copy = Owned(
        send!(copy.0, "mutableCopyWithZone:", std::ptr::null_mut() => *mut std::ffi::c_void; Id),
    );
    let sub = send!(copy.0, "attributedSubstringFromRange:", range(1, 2) => NSRange; Id);
    assert_eq!(text(sub), "YX");
    assert_eq!(attribute(sub, key.0, 0).0, value.0);
    assert!(attribute(sub, key.0, 1).0.is_null());
    let class = objc::class::lookup(c"NSAttributedString").unwrap();
    let initialized = Owned(
        send!(objc::object::create_instance(class, 0), "initWithAttributedString:", string.0 => Id; Id),
    );
    send!(string.0, "setAttributedString:", other.0 => Id; ());
    send!(mutable_copy.0, "deleteCharactersInRange:", range(0, 5) => NSRange; ());
    assert_eq!(text(copy.0), "XYXYbXYXYb");
    assert_eq!(text(initialized.0), text(copy.0));
    assert_eq!(text(mutable_copy.0), "XYXYb");
    assert_eq!(text(string.0), "XY");
    assert_eq!(attribute(copy.0, key.0, 8).0, value.0);
    assert!(!send!(copy.0, "attributedSubstringFromRange:", range(10, 0) => NSRange; Id).is_null());
}

#[test]
fn attribute_values_live_until_their_last_run_or_copy_is_removed() {
    let _pool = Pool::new();
    let string = mutable("abc");
    let (key, value) = (ns("style"), ns("owned"));
    add(string.0, key.0, value.0, range(1, 1));
    assert_eq!(objc::arc::retain_count(value.0), 2);
    let copy = Owned(send!(string.0, "copy"; Id));
    send!(string.0, "deleteCharactersInRange:", range(1, 1) => NSRange; ());
    assert_eq!(objc::arc::retain_count(value.0), 2);
    drop(copy);
    assert_eq!(objc::arc::retain_count(value.0), 1);
    add(string.0, key.0, value.0, range(0, 2));
    send!(string.0, "removeAttribute:range:", key.0 => Id, range(0, 2) => NSRange; ());
    assert_eq!(objc::arc::retain_count(value.0), 1);
}

#[test]
fn script_methods_set_superscript_attributes_over_the_requested_range() {
    let _pool = Pool::new();
    let string = mutable("abc");
    let key = ns("NSSuperscript");
    send!(string.0, "superscriptRange:", range(0, 1) => NSRange; ());
    send!(string.0, "subscriptRange:", range(1, 1) => NSRange; ());
    send!(string.0, "unscriptRange:", range(2, 1) => NSRange; ());
    for (index, expected) in [1, -1, 0].into_iter().enumerate() {
        let (value, effective) = attribute(string.0, key.0, index);
        assert_eq!(send!(value, "longLongValue"; i64), expected);
        assert_eq!(effective, range(index, 1));
    }
}

#[test]
fn mutable_string_updates_parent_and_preserves_proxy_identity() {
    let _pool = Pool::new();
    let string = mutable("ab");
    let (key, value) = (ns("style"), ns("special"));
    add(string.0, key.0, value.0, range(1, 1));
    let proxy = send!(string.0, "mutableString"; Id);
    let replacement = ns("!!");
    send!(proxy, "insertString:atIndex:", replacement.0 => Id, 1 => usize; ());
    assert_eq!(text(string.0), "a!!b");
    assert_eq!(attribute(string.0, key.0, 3).0, value.0);
    let needle = ns("!");
    let replacement = ns("?");
    assert_eq!(
        send!(proxy, "replaceOccurrencesOfString:withString:options:range:", needle.0 => Id, replacement.0 => Id, 0 => usize, range(0, 4) => NSRange; usize),
        2
    );
    assert_eq!(text(string.0), "a??b");
    assert_eq!(attribute(string.0, key.0, 3).0, value.0);
    let other = mutable("new");
    send!(string.0, "setAttributedString:", other.0 => Id; ());
    assert_eq!(send!(string.0, "mutableString"; Id), proxy);
    assert_eq!(units(proxy), [110, 101, 119]);
    let copy = Owned(send!(proxy, "copy"; Id));
    let value = ns(" tail");
    send!(proxy, "appendString:", value.0 => Id; ());
    assert_eq!(text(string.0), "new tail");
    assert_eq!(units(copy.0), [110, 101, 119]);
    send!(proxy, "setString:", replacement.0 => Id; ());
    assert_eq!(text(string.0), "?");
    send!(proxy, "deleteCharactersInRange:", range(0, 1) => NSRange; ());
    assert_eq!(length(string.0), 0);
}

#[test]
fn retained_mutable_string_outlives_parent_under_objc_debug() {
    const CHILD: &str = "ROSEMARY_ATTRIBUTED_STRING_LIFETIME_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "retained_mutable_string_outlives_parent_under_objc_debug",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    objc::debug::enable();
    let _pool = Pool::new();
    let string = mutable("alive");
    let proxy = Owned(objc::arc::retain(send!(string.0, "mutableString"; Id)));
    drop(string);
    let value = ns("!");
    send!(proxy.0, "appendString:", value.0 => Id; ());
    assert_eq!(String::from_utf16_lossy(&units(proxy.0)), "alive!");
    let live = mutable("parent");
    let proxy = send!(live.0, "mutableString"; Id);
    send!(proxy, "appendString:", value.0 => Id; ());
    assert_eq!(text(live.0), "parent!");
}

#[test]
fn invalid_and_overflowing_ranges_fail_by_name() {
    const CHILD: &str = "ROSEMARY_ATTRIBUTED_STRING_RANGE_CHILD";
    if let Ok(case) = std::env::var(CHILD) {
        let _pool = Pool::new();
        let string = mutable("abc");
        match case.as_str() {
            "delete" => send!(string.0, "deleteCharactersInRange:", range(3, 1) => NSRange; ()),
            "overflow" => {
                send!(string.0, "deleteCharactersInRange:", range(1, usize::MAX) => NSRange; ())
            }
            "attributes" => {
                send!(string.0, "setAttributes:range:", std::ptr::null_mut() => Id, range(4, 0) => NSRange; ())
            }
            "index" => {
                send!(string.0, "attributesAtIndex:effectiveRange:", 3 => usize, std::ptr::null_mut() => *mut NSRange; Id);
            }
            _ => unreachable!(),
        }
        panic!("invalid range was accepted");
    }
    for case in ["delete", "overflow", "attributes", "index"] {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "invalid_and_overflowing_ranges_fail_by_name",
                "--nocapture",
            ])
            .env(CHILD, case)
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("NSRangeException"),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
