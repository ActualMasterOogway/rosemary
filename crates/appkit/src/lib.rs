pub mod application;
pub mod classes;
pub mod color;
pub mod cursor;
pub(crate) mod decode;
pub mod event;
mod define;
pub mod keymap;
pub mod nsevent;
pub mod geometry;
pub mod image;
pub mod menu;
pub mod nib;
pub mod opengl;
pub mod raster;
pub mod screen;
pub mod view;
pub mod window;

pub fn bootstrap() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        foundation::bootstrap();
        classes::install();
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
        _ => return None,
    })
}

pub const NAMES: &[&str] = &["NSApplicationMain"];

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
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_listed_name_resolves() {
        let missing: Vec<&str> = NAMES.iter().copied().filter(|n| symbol(n).is_none()).collect();
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
        assert!(class.chain().any(|c| c.name_str().contains("String")), "{}", class.name_str());
    }
}
