use objc::class::Class;
use objc::object::Id;
use objc::sel::Sel;

const CLASSES: &[&std::ffi::CStr] = &[
    c"WebPreferences",
    c"WKWebViewConfiguration",
    c"WKProcessPool",
    c"WKUserContentController",
    c"WKWebsiteDataStore",
];

pub fn bootstrap() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        foundation::bootstrap();
        for name in CLASSES {
            corefoundation::base::abstract_class(name, None);
        }
        let class = objc::class::lookup(c"WebPreferences").expect("registered above");
        class.metaclass().replace_method(
            objc::sel::get("standardPreferences"),
            standard_preferences as *const std::ffi::c_void,
            c"@16@0:8",
        );
        class.replace_method(
            objc::sel::get("setPlugInsEnabled:"),
            set_plugins_enabled as *const std::ffi::c_void,
            c"v20@0:8c16",
        );

        let configuration = objc::class::lookup(c"WKWebViewConfiguration").expect("registered");
        for selector in [
            "setProcessPool:",
            "setUserContentController:",
            "setApplicationNameForUserAgent:",
        ] {
            configuration.replace_method(
                objc::sel::get(selector),
                accept_object_setting as *const std::ffi::c_void,
                c"v24@0:8@16",
            );
        }

        let controller = objc::class::lookup(c"WKUserContentController").expect("registered");
        controller.replace_method(
            objc::sel::get("removeAllUserScripts"),
            remove_all_user_scripts as *const std::ffi::c_void,
            c"v16@0:8",
        );

        let store = objc::class::lookup(c"WKWebsiteDataStore").expect("registered");
        store.metaclass().replace_method(
            objc::sel::get("defaultDataStore"),
            default_data_store as *const std::ffi::c_void,
            c"@16@0:8",
        );
    });
}

/// `+[WebPreferences standardPreferences]`.
unsafe extern "C" fn standard_preferences(class: Id, _sel: Sel) -> Id {
    static STANDARD: std::sync::OnceLock<usize> = std::sync::OnceLock::new();

    *STANDARD.get_or_init(|| {
        // SAFETY: a class-method receiver is a class registered with this runtime.
        let class = unsafe { objc::class::from_ptr(class as *const Class) }
            .expect("standardPreferences receiver is a class");
        let preferences = objc::object::create_instance(class, 0);
        // The standard preferences are process-global. Keep a reference so a
        // caller releasing its non-owned result cannot make the singleton dangle.
        objc::arc::retain(preferences);
        preferences as usize
    }) as Id
}

/// `-[WebPreferences setPlugInsEnabled:]`.
///
/// Web content is not implemented, so there is no plug-in subsystem to
/// configure. Accepting the setting lets applications finish configuring the
/// shared preferences object.
unsafe extern "C" fn set_plugins_enabled(_receiver: Id, _sel: Sel, _enabled: u8) {}

unsafe extern "C" fn accept_object_setting(_receiver: Id, _sel: Sel, _value: Id) {}

unsafe extern "C" fn remove_all_user_scripts(_receiver: Id, _sel: Sel) {}

unsafe extern "C" fn default_data_store(class: Id, _sel: Sel) -> Id {
    static DEFAULT: std::sync::OnceLock<usize> = std::sync::OnceLock::new();

    *DEFAULT.get_or_init(|| {
        // SAFETY: this is the registered `WKWebsiteDataStore` class object.
        let class = unsafe { objc::class::from_ptr(class as *const Class) }.expect("a class");
        let store = objc::object::create_instance(class, 0);
        objc::arc::retain(store);
        store as usize
    }) as Id
}

/// The Objective-C class objects exported by WebKit.framework.
pub fn data_symbols() -> Vec<(String, u64)> {
    bootstrap();
    let mut symbols = Vec::new();
    for name in CLASSES {
        let class = objc::class::lookup(name).expect("registered during bootstrap");
        let name = name.to_string_lossy();
        symbols.push((
            format!("_OBJC_CLASS_$_{name}"),
            objc::class::ptr(class) as usize as u64,
        ));
        symbols.push((
            format!("_OBJC_METACLASS_$_{name}"),
            objc::class::ptr(class.metaclass()) as usize as u64,
        ));
    }
    let cookies = foundation::notification::name_object("WKWebsiteDataTypeCookies");
    symbols.push((
        "_WKWebsiteDataTypeCookies".to_string(),
        corefoundation::base::global_cell("_WKWebsiteDataTypeCookies", cookies),
    ));
    symbols
}

pub fn symbol(name: &str) -> Option<u64> {
    let wanted = name.trim_start_matches('_');
    data_symbols()
        .into_iter()
        .find(|(symbol, _)| symbol.trim_start_matches('_') == wanted)
        .map(|(_, address)| address)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn standard() -> Id {
        bootstrap();
        let class = objc::class::lookup(c"WebPreferences").expect("WebPreferences");
        let method = class
            .metaclass()
            .method(objc::sel::get("standardPreferences"))
            .expect("class method");
        let implementation: unsafe extern "C" fn(Id, Sel) -> Id =
            unsafe { std::mem::transmute(method.implementation()) };
        // SAFETY: the method's encoding and implementation have this signature.
        unsafe {
            implementation(
                objc::class::ptr(class) as Id,
                objc::sel::get("standardPreferences"),
            )
        }
    }

    #[test]
    fn standard_preferences_is_a_stable_instance() {
        let first = standard();
        let second = standard();
        assert!(!first.is_null());
        assert_eq!(first, second);
        let class = unsafe { objc::object::class_of(first) }.expect("an Objective-C object");
        assert_eq!(class.name_str(), "WebPreferences");
    }

    #[test]
    fn accepts_enabling_and_disabling_plugins() {
        let preferences = standard();
        let class = unsafe { objc::object::class_of(preferences) }.expect("WebPreferences");
        let method = class
            .method(objc::sel::get("setPlugInsEnabled:"))
            .expect("instance method");
        let implementation: unsafe extern "C" fn(Id, Sel, u8) =
            unsafe { std::mem::transmute(method.implementation()) };
        // SAFETY: the method's encoding and implementation have this signature.
        unsafe {
            implementation(preferences, objc::sel::get("setPlugInsEnabled:"), 1);
            implementation(preferences, objc::sel::get("setPlugInsEnabled:"), 0);
        }
    }

    #[test]
    fn exports_class_and_metaclass() {
        let symbols = data_symbols();
        assert!(
            symbols
                .iter()
                .any(|(name, _)| name == "_OBJC_CLASS_$_WebPreferences")
        );
        assert!(
            symbols
                .iter()
                .any(|(name, _)| name == "_OBJC_METACLASS_$_WebPreferences")
        );
        assert!(symbols.iter().all(|(_, address)| *address != 0));
        let (_, cookie_cell) = symbols
            .iter()
            .find(|(name, _)| name == "_WKWebsiteDataTypeCookies")
            .expect("cookie data type");
        // SAFETY: data symbols publish the address of a process-lifetime cell.
        assert_ne!(unsafe { *(*cookie_cell as *const u64) }, 0);
    }

    #[test]
    fn default_website_data_store_is_stable() {
        bootstrap();
        let class = objc::class::lookup(c"WKWebsiteDataStore").expect("registered");
        // SAFETY: the method has no explicit arguments and returns an object.
        unsafe {
            let first = default_data_store(
                objc::class::ptr(class) as Id,
                objc::sel::get("defaultDataStore"),
            );
            let second = default_data_store(
                objc::class::ptr(class) as Id,
                objc::sel::get("defaultDataStore"),
            );
            assert!(!first.is_null());
            assert_eq!(first, second);
        }
    }
}
