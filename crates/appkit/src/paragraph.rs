//! Paragraph-style objects used by attributed AppKit text.
//!
//! Text layout currently keeps only the resulting string dimensions, so these
//! properties have no raster effect yet. The objects still preserve Cocoa's
//! construction and mutation flow instead of turning a variadic attributes
//! dictionary into an early nil terminator.

use std::sync::OnceLock;

use objc::object::Id;
use objc::sel::Sel;

use crate::define::{class_methods, methods};

unsafe extern "C" fn default_paragraph_style(_class: Id, _sel: Sel) -> Id {
    static DEFAULT: OnceLock<usize> = OnceLock::new();
    *DEFAULT.get_or_init(|| {
        let class = objc::class::lookup(c"NSParagraphStyle").expect("declared");
        let object = objc::object::create_instance(class, 0);
        objc::arc::retain(object);
        object as usize
    }) as Id
}

unsafe extern "C" fn ignored_object(_receiver: Id, _sel: Sel, _value: Id) {}

unsafe extern "C" fn ignored_integer(_receiver: Id, _sel: Sel, _value: isize) {}

pub(crate) fn install() {
    let paragraph = objc::class::lookup(c"NSParagraphStyle").expect("declared");
    class_methods!(paragraph, {
        "defaultParagraphStyle" => default_paragraph_style, c"@16@0:8";
    });

    let mutable = objc::class::lookup(c"NSMutableParagraphStyle").expect("declared");
    methods!(mutable, {
        "setParagraphStyle:" => ignored_object, c"v24@0:8@16";
        "setLineBreakMode:" => ignored_integer, c"v24@0:8q16";
        "setAlignment:" => ignored_integer, c"v24@0:8q16";
    });
}
