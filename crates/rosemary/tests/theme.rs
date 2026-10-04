mod common;

use std::process::Command;

const SOURCE: &str = r#"
#import <AppKit/AppKit.h>
#import <CoreFoundation/CoreFoundation.h>
#include <stdio.h>
#include <stdlib.h>

static int check_style(id value, BOOL dark) {
    return dark ? [value isEqualToString:@"Dark"] : value == nil;
}
static int check_appearance(NSAppearance *appearance, NSString *expected) {
    return [appearance.name isEqualToString:expected]
        && [[appearance bestMatchFromAppearancesWithNames:
             @[NSAppearanceNameAqua, NSAppearanceNameDarkAqua]] isEqualToString:expected];
}
int main(int argc, char **argv) {
    @autoreleasepool {
        BOOL dark = argc > 1 && atoi(argv[1]) == 2;
        NSString *expected = dark ? NSAppearanceNameDarkAqua : NSAppearanceNameAqua;
        NSApplication *app = [NSApplication sharedApplication];
        NSWindow *window = [NSWindow alloc];
        NSView *view = [NSView alloc];
        if (!check_appearance(app.effectiveAppearance, expected)) return 1;
        if (!check_appearance(window.effectiveAppearance, expected)) return 2;
        if (!check_appearance(view.effectiveAppearance, expected)) return 3;
        if (!check_appearance([NSAppearance currentAppearance], expected)) return 4;
        if (!check_appearance([NSAppearance currentDrawingAppearance], expected)) return 5;
        NSUserDefaults *defaults = [NSUserDefaults standardUserDefaults];
        if (!check_style([defaults objectForKey:@"AppleInterfaceStyle"], dark)) return 6;
        if (!check_style([defaults stringForKey:@"AppleInterfaceStyle"], dark)) return 7;
        if (!check_style([defaults valueForKey:@"AppleInterfaceStyle"], dark)) return 16;
        if (!check_style([defaults persistentDomainForName:NSGlobalDomain][@"AppleInterfaceStyle"], dark)) return 8;
        if (!check_style(defaults.dictionaryRepresentation[@"AppleInterfaceStyle"], dark)) return 9;
        CFStringRef key = CFSTR("AppleInterfaceStyle");
        CFPropertyListRef value = CFPreferencesCopyAppValue(key, kCFPreferencesCurrentApplication);
        if (!check_style((id)value, dark)) return 10;
        if (value) CFRelease(value);
        value = CFPreferencesCopyValue(key, kCFPreferencesAnyApplication, kCFPreferencesCurrentUser, kCFPreferencesAnyHost);
        if (!check_style((id)value, dark)) return 11;
        if (value) CFRelease(value);
        CFDictionaryRef dictionary = CFPreferencesCopyMultiple(NULL, kCFPreferencesAnyApplication, kCFPreferencesCurrentUser, kCFPreferencesAnyHost);
        if (!check_style((id)CFDictionaryGetValue(dictionary, key), dark)) return 12;
        CFRelease(dictionary);
        NSAppearance *light = [NSAppearance appearanceNamed:NSAppearanceNameAqua];
        NSAppearance *darkAppearance = [NSAppearance appearanceNamed:NSAppearanceNameDarkAqua];
        if (!check_appearance(light, NSAppearanceNameAqua)) return 13;
        if (!check_appearance(darkAppearance, NSAppearanceNameDarkAqua)) return 14;
        __block BOOL ran = NO;
        [darkAppearance performAsCurrentDrawingAppearance:^{
            ran = check_appearance([NSAppearance currentDrawingAppearance], NSAppearanceNameDarkAqua);
        }];
        if (!ran || !check_appearance([NSAppearance currentDrawingAppearance], expected)) return 15;
        puts("theme ok");
    }
    return 0;
}
"#;

#[test]
fn builtin_theme_queries_work_for_native_and_emulated_guests() {
    if !cfg!(target_os = "macos") || !common::can_build_x86_64() {
        eprintln!("skipping: requires macOS and the Mach-O compiler");
        return;
    }
    let emulated = common::build_x86_64(
        "theme",
        SOURCE,
        &[
            "-x",
            "objective-c",
            "-fblocks",
            "-framework",
            "AppKit",
            "-framework",
            "CoreFoundation",
        ],
    )
    .expect("theme fixture");
    let dir = std::path::Path::new(&emulated).parent().unwrap();
    let source = dir.join("theme.c");
    let native = dir.join("theme-native");
    let build = Command::new("cc")
        .args([
            "-x",
            "objective-c",
            "-fblocks",
            "-framework",
            "AppKit",
            "-framework",
            "CoreFoundation",
        ])
        .arg(&source)
        .arg("-o")
        .arg(&native)
        .output()
        .unwrap();
    assert!(
        build.status.success(),
        "{}",
        String::from_utf8_lossy(&build.stderr)
    );
    let mock_dir = dir.join("theme-sdl");
    std::fs::create_dir_all(&mock_dir).unwrap();
    let mock = mock_dir.join("sdl.c");
    std::fs::write(&mock, r#"
#include <stdbool.h>
#include <stdlib.h>
static int initialized;
bool SDL_InitSubSystem(unsigned flags) { initialized = 1; return true; }
int SDL_GetSystemTheme(void) { return initialized ? atoi(getenv("ROSEMARY_TEST_SYSTEM_THEME")) : 0; }
"#).unwrap();
    let build = Command::new("cc")
        .arg("-dynamiclib")
        .arg(&mock)
        .arg("-o")
        .arg(mock_dir.join("libSDL3.0.dylib"))
        .output()
        .unwrap();
    assert!(
        build.status.success(),
        "{}",
        String::from_utf8_lossy(&build.stderr)
    );
    for (backend, path) in [
        ("--emulated", std::path::Path::new(&emulated)),
        ("--native", native.as_path()),
    ] {
        for theme in ["2", "1", "0"] {
            let out = common::driver_command()
                .args(["run", backend])
                .arg(path)
                .arg(theme)
                .env("ROSEMARY_OBJC", "builtin")
                .env("ROSEMARY_BUNDLED", "1")
                .env("ROSEMARY_TEST_SYSTEM_THEME", theme)
                .env("DYLD_LIBRARY_PATH", &mock_dir)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{backend} theme={theme}: {}\n{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
            assert_eq!(String::from_utf8_lossy(&out.stdout), "theme ok\n");
        }
    }
}
