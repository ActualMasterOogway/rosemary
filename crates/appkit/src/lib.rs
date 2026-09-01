pub mod alert;
pub mod application;
pub mod classes;
pub mod color;
pub mod cursor;
pub(crate) mod decode;
mod define;
pub mod event;
pub mod geometry;
pub mod icon;
pub mod image;
pub mod keymap;
pub mod menu;
pub mod metadata;
pub mod nib;
pub mod nsevent;
pub mod opengl;
pub mod raster;
pub mod screen;
pub mod textinput;
pub mod view;
pub mod window;

pub fn bootstrap() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        foundation::bootstrap();
        classes::install();
        alert::install();
        application::install();
        color::install();
        cursor::install();
        image::install();
        image::bitmap::install();
        menu::install();
        screen::install();
        window::install();
        view::install();
        opengl::install();
        nsevent::install();
    });
}

macro_rules! at {
    ($f:expr) => {
        $f as *const () as usize as u64
    };
}

pub fn symbol(name: &str) -> Option<u64> {
    bootstrap();
    Some(match name {
        "NSApplicationMain" => at!(application::NSApplicationMain),
        "CGMainDisplayID" => at!(screen::CGMainDisplayID),
        "TISCopyCurrentKeyboardInputSource" => at!(textinput::TISCopyCurrentKeyboardInputSource),
        "TISCopyCurrentKeyboardLayoutInputSource" => {
            at!(textinput::TISCopyCurrentKeyboardLayoutInputSource)
        }
        "TISGetInputSourceProperty" => at!(textinput::TISGetInputSourceProperty),
        "LMGetKbdType" => at!(textinput::LMGetKbdType),
        "UCKeyTranslate" => at!(textinput::UCKeyTranslate),
        _ => return None,
    })
}

pub const NAMES: &[&str] = &[
    "NSApplicationMain",
    "CGMainDisplayID",
    "TISCopyCurrentKeyboardInputSource",
    "TISCopyCurrentKeyboardLayoutInputSource",
    "TISGetInputSourceProperty",
    "LMGetKbdType",
    "UCKeyTranslate",
];

pub fn ns_app_cell() -> u64 {
    corefoundation::base::global_cell("_NSApp", 0)
}

pub fn data_symbols() -> Vec<(String, u64)> {
    bootstrap();
    let mut out = classes::class_data();
    out.push(("_NSApp".to_string(), ns_app_cell()));
    for name in NOTIFICATION_NAMES {
        let symbol = format!("_{name}");
        let object = foundation::notification::name_object(name);
        out.push((
            symbol.clone(),
            corefoundation::base::global_cell(&symbol, object),
        ));
    }
    out.push((
        "_NSAppKitVersionNumber".to_string(),
        corefoundation::base::global_cell("_NSAppKitVersionNumber", APPKIT_VERSION.to_bits()),
    ));
    for (symbol, text) in textinput::STRING_GLOBALS {
        let object = foundation::notification::name_object(text);
        out.push((
            symbol.to_string(),
            corefoundation::base::global_cell(symbol, object),
        ));
    }
    for (symbol, text) in STRING_GLOBALS {
        let object = foundation::notification::name_object(text);
        out.push((
            symbol.to_string(),
            corefoundation::base::global_cell(symbol, object),
        ));
    }
    out
}

/// The `NSNotificationName` globals a program compares against.
///
/// The window's are here as well as the application's, and some of them are not
/// posted yet - `NSWindowDidResizeNotification` waits on the event pump. They
/// are published anyway, and the reason is the difference between the two ways a
/// symbol can be missing: an unresolved *function* import is refused by name at
/// the call, while an unresolved **data** import binds to zero and the guest's
/// next instruction dereferences it. A name nothing posts is a gap; a name
/// nothing defines is a segfault at address zero.
pub const NOTIFICATION_NAMES: &[&str] = &[
    application::WILL_FINISH_LAUNCHING,
    application::DID_FINISH_LAUNCHING,
    application::WILL_TERMINATE,
    application::DID_BECOME_ACTIVE,
    application::WILL_RESIGN_ACTIVE,
    window::DID_RESIZE,
    window::DID_MOVE,
    window::WILL_CLOSE,
    window::DID_BECOME_KEY,
    window::DID_RESIGN_KEY,
    window::DID_MINIATURIZE,
    window::DID_DEMINIATURIZE,
    window::WILL_ENTER_FULL_SCREEN,
    window::WILL_EXIT_FULL_SCREEN,
    window::DID_ENTER_FULL_SCREEN,
    window::DID_EXIT_FULL_SCREEN,
    window::DID_CHANGE_BACKING_PROPERTIES,
    application::DID_CHANGE_SCREEN_PARAMETERS,
    view::FRAME_DID_CHANGE,
    view::BOUNDS_DID_CHANGE,
];

/// The other AppKit string globals a program reads.
///
/// Each is a fixed name rather than something this runtime chooses: a colour
/// space and an attribute key are compared against by string, so publishing the
/// documented value is exact.
pub const STRING_GLOBALS: &[(&str, &str)] = &[
    ("_NSCalibratedRGBColorSpace", "NSCalibratedRGBColorSpace"),
    ("_NSDeviceRGBColorSpace", "NSDeviceRGBColorSpace"),
    ("_NSDeviceResolution", "NSDeviceResolution"),
    ("_NSForegroundColorAttributeName", "NSColor"),
    ("_NSAppearanceNameAqua", "NSAppearanceNameAqua"),
    ("_NSDeviceSize", "NSDeviceSize"),
    ("_NSModalPanelRunLoopMode", "NSModalPanelRunLoopMode"),
    // The pasteboard's plain-text type, under both its spellings: the modern one
    // is a uniform type identifier and the deprecated one is not, and a program
    // that writes with one and reads with the other has to name the same type.
    ("_NSPasteboardTypeString", "public.utf8-plain-text"),
    ("_NSStringPboardType", "public.utf8-plain-text"),
];

/// `NSAppKitVersionNumber`, which is a `double` variable rather than a pointer.
///
/// 2022 is Big Sur's, chosen for the reason `kCFCoreFoundationVersionNumber` is
/// 1600: it is the floor every version test in a program of this vintage asks
/// about, and claiming a later one would have a guest take a path against an
/// AppKit this tree does not implement.
const APPKIT_VERSION: f64 = 2022.0;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_listed_name_resolves() {
        let missing: Vec<&str> = NAMES
            .iter()
            .copied()
            .filter(|n| symbol(n).is_none())
            .collect();
        assert!(missing.is_empty(), "listed but unresolved: {missing:?}");
    }

    /// A data symbol is the address of a word holding the value, never the value
    /// itself: the guest loads through it. The check is that reading the cell
    /// gives back an object, which is what a guest's second load does.
    #[test]
    fn a_notification_name_global_is_a_cell_holding_the_string() {
        let symbols = data_symbols();
        let (_, cell) = symbols
            .iter()
            .find(|(n, _)| n == "_NSApplicationDidFinishLaunchingNotification")
            .expect("published");
        // SAFETY: the cell is a word this crate leaked.
        let object = unsafe { *(*cell as *const u64) } as objc::object::Id;
        assert!(!object.is_null());
        // SAFETY: an object this runtime allocated.
        let class = unsafe { objc::object::class_of(object) }.expect("a class");
        assert!(
            class.chain().any(|c| c.name_str().contains("String")),
            "{}",
            class.name_str()
        );
    }
}
