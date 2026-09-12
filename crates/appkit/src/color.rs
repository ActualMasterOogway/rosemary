//! `NSColor`, as four components and nothing else.
//!
//! No colour space. Every colour a program in this tree has asked for is one of
//! the named constants or an explicit RGBA, and both are already in the space
//! the only consumer - `glClearColor`, through a window's background - wants. A
//! `-colorUsingColorSpace:` that answered by returning `self` would be a
//! conversion that silently did not happen, so there is not one.

use std::ffi::c_void;

use foundation::geometry::CGFloat;
use objc::object::Id;
use objc::sel::Sel;

use crate::define::{class_methods, methods};

#[repr(C)]
pub struct Color {
    isa: *const c_void,
    red: CGFloat,
    green: CGFloat,
    blue: CGFloat,
    alpha: CGFloat,
}

pub const INSTANCE_SIZE: usize = size_of::<Color>();

/// The components of `color`, or opaque black for anything that is not one.
///
/// # Safety
///
/// `color` must be null or an object this runtime allocated.
pub unsafe fn components(color: Id) -> (f64, f64, f64, f64) {
    // SAFETY: the caller's obligation.
    let is_color = unsafe { objc::object::class_of(color) }.is_some_and(|c| {
        objc::class::lookup(c"NSColor").is_some_and(|k| c.chain().any(|x| std::ptr::eq(x, k)))
    });
    if !is_color {
        return (0.0, 0.0, 0.0, 1.0);
    }
    // SAFETY: the check above says it is one of this crate's colours.
    let c = unsafe { &*(color as *const Color) };
    (c.red, c.green, c.blue, c.alpha)
}

fn make(red: f64, green: f64, blue: f64, alpha: f64) -> Id {
    let class = objc::class::lookup(c"NSColor").expect("declared before its methods");
    // SAFETY: `Color` is the layout the class was registered with.
    let color = unsafe { corefoundation::base::allocate::<Color>(class) };
    // SAFETY: freshly allocated; every field is written before it is read.
    unsafe {
        (*color).red = red;
        (*color).green = green;
        (*color).blue = blue;
        (*color).alpha = alpha;
    }
    objc::arc::autorelease(color as Id)
}

