use objc::class::Class;
use objc::object::Id;
use objc::sel::Sel;

pub fn bootstrap() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        foundation::bootstrap();
        let class = corefoundation::base::abstract_class(c"WebPreferences", None);
        class.metaclass().replace_method(
            objc::sel::get("standardPreferences"),
            standard_preferences as *const std::ffi::c_void,
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

/// The Objective-C class objects exported by WebKit.framework.
pub fn data_symbols() -> Vec<(String, u64)> {
    bootstrap();
    let class = objc::class::lookup(c"WebPreferences").expect("WebPreferences is registered");
    vec![
        (
            "_OBJC_CLASS_$_WebPreferences".to_string(),
            objc::class::ptr(class) as usize as u64,
        ),
        (
            "_OBJC_METACLASS_$_WebPreferences".to_string(),
            objc::class::ptr(class.metaclass()) as usize as u64,
        ),
    ]
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
    }
}
