#![allow(
    clippy::doc_lazy_continuation,
    clippy::items_after_test_module,
    clippy::manual_is_multiple_of,
    clippy::missing_safety_doc,
    clippy::not_unsafe_ptr_arg_deref,
    clippy::only_used_in_recursion,
    clippy::ptr_eq,
    clippy::single_match,
    clippy::useless_conversion
)]

use std::ffi::c_int;

pub mod alert;
pub mod appearance;
pub mod application;
pub mod backing;
pub mod button;
pub mod cgcontext;
pub mod cgevent;
pub mod classes;
pub mod color;
pub mod coretext;
pub mod cursor;
pub(crate) mod decode;
mod define;
pub mod dock;
pub mod event;
mod font;
pub mod fonts;
pub mod geometry;
pub mod graphics;
pub mod icon;
pub mod image;
mod input_context;
pub mod keymap;
pub mod layer;
pub mod menu;
pub mod metadata;
pub mod monitor;
pub mod nib;
pub mod nsevent;
pub mod opengl;
pub mod pasteboard;
mod pasteboard_manager;
pub mod raster;
mod responder;
pub mod screen;
pub mod script;
pub mod text;
pub mod textinput;
pub mod uchr;
pub mod view;
pub mod window;
pub mod workspace;

pub fn bootstrap() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        foundation::bootstrap();
        classes::install();
        alert::install();
        appearance::install();
        application::install();
        dock::install();
        workspace::install();
        color::install();
        graphics::install();
        cgcontext::install();
        cursor::install();
        font::install();
        image::install();
        image::bitmap::install();
        menu::install();
        screen::install();
        window::install();
        view::install();
        backing::install();
        layer::install();
        text::install();
        input_context::install();
        opengl::install();
        darwin::idle::on_idle(opengl::release_for_other_threads);
        responder::install();
        pasteboard::install();
        nsevent::install();
        monitor::install();
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
        "CGSessionCopyCurrentDictionary" => at!(screen::CGSessionCopyCurrentDictionary),
        "CGDisplayBounds" => at!(screen::CGDisplayBounds),
        "CGDisplayScreenSize" => at!(screen::CGDisplayScreenSize),
        "CGDisplayIsMain" => at!(screen::CGDisplayIsMain),
        "CGDisplayIsOnline" => at!(screen::CGDisplayIsOnline),
        "CGDisplayIsAsleep" => at!(screen::CGDisplayIsAsleep),
        "CGDisplayMirrorsDisplay" => at!(screen::CGDisplayMirrorsDisplay),
        "CGDisplayModelNumber" => at!(screen::CGDisplayModelNumber),
        "CGDisplayVendorNumber" => at!(screen::CGDisplayVendorNumber),
        "CGDisplaySerialNumber" => at!(screen::CGDisplaySerialNumber),
        "CGDisplayCopyDisplayMode" => at!(screen::CGDisplayCopyDisplayMode),
        "CGDisplayModeGetRefreshRate" => at!(screen::CGDisplayModeGetRefreshRate),
        "CGDisplayModeRelease" => at!(screen::CGDisplayModeRelease),
        "CGDisplayModeRetain" => at!(screen::CGDisplayModeRetain),
        "CGDisplayCopyAllDisplayModes" => at!(screen::CGDisplayCopyAllDisplayModes),
        "CGDisplayModeGetWidth" => at!(screen::CGDisplayModeGetWidth),
        "CGDisplayModeGetHeight" => at!(screen::CGDisplayModeGetHeight),
        "CGDisplayModeGetPixelWidth" => at!(screen::CGDisplayModeGetPixelWidth),
        "CGDisplayModeGetPixelHeight" => at!(screen::CGDisplayModeGetPixelHeight),
        "CGDisplayModeGetIOFlags" => at!(screen::CGDisplayModeGetIOFlags),
        "CGDisplayModeIsUsableForDesktopGUI" => at!(screen::CGDisplayModeIsUsableForDesktopGUI),
        "CGDisplayRegisterReconfigurationCallback" => {
            at!(screen::CGDisplayRegisterReconfigurationCallback)
        }
        "CGDisplayRemoveReconfigurationCallback" => {
            at!(screen::CGDisplayRemoveReconfigurationCallback)
        }
        "CGGetDisplaysWithRect" => at!(screen::CGGetDisplaysWithRect),
        "CGDisplayCreateUUIDFromDisplayID" => at!(screen::CGDisplayCreateUUIDFromDisplayID),
        "CGDisplayGetDisplayIDFromUUID" => at!(screen::CGDisplayGetDisplayIDFromUUID),
        "NSBitsPerSampleFromDepth" => at!(screen::NSBitsPerSampleFromDepth),
        "NSBitsPerPixelFromDepth" => at!(screen::NSBitsPerPixelFromDepth),
        "NSNumberOfColorComponents" => at!(screen::NSNumberOfColorComponents),
        "NSPlanarFromDepth" => at!(screen::NSPlanarFromDepth),
        "NSColorSpaceFromDepth" => at!(screen::NSColorSpaceFromDepth),
        "NSBestDepth" => at!(screen::NSBestDepth),
        // The two lists differ only in whether a mirrored display is named,
        // and nothing here mirrors.
        "CGGetOnlineDisplayList" | "CGGetActiveDisplayList" => {
            at!(screen::CGGetOnlineDisplayList)
        }
        "CGDisplayIDToOpenGLDisplayMask" => at!(screen::CGDisplayIDToOpenGLDisplayMask),
        "CGDisplayIOServicePort" => at!(screen::CGDisplayIOServicePort),
        "CGWarpMouseCursorPosition" => at!(screen::CGWarpMouseCursorPosition),
        "CGAssociateMouseAndMouseCursorPosition" => {
            at!(screen::CGAssociateMouseAndMouseCursorPosition)
        }
        "CGEventSourceKeyState" => at!(screen::CGEventSourceKeyState),
        "CGEventCreate" => at!(cgevent::CGEventCreate),
        "CGEventGetLocation" => at!(cgevent::CGEventGetLocation),
        "CGEventCreateMouseEvent" => at!(cgevent::CGEventCreateMouseEvent),
        "CGEventSetType" => at!(cgevent::CGEventSetType),
        "CGEventPost" => at!(cgevent::CGEventPost),
        "CGColorSpaceCreateWithName" => at!(graphics::CGColorSpaceCreateWithName),
        "CGColorSpaceCreateDeviceRGB" => at!(graphics::CGColorSpaceCreateDeviceRGB),
        "CGColorSpaceRelease" => at!(graphics::CGColorSpaceRelease),
        "CGDataProviderCreateWithData" => at!(graphics::CGDataProviderCreateWithData),
        "CGDataProviderRelease" => at!(graphics::CGDataProviderRelease),
        "CGImageCreate" => at!(graphics::CGImageCreate),
        "CGImageCreateCopyWithColorSpace" => at!(graphics::CGImageCreateCopyWithColorSpace),
        "CGImageRetain" => at!(graphics::CGImageRetain),
        "CGImageRelease" => at!(graphics::CGImageRelease),
        "CGImageGetWidth" => at!(graphics::CGImageGetWidth),
        "CGImageGetHeight" => at!(graphics::CGImageGetHeight),
        "CGImageGetBitsPerComponent" => at!(graphics::CGImageGetBitsPerComponent),
        "CGImageGetAlphaInfo" => at!(graphics::CGImageGetAlphaInfo),
        "CGImageGetColorSpace" => at!(graphics::CGImageGetColorSpace),
        "CGRectGetWidth" => at!(geometry::CGRectGetWidth),
        "CGRectGetHeight" => at!(geometry::CGRectGetHeight),
        "CGRectGetMinX" => at!(geometry::CGRectGetMinX),
        "CGRectGetMidX" => at!(geometry::CGRectGetMidX),
        "CGRectGetMaxX" => at!(geometry::CGRectGetMaxX),
        "CGRectGetMinY" => at!(geometry::CGRectGetMinY),
        "CGRectGetMidY" => at!(geometry::CGRectGetMidY),
        "CGRectGetMaxY" => at!(geometry::CGRectGetMaxY),
        "NSPointInRect" => at!(geometry::NSPointInRect),
        "NSMouseInRect" => at!(geometry::NSMouseInRect),
        "NSInsetRect" => at!(geometry::NSInsetRect),
        "TISCopyCurrentKeyboardInputSource" => at!(textinput::TISCopyCurrentKeyboardInputSource),
        "TISCopyInputMethodKeyboardLayoutOverride" => {
            at!(textinput::TISCopyInputMethodKeyboardLayoutOverride)
        }
        "TISCopyCurrentKeyboardLayoutInputSource" => {
            at!(textinput::TISCopyCurrentKeyboardLayoutInputSource)
        }
        "TISCopyCurrentASCIICapableKeyboardInputSource" => {
            at!(textinput::TISCopyCurrentASCIICapableKeyboardInputSource)
        }
        "TISCopyCurrentASCIICapableKeyboardLayoutInputSource" => {
            at!(textinput::TISCopyCurrentASCIICapableKeyboardLayoutInputSource)
        }
        "TISCopyInputSourceForLanguage" => at!(textinput::TISCopyInputSourceForLanguage),
        "TISCreateInputSourceList" => at!(textinput::TISCreateInputSourceList),
        "TISCreateASCIICapableInputSourceList" => {
            at!(textinput::TISCreateASCIICapableInputSourceList)
        }
        "TISSelectInputSource" => at!(textinput::TISSelectInputSource),
        "TISDeselectInputSource" => at!(textinput::TISDeselectInputSource),
        "TISEnableInputSource" => at!(textinput::TISEnableInputSource),
        "TISDisableInputSource" => at!(textinput::TISDisableInputSource),
        "TISGetInputSourceProperty" => at!(textinput::TISGetInputSourceProperty),
        "KBGetLayoutType" => at!(textinput::KBGetLayoutType),
        "LMGetKbdType" => at!(textinput::LMGetKbdType),
        "LMGetKbdLast" => at!(textinput::LMGetKbdLast),
        "UCKeyTranslate" => at!(textinput::UCKeyTranslate),
        "CGLEnable" => at!(CGLEnable),
        "CGLDisable" => at!(CGLDisable),
        "CGLSetParameter" => at!(CGLSetParameter),
        other => {
            return pasteboard_manager::symbol(other)
                .or_else(|| coretext::symbol(other))
                .or_else(|| cgcontext::symbol(other));
        }
    })
}