/// The named colours, as `(selector, r, g, b, a)`.
///
/// The first group is the device colours, whose values are their names. The
/// second is the semantic ones - `+textColor` and its relatives - whose value
/// depends on the appearance, and which are answered with the light appearance's
/// because this runtime has one appearance and it is that one. A program that
/// draws with `+textColor` on a dark background gets black text, which is what
/// it would get from AppKit in light mode; a program that switched appearance
/// would not be followed, and there is no appearance here to switch.
static NAMED: &[(&str, f64, f64, f64, f64)] = &[
    ("textColor", 0.0, 0.0, 0.0, 1.0),
    ("labelColor", 0.0, 0.0, 0.0, 1.0),
    ("windowBackgroundColor", 0.93, 0.93, 0.93, 1.0),
    ("controlColor", 0.93, 0.93, 0.93, 1.0),
    ("controlTextColor", 0.0, 0.0, 0.0, 1.0),
    ("controlBackgroundColor", 1.0, 1.0, 1.0, 1.0),
    ("textBackgroundColor", 1.0, 1.0, 1.0, 1.0),
    ("selectedTextColor", 0.0, 0.0, 0.0, 1.0),
    ("windowFrameTextColor", 0.0, 0.0, 0.0, 1.0),
    ("disabledControlTextColor", 0.53, 0.53, 0.53, 1.0),
    // The selection and header colours a widget toolkit builds a palette out of.
    // Qt asks for all of these while its Cocoa plugin is still loading, and one
    // it cannot get is not a wrong shade - it is an unrecognised selector and a
    // program that stops. The accent-dependent ones are the blue accent's, which
    // is the default and the only one this runtime has: there is no
    // `NSColorAppearance` here to read another from.
    ("headerTextColor", 0.0, 0.0, 0.0, 1.0),
    ("headerColor", 0.93, 0.93, 0.93, 1.0),
    ("selectedControlColor", 0.70, 0.84, 1.0, 1.0),
    ("selectedControlTextColor", 0.0, 0.0, 0.0, 1.0),
    ("selectedTextBackgroundColor", 0.70, 0.84, 1.0, 1.0),
    ("alternateSelectedControlColor", 0.0, 0.39, 0.88, 1.0),
    ("alternateSelectedControlTextColor", 1.0, 1.0, 1.0, 1.0),
    ("selectedContentBackgroundColor", 0.0, 0.39, 0.88, 1.0),
    ("selectedMenuItemTextColor", 1.0, 1.0, 1.0, 1.0),
    // "Unemphasized" is the selection in a view that is not the first responder:
    // grey rather than accented, with the ordinary text colour on top.
    ("unemphasizedSelectedContentBackgroundColor", 0.86, 0.86, 0.86, 1.0),
    ("unemphasizedSelectedTextBackgroundColor", 0.86, 0.86, 0.86, 1.0),
    ("unemphasizedSelectedTextColor", 0.0, 0.0, 0.0, 1.0),
    // The focus ring is drawn over what it surrounds, so its alpha is part of
    // the colour rather than something the caller applies.
    ("keyboardFocusIndicatorColor", 0.0, 0.37, 0.89, 0.25),
    ("linkColor", 0.0, 0.41, 0.85, 1.0),
    ("shadowColor", 0.0, 0.0, 0.0, 1.0),
    ("highlightColor", 1.0, 1.0, 1.0, 1.0),
    ("gridColor", 0.90, 0.90, 0.90, 1.0),
    // The label tiers and the separator differ from `labelColor` only in alpha,
    // which is how AppKit defines them.
    ("secondaryLabelColor", 0.0, 0.0, 0.0, 0.5),
    ("tertiaryLabelColor", 0.0, 0.0, 0.0, 0.26),
    ("quaternaryLabelColor", 0.0, 0.0, 0.0, 0.1),
    ("placeholderTextColor", 0.0, 0.0, 0.0, 0.25),
    ("separatorColor", 0.0, 0.0, 0.0, 0.1),
    ("underPageBackgroundColor", 0.60, 0.60, 0.60, 1.0),
    ("alternatingContentBackgroundColor", 0.96, 0.96, 0.96, 1.0),
    // The `systemX` palette, whose values are Apple's published sRGB ones. A
    // program picks one to draw with rather than asking the appearance about it.
    ("systemRedColor", 1.0, 0.23, 0.19, 1.0),
    ("systemGreenColor", 0.16, 0.80, 0.25, 1.0),
    ("systemBlueColor", 0.0, 0.48, 1.0, 1.0),
    ("systemOrangeColor", 1.0, 0.58, 0.0, 1.0),
    ("systemYellowColor", 1.0, 0.80, 0.0, 1.0),
    ("systemPinkColor", 1.0, 0.18, 0.33, 1.0),
    ("systemPurpleColor", 0.69, 0.32, 0.87, 1.0),
    ("systemTealColor", 0.35, 0.78, 0.98, 1.0),
    ("systemIndigoColor", 0.35, 0.34, 0.84, 1.0),
    ("systemBrownColor", 0.64, 0.52, 0.37, 1.0),
    ("systemGrayColor", 0.56, 0.56, 0.58, 1.0),
    ("controlAccentColor", 0.0, 0.39, 0.88, 1.0),
    ("blackColor", 0.0, 0.0, 0.0, 1.0),
    ("whiteColor", 1.0, 1.0, 1.0, 1.0),
    ("clearColor", 0.0, 0.0, 0.0, 0.0),
    ("redColor", 1.0, 0.0, 0.0, 1.0),
    ("greenColor", 0.0, 1.0, 0.0, 1.0),
    ("blueColor", 0.0, 0.0, 1.0, 1.0),
    ("yellowColor", 1.0, 1.0, 0.0, 1.0),
    ("cyanColor", 0.0, 1.0, 1.0, 1.0),
    ("magentaColor", 1.0, 0.0, 1.0, 1.0),
    ("grayColor", 0.5, 0.5, 0.5, 1.0),
    ("lightGrayColor", 2.0 / 3.0, 2.0 / 3.0, 2.0 / 3.0, 1.0),
    ("darkGrayColor", 1.0 / 3.0, 1.0 / 3.0, 1.0 / 3.0, 1.0),
];

/// One `+namedColor`, resolved by the selector it was sent.
///
/// A single implementation for the whole table rather than twelve: the selector
/// is an argument of every method, so the name it was reached by is the lookup
/// key, and twelve near-identical functions would be twelve places to get a
/// component wrong.
unsafe extern "C" fn named(_class: Id, sel: Sel) -> Id {
    // SAFETY: a registered selector is a NUL-terminated interned name.
    let name = unsafe { std::ffi::CStr::from_ptr(sel) };
    let name = name.to_string_lossy();
    match NAMED.iter().find(|(n, ..)| *n == name) {
        Some((_, r, g, b, a)) => make(*r, *g, *b, *a),
        // Unreachable through dispatch - the selector was installed from this
        // very table - and named rather than silent in case it stops being.
        None => objc::fail!("+[NSColor {name}]: not one of this runtime's named colours"),
    }
}

