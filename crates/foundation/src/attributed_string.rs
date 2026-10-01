use objc::object::Id;
use objc::sel::Sel;

use crate::define::{class_methods, methods};
use crate::geometry::{NSRange, NSRect, NSSize};

fn class() -> &'static objc::class::Class {
    objc::class::lookup(c"NSAttributedString").expect("NSAttributedString is registered")
}

/// The text, and one attributes dictionary for all of it: the attributes a
/// string was made with. Per-range attributes are not kept.
#[derive(Default)]
struct Contents {
    text: String,
    /// Retained, or null.
    attributes: usize,
}

impl Contents {
    fn new(text: String, attributes: Id) -> Contents {
        Contents {
            text,
            attributes: objc::arc::retain(attributes) as usize,
        }
    }
}

impl Clone for Contents {
    fn clone(&self) -> Contents {
        Contents::new(self.text.clone(), self.attributes as Id)
    }
}

impl Drop for Contents {
    fn drop(&mut self) {
        objc::arc::release(self.attributes as Id);
    }
}

/// # Safety
///
/// `object` is null or an object this runtime allocated.
unsafe fn contents<'a>(object: Id) -> Option<&'a mut Contents> {
    unsafe { crate::boxed::contents::<Contents>(object, class()) }
}

/// The text of an attributed string and the attributes it was made with
/// (null for none), for AppKit, which draws it.
///
/// # Safety
///
/// `object` is null or an object this runtime allocated.
pub unsafe fn parts(object: Id) -> Option<(String, Id)> {
    unsafe { contents(object) }.map(|c| (c.text.clone(), c.attributes as Id))
}

unsafe extern "C" fn init_with_string(receiver: Id, sel: Sel, value: Id) -> Id {
    unsafe { init_with_string_attributes(receiver, sel, value, std::ptr::null_mut()) }
}

/// `-[NSAttributedString initWithString:attributes:]`.
unsafe extern "C" fn init_with_string_attributes(
    receiver: Id,
    _sel: Sel,
    value: Id,
    attributes: Id,
) -> Id {
    // SAFETY: `value` is null or an NSString supplied by the caller.
    let text = String::from_utf8_lossy(&unsafe { crate::string::bytes(value) }).into_owned();
    // SAFETY: this class is registered with `Contents` boxed storage.
    unsafe { crate::boxed::fill(receiver, Contents::new(text, attributes)) };
    receiver
}

unsafe extern "C" fn attributed_string_with_string(_receiver: Id, _sel: Sel, value: Id) -> Id {
    // SAFETY: `value` is null or an NSString supplied by the caller.
    let text = String::from_utf8_lossy(&unsafe { crate::string::bytes(value) }).into_owned();
    let object = crate::boxed::make(class(), Contents::new(text, std::ptr::null_mut()));
    // SAFETY: a fresh object owned by this method.
    unsafe { corefoundation::api::CFAutorelease(object as corefoundation::base::CFTypeRef) as Id }
}

unsafe extern "C" fn string(receiver: Id, _sel: Sel) -> Id {
    // SAFETY: an instance of the class registered below.
    let text = unsafe { contents(receiver) }.map_or("", |c| c.text.as_str());
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
    let count = unsafe { contents(receiver) }.map_or(0, |c| c.text.chars().count());
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

/// AppKit's `-superscriptRange:`, `-subscriptRange:` and `-unscriptRange:`,
/// which are `NSSuperscriptAttributeName` over a range. Per-range attributes are
/// not kept, as [`add_attribute`] already drops them.
///
/// FIXME (ACCURACY): the text keeps its baseline; "x2" draws where Apple's
/// draws a raised "2".
unsafe extern "C" fn script_range(_receiver: Id, _sel: Sel, _range: NSRange) {}

unsafe extern "C" fn set_attributed_string(receiver: Id, _sel: Sel, value: Id) {
    // SAFETY: both objects use this module's boxed storage.
    let replacement = unsafe { contents(value) }.cloned().unwrap_or_default();
    if let Some(contents) = unsafe { contents(receiver) } {
        *contents = replacement;
    }
}

unsafe extern "C" fn append_attributed_string(receiver: Id, _sel: Sel, value: Id) {
    // SAFETY: both objects use this module's boxed storage.
    let Some(appended) = unsafe { contents(value) }.cloned() else {
        return;
    };
    if let Some(contents) = unsafe { contents(receiver) } {
        contents.text.push_str(&appended.text);
        // An empty string takes on what is appended to it.
        if contents.attributes == 0 {
            *contents = Contents::new(contents.text.clone(), appended.attributes as Id);
        }
    }
}

pub(crate) fn make(text: &str) -> Id {
    let object = crate::boxed::make(
        class(),
        Contents::new(text.to_owned(), std::ptr::null_mut()),
    );
    // SAFETY: a fresh object owned by this function.
    unsafe { corefoundation::api::CFAutorelease(object as corefoundation::base::CFTypeRef) as Id }
}

pub(crate) fn install() {
    let class = crate::boxed::register::<Contents>(c"NSAttributedString", None);
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
    let mutable = crate::boxed::register::<Contents>(c"NSMutableAttributedString", Some(class));
    methods!(mutable, {
        "addAttribute:value:range:" => add_attribute, c"v48@0:8@16@24{_NSRange=QQ}32";
        "setAttributedString:" => set_attributed_string, c"v24@0:8@16";
        "appendAttributedString:" => append_attributed_string, c"v24@0:8@16";
        "superscriptRange:" => script_range, c"v32@0:8{_NSRange=QQ}16";
        "subscriptRange:" => script_range, c"v32@0:8{_NSRange=QQ}16";
        "unscriptRange:" => script_range, c"v32@0:8{_NSRange=QQ}16";
    });
}
