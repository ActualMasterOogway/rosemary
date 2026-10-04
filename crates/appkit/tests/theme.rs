use std::process::Command;

use objc::{object::Id, sel::Sel};

unsafe fn send(receiver: Id, selector: &str, argument: Id) -> Id {
    let sel = objc::sel::get(selector);
    let class = unsafe { objc::object::class_of(receiver) }.expect("an object");
    let imp = objc::class::method_implementation(class, sel);
    if selector.contains(':') {
        let call: unsafe extern "C" fn(Id, Sel, Id) -> Id = unsafe { std::mem::transmute(imp) };
        unsafe { call(receiver, sel, argument) }
    } else {
        let call: unsafe extern "C" fn(Id, Sel) -> Id = unsafe { std::mem::transmute(imp) };
        unsafe { call(receiver, sel) }
    }
}

fn class(name: &str) -> Id {
    let name = std::ffi::CString::new(name).unwrap();
    objc::class::ptr(objc::class::lookup(&name).unwrap()) as Id
}

unsafe fn string_is(object: Id, expected: Option<&str>) {
    if let Some(expected) = expected {
        assert!(!object.is_null(), "expected {expected}");
        assert_eq!(
            unsafe { foundation::string::bytes(object) },
            expected.as_bytes()
        );
    } else {
        assert!(object.is_null());
    }
}