unsafe extern "C" fn color_with_rgba(
    _class: Id,
    _sel: Sel,
    red: CGFloat,
    green: CGFloat,
    blue: CGFloat,
    alpha: CGFloat,
) -> Id {
    make(red, green, blue, alpha)
}

unsafe extern "C" fn color_with_white(_class: Id, _sel: Sel, white: CGFloat, alpha: CGFloat) -> Id {
    make(white, white, white, alpha)
}

unsafe extern "C" fn component(receiver: Id, sel: Sel) -> CGFloat {
    // SAFETY: a colour of this crate's.
    let c = unsafe { &*(receiver as *const Color) };
    // SAFETY: a registered selector is a NUL-terminated interned name.
    match unsafe { std::ffi::CStr::from_ptr(sel) }.to_bytes() {
        b"redComponent" => c.red,
        b"greenComponent" => c.green,
        b"blueComponent" => c.blue,
        _ => c.alpha,
    }
}

/// `-colorSpaceName`, the deprecated name of the space a colour is in.
///
/// Calibrated RGB, which is what every colour here is: four components in the
/// space the module note describes. A caller compares the result against
/// `NSDeviceCMYKColorSpace` before reaching for the CMYK accessors, and this sends
/// it down the RGB branch - which is the one that can be answered.
unsafe extern "C" fn color_space_name(_receiver: Id, _sel: Sel) -> Id {
    // The same interned object the `_NSCalibratedRGBColorSpace` global holds, so a
    // caller comparing the two with `==` - which is what a space-name check is
    // written as - finds them identical as well as equal.
    foundation::notification::name_object("NSCalibratedRGBColorSpace") as Id
}

/// `-colorUsingColorSpaceName:`
///
/// The receiver when the space asked for is an RGB one, and nil otherwise.
///
/// This is not the `self`-returning conversion the module note refuses. The note's
/// objection is to a conversion that silently did not happen; a colour here *is*
/// calibrated RGB, so answering it to a caller that asked for RGB is the
/// conversion, complete. A caller that asked for CMYK or grey gets nil, which is
/// the documented answer for a colour that cannot be converted and the branch it
/// already has - rather than four RGB components relabelled as something else.
///
/// # Safety
///
/// `name` is an `NSString` or nil.
unsafe extern "C" fn color_using_color_space_name(receiver: Id, _sel: Sel, name: Id) -> Id {
    // SAFETY: the caller's obligation.
    let name = unsafe { foundation::string::bytes(name) };
    // A null or empty name means "the most appropriate space", which is this one.
    if name.is_empty() {
        return receiver;
    }
    const RGB: [&str; 4] = [
        "NSCalibratedRGBColorSpace",
        "NSDeviceRGBColorSpace",
        "NSCustomColorSpace",
        "kCGColorSpaceSRGB",
    ];
    match RGB.iter().any(|space| space.as_bytes() == name.as_slice()) {
        true => receiver,
        false => std::ptr::null_mut(),
    }
}

/// `-colorUsingColorSpace:`
///
/// The receiver. There is one colour space in this runtime and every colour is
/// already in it - see the module note - and the `NSColorSpace` a caller would pass
/// to ask for another is a class this tree does not have, so the argument is
/// always the space the receiver is in or nil.
unsafe extern "C" fn color_using_color_space(receiver: Id, _sel: Sel, _space: Id) -> Id {
    receiver
}

/// `-catalogNameComponent` and `-colorNameComponent`.
///
/// Nil, because a colour here is not a catalog colour: it is four components, and
/// `+controlTextColor` resolved them when it was sent rather than handing back a
/// named reference into a catalog that is resolved later.
///
/// The distinction is one a caller acts on. Qt asks whether the catalog is
/// `System`, and renders a colour that says yes into a pattern image instead of
/// reading its components - so claiming a catalog would send it down a drawing
/// path to represent a colour that is already a plain one.
unsafe extern "C" fn name_component(_receiver: Id, _sel: Sel) -> Id {
    std::ptr::null_mut()
}

/// `-numberOfComponents`, which is four: red, green, blue and alpha.
unsafe extern "C" fn number_of_components(_receiver: Id, _sel: Sel) -> isize {
    4
}

/// `-whiteComponent`, the brightness of a grey colour.
///
/// The Rec. 601 luma of the three components, which is what AppKit answers for a
/// colour that is not already grey - and for one that is, the three are equal and
/// any weighting gives the same number back.
unsafe extern "C" fn white_component(receiver: Id, _sel: Sel) -> CGFloat {
    // SAFETY: a colour of this crate's.
    let c = unsafe { &*(receiver as *const Color) };
    0.299 * c.red + 0.587 * c.green + 0.114 * c.blue
}