/// Every name [`symbol`] answers, this crate's and the Pasteboard Manager's.
pub fn names() -> Vec<&'static str> {
    NAMES
        .iter()
        .copied()
        .chain(pasteboard_manager::NAMES.iter().copied())
        .chain(coretext::NAMES.iter().copied())
        .chain(cgcontext::NAMES.iter().copied())
        .collect()
}

pub const NAMES: &[&str] = &[
    "NSApplicationMain",
    "CGMainDisplayID",
    "CGSessionCopyCurrentDictionary",
    "CGDisplayBounds",
    "CGDisplayScreenSize",
    "CGDisplayIsMain",
    "CGDisplayIsOnline",
    "CGDisplayIsAsleep",
    "CGDisplayMirrorsDisplay",
    "CGDisplayModelNumber",
    "CGDisplayVendorNumber",
    "CGDisplaySerialNumber",
    "CGDisplayCopyDisplayMode",
    "CGDisplayModeGetRefreshRate",
    "CGDisplayModeRelease",
    "CGDisplayModeRetain",
    "CGDisplayCopyAllDisplayModes",
    "CGDisplayModeGetWidth",
    "CGDisplayModeGetHeight",
    "CGDisplayModeGetPixelWidth",
    "CGDisplayModeGetPixelHeight",
    "CGDisplayModeGetIOFlags",
    "CGDisplayModeIsUsableForDesktopGUI",
    "CGDisplayRegisterReconfigurationCallback",
    "CGDisplayRemoveReconfigurationCallback",
    "CGGetDisplaysWithRect",
    "CGDisplayCreateUUIDFromDisplayID",
    "CGDisplayGetDisplayIDFromUUID",
    "NSBitsPerSampleFromDepth",
    "NSBitsPerPixelFromDepth",
    "NSNumberOfColorComponents",
    "NSPlanarFromDepth",
    "NSColorSpaceFromDepth",
    "NSBestDepth",
    "CGGetOnlineDisplayList",
    "CGGetActiveDisplayList",
    "CGDisplayIDToOpenGLDisplayMask",
    "CGDisplayIOServicePort",
    "CGWarpMouseCursorPosition",
    "CGAssociateMouseAndMouseCursorPosition",
    "CGEventSourceKeyState",
    "CGEventCreate",
    "CGEventGetLocation",
    "CGEventCreateMouseEvent",
    "CGEventSetType",
    "CGEventPost",
    "CGColorSpaceCreateWithName",
    "CGColorSpaceCreateDeviceRGB",
    "CGColorSpaceRelease",
    "CGDataProviderCreateWithData",
    "CGDataProviderRelease",
    "CGImageCreate",
    "CGImageCreateCopyWithColorSpace",
    "CGImageRetain",
    "CGImageRelease",
    "CGImageGetWidth",
    "CGImageGetHeight",
    "CGImageGetBitsPerComponent",
    "CGImageGetAlphaInfo",
    "CGImageGetColorSpace",
    "CGRectGetWidth",
    "CGRectGetHeight",
    "CGRectGetMinX",
    "CGRectGetMidX",
    "CGRectGetMaxX",
    "CGRectGetMinY",
    "CGRectGetMidY",
    "CGRectGetMaxY",
    "NSPointInRect",
    "NSMouseInRect",
    "NSInsetRect",
    "TISCopyCurrentKeyboardInputSource",
    "TISCopyInputMethodKeyboardLayoutOverride",
    "TISCopyCurrentKeyboardLayoutInputSource",
    "TISCopyCurrentASCIICapableKeyboardInputSource",
    "TISCopyCurrentASCIICapableKeyboardLayoutInputSource",
    "TISCopyInputSourceForLanguage",
    "TISCreateInputSourceList",
    "TISCreateASCIICapableInputSourceList",
    "TISSelectInputSource",
    "TISDeselectInputSource",
    "TISEnableInputSource",
    "TISDisableInputSource",
    "TISGetInputSourceProperty",
    "KBGetLayoutType",
    "LMGetKbdType",
    "LMGetKbdLast",
    "UCKeyTranslate",
    "CGLDisable",
    "CGLSetParameter",
    "CGLEnable",
];

