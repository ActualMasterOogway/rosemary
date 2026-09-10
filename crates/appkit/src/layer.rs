use std::ffi::c_void;

use foundation::geometry::NSSize;
use objc::object::Id;
use objc::sel::Sel;

use crate::define::{class_methods, methods};

#[repr(C)]
pub struct Layer {
    isa: *const c_void,
    /// `-contentsScale`. Never zero: see the module note.
    contents_scale: f64,
    /// The view whose layer this is, or null. Unretained, the mirror of the
    /// view's own retained reference: a layer never outlives the view that
    /// holds it, and a retain here would be a cycle neither side could break.
    ///
    /// AppKit spells this `-superlayer` and a real one is a layer, not a
    /// view. What the runtime actually needs it for is presentation: a
    /// `CAMetalLayer`'s frame reaches a window through the view that holds
    /// it, and nothing else in the object graph points that way.
    view: Id,
    opaque: u8,
}

pub const INSTANCE_SIZE: usize = size_of::<Layer>();

#[repr(C)]
struct MetalLayer {
    layer: Layer,
    device: Id,
    pixel_format: u64,
    drawable_size: NSSize,
    framebuffer_only: u8,
    display_sync_enabled: u8,
}

pub const METAL_INSTANCE_SIZE: usize = size_of::<MetalLayer>();

fn layer_of<'a>(receiver: Id) -> &'a mut Layer {
    // SAFETY: a layer of this crate's, which is what every method here is sent
    // to.
    unsafe { &mut *(receiver as *mut Layer) }
}

fn metal_layer_of<'a>(receiver: Id) -> &'a mut MetalLayer {
    // SAFETY: a CAMetalLayer, whose registered layout is MetalLayer.
    unsafe { &mut *(receiver as *mut MetalLayer) }
}

/// `+[CALayer layer]`, autoreleased as a `+`-less constructor is.
unsafe extern "C" fn new_layer(class: Id, _sel: Sel) -> Id {
    // SAFETY: `class` is `CALayer` or one of its declared subclasses.
    let object = unsafe {
        objc::object::create_instance(
            objc::class::from_ptr(class as *const objc::class::Class).expect("a class"),
            0,
        )
    };
    let layer = layer_of(object);
    layer.contents_scale = 1.0;
    layer.view = std::ptr::null_mut();
    layer.opaque = 0;
    if objc::class::lookup(c"CAMetalLayer").is_some_and(|metal| {
        // SAFETY: the receiver of a class method is a class object.
        unsafe { objc::class::from_ptr(class as *const objc::class::Class) }.is_some_and(|actual| {
            actual
                .chain()
                .any(|candidate| std::ptr::eq(candidate, metal))
        })
    }) {
        let metal = metal_layer_of(object);
        // CAMetalLayer's documented defaults.
        metal.pixel_format = 80; // MTLPixelFormatBGRA8Unorm
        metal.framebuffer_only = 1;
        metal.display_sync_enabled = 1;
    }
    // SAFETY: a fresh object this call owns.
    unsafe { corefoundation::api::CFAutorelease(object as corefoundation::base::CFTypeRef) as Id }
}

/// Record which view holds `layer`, as `-[NSView setLayer:]` decides it.
///
/// # Safety
///
/// `layer` is null or a layer of this crate's, and `view` is null or a view
/// of this crate's which outlives the layer.
pub unsafe fn set_view(layer: Id, view: Id) {
    if layer.is_null() || !is_layer(layer) {
        return;
    }
    layer_of(layer).view = view;
}

/// The view a layer was given to, or null.
///
/// # Safety
///
/// As [`set_view`].
pub unsafe fn view_of(layer: Id) -> Id {
    match !layer.is_null() && is_layer(layer) {
        true => layer_of(layer).view,
        false => std::ptr::null_mut(),
    }
}

fn is_layer(object: Id) -> bool {
    objc::class::lookup(c"CALayer").is_some_and(|base| {
        // SAFETY: null or an object the runtime allocated.
        unsafe { objc::object::class_of(object) }
            .is_some_and(|class| class.chain().any(|candidate| std::ptr::eq(candidate, base)))
    })
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

unsafe extern "C" fn set_device(receiver: Id, _sel: Sel, device: Id) {
    let layer = metal_layer_of(receiver);
    if layer.device != device {
        let old = layer.device;
        layer.device = objc::arc::retain(device);
        objc::arc::release(old);
    }
}

unsafe extern "C" fn device(receiver: Id, _sel: Sel) -> Id {
    metal_layer_of(receiver).device
}

unsafe extern "C" fn set_pixel_format(receiver: Id, _sel: Sel, format: u64) {
    metal_layer_of(receiver).pixel_format = format;
}

unsafe extern "C" fn pixel_format(receiver: Id, _sel: Sel) -> u64 {
    metal_layer_of(receiver).pixel_format
}

unsafe extern "C" fn set_drawable_size(receiver: Id, _sel: Sel, size: NSSize) {
    metal_layer_of(receiver).drawable_size = size;
}

unsafe extern "C" fn drawable_size(receiver: Id, _sel: Sel) -> NSSize {
    metal_layer_of(receiver).drawable_size
}

unsafe extern "C" fn set_framebuffer_only(receiver: Id, _sel: Sel, value: u8) {
    metal_layer_of(receiver).framebuffer_only = value;
}

unsafe extern "C" fn framebuffer_only(receiver: Id, _sel: Sel) -> u8 {
    metal_layer_of(receiver).framebuffer_only
}

unsafe extern "C" fn set_display_sync_enabled(receiver: Id, _sel: Sel, value: u8) {
    metal_layer_of(receiver).display_sync_enabled = value;
}

unsafe extern "C" fn display_sync_enabled(receiver: Id, _sel: Sel) -> u8 {
    metal_layer_of(receiver).display_sync_enabled
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
    let metal = objc::class::lookup(c"CAMetalLayer").expect("declared before its methods");
    methods!(metal, {
        "setDevice:" => set_device, c"v24@0:8@16";
        "device" => device, c"@16@0:8";
        "setPixelFormat:" => set_pixel_format, c"v24@0:8Q16";
        "pixelFormat" => pixel_format, c"Q16@0:8";
        "setDrawableSize:" => set_drawable_size, c"v32@0:8{CGSize=dd}16";
        "drawableSize" => drawable_size, c"{CGSize=dd}16@0:8";
        "setFramebufferOnly:" => set_framebuffer_only, c"v20@0:8c16";
        "framebufferOnly" => framebuffer_only, c"c16@0:8";
        "setDisplaySyncEnabled:" => set_display_sync_enabled, c"v20@0:8c16";
        "displaySyncEnabled" => display_sync_enabled, c"c16@0:8";
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
    fn a_metal_layer_inherits_the_layer_constructor() {
        crate::bootstrap();
        let class = objc::class::lookup(c"CAMetalLayer").expect("CAMetalLayer");
        let selector = objc::sel::get("layer");
        let implementation = class
            .metaclass()
            .method(selector)
            .expect("CAMetalLayer inherits +[CALayer layer]")
            .implementation();
        // SAFETY: the inherited method is `id (Class, SEL)`.
        let call: unsafe extern "C" fn(Id, Sel) -> Id =
            unsafe { std::mem::transmute(implementation) };
        let object = unsafe { call(objc::class::ptr(class) as Id, selector) };
        // SAFETY: a live object returned by `+layer`.
        assert!(
            unsafe { objc::object::class_of(object) }.is_some_and(|actual| {
                actual
                    .chain()
                    .any(|candidate| std::ptr::eq(candidate, class))
            })
        );
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