/// `-getWhite:alpha:`
///
/// # Safety
///
/// Each out-parameter is null or writable for one `CGFloat`.
unsafe extern "C" fn get_white(
    receiver: Id,
    sel: Sel,
    white: *mut CGFloat,
    alpha: *mut CGFloat,
) {
    // SAFETY: a colour of this crate's.
    let c = unsafe { &*(receiver as *const Color) };
    // SAFETY: the caller states each is null or writable.
    unsafe {
        if !white.is_null() {
            *white = white_component(receiver, sel);
        }
        if !alpha.is_null() {
            *alpha = c.alpha;
        }
    }
}

unsafe extern "C" fn get_components(
    receiver: Id,
    _sel: Sel,
    red: *mut CGFloat,
    green: *mut CGFloat,
    blue: *mut CGFloat,
    alpha: *mut CGFloat,
) {
    // SAFETY: a colour of this crate's.
    let c = unsafe { &*(receiver as *const Color) };
    // SAFETY: four out-parameters, each null or writable, as the method's
    // contract says.
    unsafe {
        for (slot, value) in [
            (red, c.red),
            (green, c.green),
            (blue, c.blue),
            (alpha, c.alpha),
        ] {
            if !slot.is_null() {
                *slot = value;
            }
        }
    }
}

pub(crate) fn install() {
    let class = objc::class::lookup(c"NSColor").expect("declared before its methods");
    let meta = class.metaclass();
    for (name, ..) in NAMED {
        meta.replace_method(objc::sel::get(name), named as *const c_void, c"@16@0:8");
    }
    class_methods!(class, {
        "colorWithCalibratedRed:green:blue:alpha:" => color_with_rgba, c"@48@0:8d16d24d32d40";
        "colorWithDeviceRed:green:blue:alpha:" => color_with_rgba, c"@48@0:8d16d24d32d40";
        "colorWithSRGBRed:green:blue:alpha:" => color_with_rgba, c"@48@0:8d16d24d32d40";
        "colorWithRed:green:blue:alpha:" => color_with_rgba, c"@48@0:8d16d24d32d40";
        "colorWithCalibratedWhite:alpha:" => color_with_white, c"@32@0:8d16d24";
        "colorWithDeviceWhite:alpha:" => color_with_white, c"@32@0:8d16d24";
        "colorWithWhite:alpha:" => color_with_white, c"@32@0:8d16d24";
    });
    methods!(class, {
        "redComponent" => component, c"d16@0:8";
        "greenComponent" => component, c"d16@0:8";
        "blueComponent" => component, c"d16@0:8";
        "alphaComponent" => component, c"d16@0:8";
        "getRed:green:blue:alpha:" => get_components, c"v48@0:8^d16^d24^d32^d40";
        "whiteComponent" => white_component, c"d16@0:8";
        "getWhite:alpha:" => get_white, c"v32@0:8^d16^d24";
        "numberOfComponents" => number_of_components, c"q16@0:8";
        "colorSpaceName" => color_space_name, c"@16@0:8";
        "colorUsingColorSpaceName:" => color_using_color_space_name, c"@24@0:8@16";
        "colorUsingColorSpaceName:device:" => color_using_color_space_name, c"@32@0:8@16@24";
        "colorUsingColorSpace:" => color_using_color_space, c"@24@0:8@16";
        "catalogNameComponent" => name_component, c"@16@0:8";
        "colorNameComponent" => name_component, c"@16@0:8";
        "localizedCatalogNameComponent" => name_component, c"@16@0:8";
        "localizedColorNameComponent" => name_component, c"@16@0:8";
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every named colour answers, and answers with its own components - which
    /// is the claim one shared implementation has to earn.
    #[test]
    fn each_named_colour_is_its_own() {
        crate::bootstrap();
        let class = objc::class::ptr(objc::class::lookup(c"NSColor").expect("NSColor")) as Id;
        for (name, r, g, b, a) in NAMED {
            // SAFETY: `+namedColor` is `id (Class, SEL)`.
            let color = unsafe { named(class, objc::sel::get(name)) };
            assert!(!color.is_null(), "{name}");
            // SAFETY: a colour this crate just made.
            assert_eq!(unsafe { components(color) }, (*r, *g, *b, *a), "{name}");
        }
    }

    /// And something that is not a colour reads as opaque black rather than as
    /// whatever its second word happens to be.
    #[test]
    fn a_non_colour_reads_as_black() {
        crate::bootstrap();
        let string = foundation::string::from_str("not a colour");
        // SAFETY: a live object.
        assert_eq!(unsafe { components(string) }, (0.0, 0.0, 0.0, 1.0));
        // SAFETY: null is allowed.
        assert_eq!(
            unsafe { components(std::ptr::null_mut()) },
            (0.0, 0.0, 0.0, 1.0)
        );
    }
}
