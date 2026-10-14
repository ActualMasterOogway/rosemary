use corefoundation::plist::Value;
use objc::{object::Id, sel::Sel};

fn imp(receiver: Id, selector: &str) -> (usize, Sel) {
    let sel = objc::sel::get(selector);
    // SAFETY: callers supply live objects.
    let class = unsafe { objc::object::class_of(receiver) }.expect("an object");
    (objc::class::method_implementation(class, sel) as usize, sel)
}

unsafe fn object(receiver: Id, selector: &str, a: Id) -> Id {
    let (f, sel) = imp(receiver, selector);
    unsafe {
        std::mem::transmute::<usize, unsafe extern "C" fn(Id, Sel, Id) -> Id>(f)(receiver, sel, a)
    }
}

unsafe fn set(receiver: Id, value: Id, key: &str) {
    let (f, sel) = imp(receiver, "setObject:forKey:");
    let key = foundation::string::from_str(key);
    unsafe {
        std::mem::transmute::<usize, unsafe extern "C" fn(Id, Sel, Id, Id)>(f)(
            receiver, sel, value, key,
        )
    }
}

unsafe fn set_bool(receiver: Id, value: bool, key: &str) {
    let (f, sel) = imp(receiver, "setBool:forKey:");
    let key = foundation::string::from_str(key);
    unsafe {
        std::mem::transmute::<usize, unsafe extern "C" fn(Id, Sel, u8, Id)>(f)(
            receiver,
            sel,
            value as u8,
            key,
        )
    }
}

unsafe fn text(receiver: Id, key: &str) -> Option<String> {
    let key = foundation::string::from_str(key);
    let result = unsafe { object(receiver, "stringForKey:", key) };
    (!result.is_null()).then(|| {
        String::from_utf8_lossy(&unsafe { foundation::string::bytes(result) }).into_owned()
    })
}

/// One process can stand in for two applications by changing argv[0], since
/// the domain is the running program's.
#[test]
fn applications_do_not_see_each_others_preferences() {
    let home = std::env::temp_dir().join(format!("rosemary-domains-{}", std::process::id()));
    std::fs::create_dir_all(&home).unwrap();
    // SAFETY: this process has one test.
    unsafe { std::env::set_var("HOME", &home) };
    foundation::bootstrap();
    let as_app =
        |name: &str| foundation::processinfo::set_arguments(&[format!("/Apps/{name}").into()]);
    // SAFETY: registered classes and methods, with NSString arguments.
    unsafe {
        let class = objc::class::lookup(c"NSUserDefaults").unwrap();
        let defaults = object(
            objc::class::ptr(class) as Id,
            "standardUserDefaults",
            std::ptr::null_mut(),
        );

        as_app("One");
        set(defaults, foundation::string::from_str("one"), "Name");
        set_bool(defaults, true, "Flag");
        as_app("Two");
        assert_eq!(text(defaults, "Name"), None, "another application's key");
        set(defaults, foundation::string::from_str("two"), "Name");
        as_app("One");
        assert_eq!(text(defaults, "Name").as_deref(), Some("one"));
        assert_eq!(text(defaults, "Flag").as_deref(), Some("1"));

        // The global domain is searched by everyone.
        corefoundation::preferences::set_value(
            corefoundation::preferences::GLOBAL,
            "Shared",
            Some(Value::String("everyone".into())),
        );
        as_app("Two");
        assert_eq!(text(defaults, "Shared").as_deref(), Some("everyone"));

        // A suite is its own domain, and the same one from either application.
        let alloc = object(objc::class::ptr(class) as Id, "alloc", std::ptr::null_mut());
        let suite = object(
            alloc,
            "initWithSuiteName:",
            foundation::string::from_str("group.example"),
        );
        set(suite, foundation::string::from_str("grouped"), "Name");
        assert_eq!(text(suite, "Name").as_deref(), Some("grouped"));
        assert_eq!(text(defaults, "Name").as_deref(), Some("two"));
    }
    let preferences = home.join("Library/Preferences");
    let read = |name: &str| {
        corefoundation::plist::parse(&std::fs::read(preferences.join(name)).unwrap()).unwrap()
    };
    assert_eq!(read("One.plist").get("Flag"), Some(&Value::Bool(true)));
    assert_eq!(
        read("Two.plist").get("Name"),
        Some(&Value::String("two".into()))
    );
    assert_eq!(
        read("group.example.plist").get("Name"),
        Some(&Value::String("grouped".into()))
    );
    std::fs::remove_dir_all(home).unwrap();
}
