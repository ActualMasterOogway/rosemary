use objc::object::Id;
use objc::sel::Sel;

use crate::define::{class_methods, methods};
use crate::geometry::{NSRange, NSRect, NSSize};

fn class() -> &'static objc::class::Class {
    objc::class::lookup(c"NSAttributedString").expect("NSAttributedString is registered")
}

unsafe extern "C" fn init_with_string(receiver: Id, _sel: Sel, value: Id) -> Id {
    // SAFETY: `value` is null or an NSString supplied by the caller.
    let text = String::from_utf8_lossy(&unsafe { crate::string::bytes(value) }).into_owned();
    // SAFETY: this class is registered with `String` boxed storage.
    unsafe { crate::boxed::fill(receiver, text) };
    receiver
}

/// `-[NSAttributedString initWithString:attributes:]`.
///
/// The immutable facade currently stores text only. Accepting the attribute
/// dictionary still preserves the string for callers that measure it.
unsafe extern "C" fn init_with_string_attributes(
    receiver: Id,
    sel: Sel,
    value: Id,
    _attributes: Id,
) -> Id {
    // SAFETY: identical text argument and receiver contract to
    // `-initWithString:`.
    unsafe { init_with_string(receiver, sel, value) }
}

unsafe extern "C" fn attributed_string_with_string(_receiver: Id, _sel: Sel, value: Id) -> Id {
    // SAFETY: `value` is null or an NSString supplied by the caller.
    let text = String::from_utf8_lossy(&unsafe { crate::string::bytes(value) }).into_owned();
    let object = crate::boxed::make(class(), text);
    // SAFETY: a fresh object owned by this method.
    unsafe { corefoundation::api::CFAutorelease(object as corefoundation::base::CFTypeRef) as Id }
}

unsafe extern "C" fn string(receiver: Id, _sel: Sel) -> Id {
    // SAFETY: an instance of the class registered below.
    let text = unsafe { crate::boxed::contents::<String>(receiver, class()) }
        .map_or("", |text| text.as_str());
    objc::arc::autorelease(crate::string::from_str(text))
}

/// `-[NSAttributedString size]`.
///
/// Foundation cannot shape AppKit fonts, but callers still need a non-empty
/// canvas for the common draw-into-an-image path. This conservative estimate
/// keeps one line of text visible to that path until AppKit supplies glyph
/// metrics.
unsafe extern "C" fn size(receiver: Id, _sel: Sel) -> NSSize {
    // SAFETY: an instance of the class registered below.
    let count = unsafe { crate::boxed::contents::<String>(receiver, class()) }
        .map_or(0, |text| text.chars().count());
    NSSize {
        width: count as f64 * 8.0,
        height: if count == 0 { 0.0 } else { 16.0 },
    }
}

/// `-[NSAttributedString drawInRect:]`.
///
/// The focused-image machinery accepts the draw even though Foundation does
/// not itself rasterize AppKit glyphs.
unsafe extern "C" fn draw_in_rect(_receiver: Id, _sel: Sel, _rect: NSRect) {}

unsafe extern "C" fn add_attribute(
    _receiver: Id,
    _sel: Sel,
    _name: Id,
    _value: Id,
    _range: NSRange,
) {
}

unsafe extern "C" fn set_attributed_string(receiver: Id, _sel: Sel, value: Id) {
    // SAFETY: both objects use this module's boxed String storage.
    let text = unsafe { crate::boxed::contents::<String>(value, class()) }
        .cloned()
        .unwrap_or_default();
    if let Some(contents) = unsafe { crate::boxed::contents::<String>(receiver, class()) } {
        *contents = text;
    }
}

unsafe extern "C" fn append_attributed_string(receiver: Id, _sel: Sel, value: Id) {
    // SAFETY: both objects use this module's boxed String storage.
    let text = unsafe { crate::boxed::contents::<String>(value, class()) }
        .cloned()
        .unwrap_or_default();
    if let Some(contents) = unsafe { crate::boxed::contents::<String>(receiver, class()) } {
        contents.push_str(&text);
    }
}

pub(crate) fn make(text: &str) -> Id {
    let object = crate::boxed::make(class(), text.to_owned());
    // SAFETY: a fresh object owned by this function.
    unsafe { corefoundation::api::CFAutorelease(object as corefoundation::base::CFTypeRef) as Id }
}

pub(crate) fn install() {
    let class = crate::boxed::register::<String>(c"NSAttributedString", None);
    methods!(class, {
        "initWithString:" => init_with_string, c"@24@0:8@16";
        "initWithString:attributes:" => init_with_string_attributes, c"@32@0:8@16@24";
        "string" => string, c"@16@0:8";
        "size" => size, c"{CGSize=dd}16@0:8";
        "drawInRect:" => draw_in_rect, c"v48@0:8{CGRect={CGPoint=dd}{CGSize=dd}}16";
    });
    class_methods!(class, {
        "attributedStringWithString:" => attributed_string_with_string, c"@24@0:8@16";
    });
    let mutable = crate::boxed::register::<String>(c"NSMutableAttributedString", Some(class));
    methods!(mutable, {
        "addAttribute:value:range:" => add_attribute, c"v48@0:8@16@24{_NSRange=QQ}32";
        "setAttributedString:" => set_attributed_string, c"v24@0:8@16";
        "appendAttributedString:" => append_attributed_string, c"v24@0:8@16";
    });
}
