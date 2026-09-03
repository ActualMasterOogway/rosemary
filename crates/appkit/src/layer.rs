//! `CALayer`, which is here for one number.
//!
//! Core Animation, not AppKit, but a layer only ever reaches this tree through
//! `-[NSView setLayer:]`, so it lives beside [`crate::view`].
//!
//! Nothing here composites a layer and nothing is going to: a guest that hands
//! its view a layer draws through the layer's own drawables - Metal's, or the
//! GL context's - and the view's whole obligation is to give the same object
//! back. What the *layer* owes is `-contentsScale`, and that one accessor is
//! why this file exists.
//!
//! An unbound `_OBJC_CLASS_$_CALayer` binds to zero, so `+layer` is nil,
//! `-[NSView setLayer:nil]` stores nothing, and `[[view layer] contentsScale]`
//! is **0.0** - a `double` return from a message to nil. Roblox multiplies its
//! view bounds by that number. Nothing faults, nothing is logged, and the
//! program carries on with a size of zero: the silent-wrong-answer this runtime
//! exists to avoid, in one accessor.
//!
//! The default is `1.0` because that is Apple's, and a layer attached to a view
//! takes the window's backing scale - see [`crate::view`]'s `-layer`.

use std::ffi::c_void;

use objc::object::Id;
use objc::sel::Sel;

use crate::define::{class_methods, methods};

#[repr(C)]
pub struct Layer {
    isa: *const c_void,
    /// `-contentsScale`. Never zero: see the module note.
    contents_scale: f64,
    opaque: u8,
}

pub const INSTANCE_SIZE: usize = size_of::<Layer>();

fn layer_of<'a>(receiver: Id) -> &'a mut Layer {
    // SAFETY: a layer of this crate's, which is what every method here is sent
    // to.
    unsafe { &mut *(receiver as *mut Layer) }
}

/// `+[CALayer layer]`, autoreleased as a `+`-less constructor is.
unsafe extern "C" fn new_layer(class: Id, _sel: Sel) -> Id {
    // SAFETY: `class` is `CALayer`, which this crate declared.
    let object = unsafe {
        objc::object::create_instance(
            objc::class::from_ptr(class as *const objc::class::Class).expect("a class"),
            0,
        )
    };
    let layer = layer_of(object);
    layer.contents_scale = 1.0;
    layer.opaque = 0;
    // SAFETY: a fresh object this call owns.
    unsafe { corefoundation::api::CFAutorelease(object as corefoundation::base::CFTypeRef) as Id }
}

unsafe extern "C" fn contents_scale(receiver: Id, _sel: Sel) -> f64 {
    layer_of(receiver).contents_scale
}

/// `-setContentsScale:`, which refuses a scale of zero.
///
/// A caller that reads the scale back and multiplies by it is the reason the
/// class is here at all, and a zero stored on the way in would reintroduce the
/// bug from the other side. Apple's clamps too.
unsafe extern "C" fn set_contents_scale(receiver: Id, _sel: Sel, scale: f64) {
    if scale > 0.0 {
        layer_of(receiver).contents_scale = scale;
    }
}

unsafe extern "C" fn set_opaque(receiver: Id, _sel: Sel, opaque: u8) {
    layer_of(receiver).opaque = opaque;
}

unsafe extern "C" fn is_opaque(receiver: Id, _sel: Sel) -> u8 {
    layer_of(receiver).opaque
}

pub(crate) fn install() {
    let class = objc::class::lookup(c"CALayer").expect("declared before its methods");
    class_methods!(class, {
        "layer" => new_layer, c"@16@0:8";
    });
    methods!(class, {
        "contentsScale" => contents_scale, c"d16@0:8";
        "setContentsScale:" => set_contents_scale, c"v24@0:8d16";
        "setOpaque:" => set_opaque, c"v20@0:8c16";
        "isOpaque" => is_opaque, c"c16@0:8";
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layer() -> Id {
        crate::bootstrap();
        let class = objc::class::lookup(c"CALayer").expect("CALayer");
        // SAFETY: the class this crate declared.
        unsafe { new_layer(objc::class::ptr(class) as Id, objc::sel::get("layer")) }
    }

    /// The whole reason the class exists: a scale of zero is what a message to
    /// nil answers, and a caller multiplying its bounds by it gets nothing.
    #[test]
    fn a_new_layer_has_a_scale_of_one() {
        let layer = layer();
        // SAFETY: a layer this test made.
        assert_eq!(unsafe { contents_scale(layer, objc::sel::get("x")) }, 1.0);
    }

    #[test]
    fn a_scale_reads_back_and_zero_is_refused() {
        let layer = layer();
        let sel = objc::sel::get("x");
        // SAFETY: a layer this test made.
        unsafe {
            set_contents_scale(layer, sel, 2.0);
            assert_eq!(contents_scale(layer, sel), 2.0);
            set_contents_scale(layer, sel, 0.0);
            assert_eq!(contents_scale(layer, sel), 2.0, "zero must not be stored");
        }
    }
}