#[test]
fn all_theme_queries_follow_sdl_including_changes_before_a_window_exists() {
    if std::env::var_os("ROSEMARY_THEME_TEST_CHILD").is_none() {
        let dir = std::env::temp_dir().join(format!("rosemary-theme-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let source = dir.join("sdl.c");
        std::fs::write(
            &source,
            r#"
#include <stdbool.h>
static int initialized, theme;
bool SDL_InitSubSystem(unsigned flags) { initialized = 1; return true; }
int SDL_GetSystemTheme(void) { return initialized ? theme : 0; }
void RosemaryTestTheme(int value) { theme = value; }
"#,
        )
        .unwrap();
        let library = if cfg!(target_os = "macos") {
            "libSDL3.0.dylib"
        } else {
            "libSDL3.so.0"
        };
        let build = Command::new("cc")
            .args(if cfg!(target_os = "macos") {
                vec!["-dynamiclib"]
            } else {
                vec!["-shared", "-fPIC"]
            })
            .arg(&source)
            .arg("-o")
            .arg(dir.join(library))
            .output()
            .expect("C compiler");
        assert!(
            build.status.success(),
            "{}",
            String::from_utf8_lossy(&build.stderr)
        );
        let child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "all_theme_queries_follow_sdl_including_changes_before_a_window_exists",
                "--nocapture",
            ])
            .env("ROSEMARY_THEME_TEST_CHILD", "1")
            .env(
                if cfg!(target_os = "macos") {
                    "DYLD_LIBRARY_PATH"
                } else {
                    "LD_LIBRARY_PATH"
                },
                &dir,
            )
            .output()
            .unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(
            child.status.success(),
            "stdout: {}\nstderr: {}",
            String::from_utf8_lossy(&child.stdout),
            String::from_utf8_lossy(&child.stderr)
        );
        return;
    }
    appkit::bootstrap();
    unsafe {
        let handle = libc_open();
        let pointer = symbol(handle, c"RosemaryTestTheme");
        let theme: unsafe extern "C" fn(i32) = std::mem::transmute(pointer);
        let app = send(
            class("NSApplication"),
            "sharedApplication",
            std::ptr::null_mut(),
        );
        let defaults = send(
            class("NSUserDefaults"),
            "standardUserDefaults",
            std::ptr::null_mut(),
        );
        let key = foundation::string::from_str("AppleInterfaceStyle");
        let global = foundation::string::from_str("NSGlobalDomain");
        let cf_global = corefoundation::string::from_bytes(b"*", false);
        let window = objc::object::create_instance(objc::class::lookup(c"NSWindow").unwrap(), 0);
        let view = objc::object::create_instance(objc::class::lookup(c"NSView").unwrap(), 0);
        appkit::view::set_window(view, window);
        let aqua = foundation::string::from_str(appkit::appearance::AQUA);
        let dark = foundation::string::from_str(appkit::appearance::DARK_AQUA);
        let names = corefoundation::collections::new_array(
            &[aqua.cast(), dark.cast()],
            corefoundation::callbacks::Policy::from_type_callbacks(),
            false,
        );
        // Includes a second dark read after light, catching stale singleton names
        // and preference values that were accidentally persisted or cached.
        for value in [2, 1, 2, 0] {
            theme(value);
            let expected = if value == 2 {
                appkit::appearance::DARK_AQUA
            } else {
                appkit::appearance::AQUA
            };
            let style = (value == 2).then_some("Dark");
            for object in [app, window, view] {
                let appearance = send(object, "effectiveAppearance", std::ptr::null_mut());
                string_is(
                    send(appearance, "name", std::ptr::null_mut()),
                    Some(expected),
                );
                string_is(
                    send(
                        appearance,
                        "bestMatchFromAppearancesWithNames:",
                        names as Id,
                    ),
                    Some(expected),
                );
            }
            for selector in ["currentAppearance", "currentDrawingAppearance"] {
                let appearance = send(class("NSAppearance"), selector, std::ptr::null_mut());
                string_is(
                    send(appearance, "name", std::ptr::null_mut()),
                    Some(expected),
                );
            }
            for selector in [
                "stringForKey:",
                "objectForKey:",
                "valueForKey:",
                "valueForKeyPath:",
            ] {
                string_is(send(defaults, selector, key), style);
            }
            for dictionary in [
                send(defaults, "persistentDomainForName:", global),
                send(defaults, "dictionaryRepresentation", std::ptr::null_mut()),
            ] {
                string_is(send(dictionary, "objectForKey:", key), style);
            }
            let value =
                corefoundation::preferences::CFPreferencesCopyAppValue(key.cast(), cf_global);
            string_is(value as Id, style);
            corefoundation::api::CFRelease(value);
            let value = corefoundation::preferences::CFPreferencesCopyValue(
                key.cast(),
                cf_global,
                std::ptr::null(),
                std::ptr::null(),
            );
            string_is(value as Id, style);
            corefoundation::api::CFRelease(value);
            for all_keys in [true, false] {
                let keys = if all_keys {
                    std::ptr::null()
                } else {
                    corefoundation::collections::new_array(
                        &[key.cast()],
                        corefoundation::callbacks::Policy::from_type_callbacks(),
                        false,
                    )
                };
                let dictionary = corefoundation::preferences::CFPreferencesCopyMultiple(
                    keys,
                    cf_global,
                    std::ptr::null(),
                    std::ptr::null(),
                );
                string_is(send(dictionary as Id, "objectForKey:", key), style);
                corefoundation::api::CFRelease(dictionary);
                corefoundation::api::CFRelease(keys);
            }
            let text = send(class("NSColor"), "textColor", std::ptr::null_mut());
            let background = send(
                class("NSColor"),
                "textBackgroundColor",
                std::ptr::null_mut(),
            );
            assert_eq!(
                appkit::color::components(text).0 > appkit::color::components(background).0,
                style.is_some()
            );
        }
        let named_dark = send(class("NSAppearance"), "appearanceNamed:", dark);
        let named_light = send(class("NSAppearance"), "appearanceNamed:", aqua);
        send(app, "setAppearance:", named_dark);
        assert_eq!(
            send(
                class("NSAppearance"),
                "currentDrawingAppearance",
                std::ptr::null_mut()
            ),
            named_dark
        );
        string_is(
            send(
                send(view, "effectiveAppearance", std::ptr::null_mut()),
                "name",
                std::ptr::null_mut(),
            ),
            Some(appkit::appearance::DARK_AQUA),
        );
        send(window, "setAppearance:", named_light);
        string_is(
            send(
                send(view, "effectiveAppearance", std::ptr::null_mut()),
                "name",
                std::ptr::null_mut(),
            ),
            Some(appkit::appearance::AQUA),
        );
        send(view, "setAppearance:", named_dark);
        assert_eq!(send(view, "appearance", std::ptr::null_mut()), named_dark);
        let child = objc::object::create_instance(objc::class::lookup(c"NSView").unwrap(), 0);
        send(view, "addSubview:", child);
        assert_eq!(
            send(child, "effectiveAppearance", std::ptr::null_mut()),
            named_dark
        );
        send(view, "setAppearance:", std::ptr::null_mut());
        assert_eq!(
            send(child, "effectiveAppearance", std::ptr::null_mut()),
            named_light
        );
        send(class("NSAppearance"), "setCurrentAppearance:", named_dark);
        assert_eq!(
            send(
                class("NSAppearance"),
                "currentDrawingAppearance",
                std::ptr::null_mut()
            ),
            named_dark
        );
        send(
            class("NSAppearance"),
            "setCurrentAppearance:",
            std::ptr::null_mut(),
        );
        send(app, "setAppearance:", std::ptr::null_mut());
    }
}

// Load the same mock library the SDL bindings resolve, without adding a direct
// SDL link dependency to the test or exposing a production theme override.
unsafe fn libc_open() -> *mut std::ffi::c_void {
    unsafe extern "C" {
        fn dlopen(name: *const std::ffi::c_char, flags: i32) -> *mut std::ffi::c_void;
    }
    let name = if cfg!(target_os = "macos") {
        c"libSDL3.0.dylib"
    } else {
        c"libSDL3.so.0"
    };
    let handle = unsafe { dlopen(name.as_ptr(), 1) };
    assert!(!handle.is_null());
    handle
}

unsafe fn symbol(handle: *mut std::ffi::c_void, name: &std::ffi::CStr) -> *mut std::ffi::c_void {
    unsafe extern "C" {
        fn dlsym(
            handle: *mut std::ffi::c_void,
            name: *const std::ffi::c_char,
        ) -> *mut std::ffi::c_void;
    }
    let pointer = unsafe { dlsym(handle, name.as_ptr()) };
    assert!(!pointer.is_null());
    pointer
}
