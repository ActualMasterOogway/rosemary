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

unsafe extern "C" fn color_with_white(
    _class: Id,
    _sel: Sel,
    white: CGFloat,
    alpha: CGFloat,
) -> Id {
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
        for (slot, value) in [(red, c.red), (green, c.green), (blue, c.blue), (alpha, c.alpha)] {
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
        meta.replace_method(
            objc::sel::get(name),
            named as *const c_void,
            c"@16@0:8",
        );
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
        assert_eq!(unsafe { components(std::ptr::null_mut()) }, (0.0, 0.0, 0.0, 1.0));
    }
}