// FIXME (ACCURACY): move all of these to the opengl framework and actually respect them
#[allow(non_snake_case)]
extern "C" fn CGLDisable() -> c_int {
    0
}

#[allow(non_snake_case)]
extern "C" fn CGLEnable() -> c_int {
    0
}

#[allow(non_snake_case)]
extern "C" fn CGLSetParameter() -> c_int {
    0
}

pub fn ns_app_cell() -> u64 {
    corefoundation::base::global_cell("_NSApp", 0)
}

// CoreGraphics exports these structures themselves, rather than pointers to
// them. A guest loads their addresses from its GOT and copies the fields.
static CG_AFFINE_TRANSFORM_IDENTITY: [f64; 6] = [1.0, 0.0, 0.0, 1.0, 0.0, 0.0];
static CG_POINT_ZERO: [f64; 2] = [0.0, 0.0];
/// `{{-CGFLOAT_MAX / 2, -CGFLOAT_MAX / 2}, {CGFLOAT_MAX, CGFLOAT_MAX}}`.
static CG_RECT_INFINITE: [f64; 4] = [f64::MAX / -2.0, f64::MAX / -2.0, f64::MAX, f64::MAX];

pub fn data_symbols() -> Vec<(String, u64)> {
    bootstrap();
    let mut out = classes::class_data();
    out.extend(coretext::data_symbols());
    out.extend(cgcontext::data_symbols());
    out.push(("_NSApp".to_string(), ns_app_cell()));
    out.push((
        "_CGAffineTransformIdentity".to_string(),
        CG_AFFINE_TRANSFORM_IDENTITY.as_ptr() as usize as u64,
    ));
    out.push((
        "_CGPointZero".to_string(),
        CG_POINT_ZERO.as_ptr() as usize as u64,
    ));
    out.push((
        "_CGRectInfinite".to_string(),
        CG_RECT_INFINITE.as_ptr() as usize as u64,
    ));
    let srgb = foundation::notification::name_object("kCGColorSpaceSRGB");
    out.push((
        "_kCGColorSpaceSRGB".to_string(),
        corefoundation::base::global_cell("_kCGColorSpaceSRGB", srgb),
    ));
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
    // Doubles rather than pointers - see [`FONT_WEIGHTS`].
    for (symbol, weight) in FONT_WEIGHTS {
        out.push((
            (*symbol).to_string(),
            corefoundation::base::global_cell(symbol, weight.to_bits()),
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
    window::DID_BECOME_MAIN,
    window::DID_RESIGN_MAIN,
    window::DID_MINIATURIZE,
    window::DID_DEMINIATURIZE,
    window::WILL_ENTER_FULL_SCREEN,
    window::WILL_EXIT_FULL_SCREEN,
    window::DID_ENTER_FULL_SCREEN,
    window::DID_EXIT_FULL_SCREEN,
    window::DID_CHANGE_BACKING_PROPERTIES,
    text::DID_CHANGE,
    text::DID_CHANGE_SELECTION,
    text::CONTROL_DID_CHANGE,
    application::DID_CHANGE_SCREEN_PARAMETERS,
    workspace::WILL_SLEEP,
    workspace::DID_WAKE,
    workspace::SCREENS_DID_SLEEP,
    workspace::SCREENS_DID_WAKE,
    view::FRAME_DID_CHANGE,
    view::BOUNDS_DID_CHANGE,
    // Watched by a widget toolkit and posted by nothing here: there is no system
    // colour change to follow, no live resize to bracket, and no colour panel.
    // Published for the reason the note above gives - a name nothing posts is a
    // gap, a name nothing defines is a segfault at address zero.
    "NSSystemColorsDidChangeNotification",
    "NSApplicationWillBecomeActiveNotification",
    window::WILL_START_LIVE_RESIZE,
    window::DID_END_LIVE_RESIZE,
    "NSColorPanelColorDidChangeNotification",
    "NSPreferredScrollerStyleDidChangeNotification",
    "NSTextInputContextKeyboardSelectionDidChangeNotification",
];

/// The other AppKit string globals a program reads.
///
/// Each is a fixed name rather than something this runtime chooses: a colour
/// space and an attribute key are compared against by string, so publishing the
/// documented value is exact.
pub const STRING_GLOBALS: &[(&str, &str)] = &[
    ("_NSCalibratedRGBColorSpace", "NSCalibratedRGBColorSpace"),
    ("_NSDeviceRGBColorSpace", "NSDeviceRGBColorSpace"),
    // The other colour-space names, published so the *comparison* against them
    // works. `-colorSpaceName` answers calibrated RGB for every colour here, so a
    // caller testing against these finds "no" - which is the answer, and which it
    // cannot reach at all while the symbol it is comparing with is address zero.
    (
        "_NSCalibratedWhiteColorSpace",
        "NSCalibratedWhiteColorSpace",
    ),
    (
        "_NSCalibratedBlackColorSpace",
        "NSCalibratedBlackColorSpace",
    ),
    ("_NSDeviceWhiteColorSpace", "NSDeviceWhiteColorSpace"),
    ("_NSDeviceBlackColorSpace", "NSDeviceBlackColorSpace"),
    ("_NSDeviceCMYKColorSpace", "NSDeviceCMYKColorSpace"),
    ("_NSNamedColorSpace", "NSNamedColorSpace"),
    ("_NSPatternColorSpace", "NSPatternColorSpace"),
    ("_NSDeviceResolution", "NSDeviceResolution"),
    ("_NSForegroundColorAttributeName", "NSColor"),
    ("_NSFontAttributeName", "NSFont"),
    ("_NSObliquenessAttributeName", "NSObliqueness"),
    ("_NSAppearanceNameAqua", "NSAppearanceNameAqua"),
    ("_NSDeviceSize", "NSDeviceSize"),
    ("_NSModalPanelRunLoopMode", "NSModalPanelRunLoopMode"),
    // The pasteboard's plain-text type, under both its spellings: the modern one
    // is a uniform type identifier and the deprecated one is not, and a program
    // that writes with one and reads with the other has to name the same type.
    ("_NSPasteboardTypeString", "public.utf8-plain-text"),
    ("_NSStringPboardType", "public.utf8-plain-text"),
    // The rest of the deprecated pasteboard types, whose values are their own
    // names - which is what they are on a Mac, and what makes a program that
    // writes with the old spelling and reads with it find its own data.
    ("_NSFilenamesPboardType", "NSFilenamesPboardType"),
    ("_NSFilesPromisePboardType", "NSFilesPromisePboardType"),
    ("_NSURLPboardType", "NSURLPboardType"),
    ("_NSColorPboardType", "NSColorPboardType"),
    ("_NSFontPboardType", "NSFontPboardType"),
    ("_NSRulerPboardType", "NSRulerPboardType"),
    ("_NSTIFFPboardType", "NSTIFFPboardType"),
    ("_NSPDFPboardType", "NSPDFPboardType"),
    ("_NSPostScriptPboardType", "NSPostScriptPboardType"),
    ("_NSRTFPboardType", "NSRTFPboardType"),
    ("_NSRTFDPboardType", "NSRTFDPboardType"),
    ("_NSHTMLPboardType", "NSHTMLPboardType"),
    ("_NSTabularTextPboardType", "NSTabularTextPboardType"),
    ("_NSVCardPboardType", "NSVCardPboardType"),
    ("_NSInkTextPboardType", "NSInkTextPboardType"),
    ("_NSFileContentsPboardType", "NSFileContentsPboardType"),
    (
        "_NSMultipleTextSelectionPboardType",
        "NSMultipleTextSelectionPboardType",
    ),
    // The drag pasteboard's name, which is Apple's string and not the symbol's:
    // a program naming the board by hand has to name the same one.
    ("_NSPasteboardNameDrag", "Apple CFPasteboard drag"),
    // Attributed-string attribute names. Each is shorter than its symbol, as
    // `NSForegroundColorAttributeName` above already is.
    ("_NSUnderlineStyleAttributeName", "NSUnderline"),
    ("_NSUnderlineColorAttributeName", "NSUnderlineColor"),
    ("_NSDocumentTypeDocumentAttribute", "DocumentType"),
    ("_NSHTMLTextDocumentType", "NSHTML"),
    ("_NSRTFTextDocumentType", "NSRTF"),
    // The only named system image a program asks for here.
    ("_NSImageNameApplicationIcon", "NSApplicationIcon"),
];

/// `NSFontWeight` globals, which are **`CGFloat` variables** and not pointers.
///
/// The difference matters: a guest loads the symbol's address and reads a
/// *double* out of it, so publishing one as a string object hands it the bit
/// pattern of a pointer as a font weight. The values are Apple's own, on the
/// -1.0 to 1.0 scale `+systemFontOfSize:weight:` takes.
const FONT_WEIGHTS: &[(&str, f64)] = &[
    ("_NSFontWeightUltraLight", -0.8),
    ("_NSFontWeightThin", -0.6),
    ("_NSFontWeightLight", -0.4),
    ("_NSFontWeightRegular", 0.0),
    ("_NSFontWeightMedium", 0.23),
    ("_NSFontWeightSemibold", 0.3),
    ("_NSFontWeightBold", 0.4),
    ("_NSFontWeightHeavy", 0.56),
    ("_NSFontWeightBlack", 0.62),
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

    #[test]
    fn core_graphics_structure_globals_point_at_their_fields() {
        let symbols = data_symbols();
        let (_, identity) = symbols
            .iter()
            .find(|(name, _)| name == "_CGAffineTransformIdentity")
            .expect("published");
        // SAFETY: the symbol points at the six-double static above.
        let fields = unsafe { std::slice::from_raw_parts(*identity as *const f64, 6) };
        assert_eq!(fields, &[1.0, 0.0, 0.0, 1.0, 0.0, 0.0]);
    }
}
