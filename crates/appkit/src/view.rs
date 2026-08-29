//! `NSView`.
//!
//! A view here has a frame, a window and a subview list, and draws nothing. That
//! is not a stub: the one view Deltarune actually renders through is its own
//! `YYGLView`, whose drawing is `NSOpenGLContext` and `gl*` calls the guest
//! makes itself. What AppKit has to supply is where the view is and how big its
//! backing store is, because those are what the guest sizes its viewport from.
//!
//! `-convertRectToBacking:` is the reason `-frame` alone is not enough. On a
//! Retina display a 640x480 view has a 1280x960 framebuffer, and a guest that
//! set its viewport from the frame would draw into a quarter of the window.

use std::ffi::c_void;

use foundation::geometry::{CGFloat, NSPoint, NSRect, NSSize};
use objc::object::Id;
use objc::sel::Sel;

use crate::define::methods;

#[repr(C)]
pub struct View {
    isa: *const c_void,
    /// Unretained, as AppKit's is: a view's window owns the view.
    window: Id,
    superview: Id,
    subviews: Id,
    tracking_areas: Id,
    frame: NSRect,
    wants_best_resolution: usize,
}

pub const INSTANCE_SIZE: usize = size_of::<View>();

fn view_of<'a>(receiver: Id) -> &'a mut View {
    // SAFETY: a view of this crate's, which is what every method here is sent to.
    unsafe { &mut *(receiver as *mut View) }
}

fn is_view(object: Id) -> bool {
    // SAFETY: null or an object this runtime allocated, which is what every
    // caller has.
    unsafe { objc::object::class_of(object) }.is_some_and(|c| {
        objc::class::lookup(c"NSView").is_some_and(|k| c.chain().any(|x| std::ptr::eq(x, k)))
    })
}

/// Tell a view which window it is in, from `-[NSWindow setContentView:]`.
///
/// # Safety
///
/// `view` must be null or an object this runtime allocated.
pub unsafe fn set_window(view: Id, window: Id) {
    if !is_view(view) {
        return;
    }
    view_of(view).window = window;
}

/// The window a view is in, following the superview chain.
///
/// # Safety
///
/// As [`set_window`].
pub unsafe fn window_of(view: Id) -> Id {
    let mut current = view;
    // Bounded rather than a bare loop: a subview list a guest built with a cycle
    // in it would otherwise hang inside a method that looks like an accessor.
    for _ in 0..64 {
        if !is_view(current) {
            return std::ptr::null_mut();
        }
        let v = view_of(current);
        if !v.window.is_null() {
            return v.window;
        }
        if v.superview.is_null() {
            return std::ptr::null_mut();
        }
        current = v.superview;
    }
    std::ptr::null_mut()
}

/// The backing scale a view draws at: its window's, or one.
fn scale(receiver: Id) -> f64 {
    // SAFETY: a view of this crate's.
    let window = unsafe { window_of(receiver) };
    // SAFETY: null or a window this crate made.
    let sdl = unsafe { crate::window::sdl_window(window) };
    match sdl.is_null() || view_of(receiver).wants_best_resolution == 0 {
        true => 1.0,
        false => sdl3::video::window_pixel_density(sdl) as f64,
    }
}

unsafe extern "C" fn init_with_frame(receiver: Id, _sel: Sel, frame: NSRect) -> Id {
    view_of(receiver).frame = frame;
    receiver
}

unsafe extern "C" fn frame(receiver: Id, _sel: Sel) -> NSRect {
    view_of(receiver).frame
}

unsafe extern "C" fn set_frame(receiver: Id, _sel: Sel, frame: NSRect) {
    view_of(receiver).frame = frame;
}

unsafe extern "C" fn set_frame_size(receiver: Id, _sel: Sel, size: NSSize) {
    view_of(receiver).frame.size = size;
}

unsafe extern "C" fn set_frame_origin(receiver: Id, _sel: Sel, origin: NSPoint) {
    view_of(receiver).frame.origin = origin;
}

/// `-bounds`: the frame's size at the origin, which is what a view with no
/// transform of its own has.
unsafe extern "C" fn bounds(receiver: Id, _sel: Sel) -> NSRect {
    NSRect {
        origin: NSPoint::default(),
        size: view_of(receiver).frame.size,
    }
}

unsafe extern "C" fn set_bounds(receiver: Id, _sel: Sel, bounds: NSRect) {
    view_of(receiver).frame.size = bounds.size;
}

/// `-visibleRect`: the part of the view not clipped away by an ancestor.
///
/// The bounds, because nothing in this tree clips: there is no scroll view and
/// no clip view with a smaller frame, so every view is wholly visible. A guest
/// sizing a viewport from this gets the same answer it would from `-bounds`,
/// which is what it would get from AppKit for a view that fills its window.
unsafe extern "C" fn visible_rect(receiver: Id, sel: Sel) -> NSRect {
    // SAFETY: `-bounds` is `NSRect (id, SEL)`.
    unsafe { bounds(receiver, sel) }
}

/// `-convertRectToBacking:`, which is the whole reason a Retina program asks a
/// view anything.
unsafe extern "C" fn convert_rect_to_backing(receiver: Id, _sel: Sel, rect: NSRect) -> NSRect {
    let scale = scale(receiver);
    NSRect {
        origin: NSPoint {
            x: rect.origin.x * scale,
            y: rect.origin.y * scale,
        },
        size: NSSize {
            width: rect.size.width * scale,
            height: rect.size.height * scale,
        },
    }
}

unsafe extern "C" fn convert_rect_from_backing(receiver: Id, _sel: Sel, rect: NSRect) -> NSRect {
    let scale = scale(receiver);
    NSRect {
        origin: NSPoint {
            x: rect.origin.x / scale,
            y: rect.origin.y / scale,
        },
        size: NSSize {
            width: rect.size.width / scale,
            height: rect.size.height / scale,
        },
    }
}

unsafe extern "C" fn convert_size_to_backing(receiver: Id, _sel: Sel, size: NSSize) -> NSSize {
    let scale = scale(receiver);
    NSSize {
        width: size.width * scale,
        height: size.height * scale,
    }
}

/// `-convertPoint:fromView:`.
///
/// Only the two cases that are answerable without a view hierarchy that has
/// positions in it: `nil`, which means the window, and the receiver itself. A
/// conversion between two sibling views would need each one's origin inside its
/// superview, which this tree does not track - so it is refused by name rather
/// than answered with the point unchanged, which would be off by wherever the
/// views actually are.
unsafe extern "C" fn convert_point_from_view(
    receiver: Id,
    _sel: Sel,
    point: NSPoint,
    from: Id,
) -> NSPoint {
    if from.is_null() || from == receiver {
        return point;
    }
    objc::fail!(
        "-[NSView convertPoint:fromView:]: a conversion between two views needs a view \
         hierarchy with positions in it, which this runtime does not have"
    );
}

unsafe extern "C" fn window(receiver: Id, _sel: Sel) -> Id {
    // SAFETY: a view of this crate's.
    unsafe { window_of(receiver) }
}

/// `-isInFullScreenMode`: the window's answer, because a view is only ever in
/// full screen by being in a window that is.
///
/// A view with no window is not, which is also what `-window` returning nil
/// means everywhere else here.
unsafe extern "C" fn is_in_full_screen_mode(receiver: Id, _sel: Sel) -> u8 {
    // SAFETY: a view of this crate's.
    let window = unsafe { window_of(receiver) };
    if window.is_null() {
        return 0;
    }
    let sel = objc::sel::get("isInFullScreenMode");
    // SAFETY: `-isInFullScreenMode` is `BOOL (id, SEL)` on a window this crate
    // made.
    unsafe {
        let f: unsafe extern "C" fn(Id, Sel) -> u8 = std::mem::transmute(objc::msg::entry());
        f(window, sel)
    }
}

unsafe extern "C" fn superview(receiver: Id, _sel: Sel) -> Id {
    view_of(receiver).superview
}

unsafe extern "C" fn subviews(receiver: Id, _sel: Sel) -> Id {
    let view = view_of(receiver);
    if view.subviews.is_null() {
        let array = corefoundation::collections::new_array(
            &[],
            corefoundation::callbacks::Policy::from_type_callbacks(),
            true,
        );
        view.subviews = array as Id;
    }
    view.subviews
}

unsafe extern "C" fn add_subview(receiver: Id, sel: Sel, subview: Id) {
    if !is_view(subview) {
        return;
    }
    view_of(subview).superview = receiver;
    // SAFETY: `-subviews` is `id (id, SEL)` and answers a mutable array.
    let list = unsafe { subviews(receiver, sel) };
    if let Some(array) = unsafe { corefoundation::collections::array(list as *const c_void) } {
        array.values.push(array.policy.retain(subview as *const c_void));
    }
}

unsafe extern "C" fn remove_from_superview(receiver: Id, _sel: Sel) {
    let view = view_of(receiver);
    let parent = view.superview;
    view.superview = std::ptr::null_mut();
    if !is_view(parent) {
        return;
    }
    let list = view_of(parent).subviews;
    if list.is_null() {
        return;
    }
    // SAFETY: an array this module made.
    if let Some(array) = unsafe { corefoundation::collections::array(list as *const c_void) } {
        array.values.retain(|v| *v != receiver as *const c_void);
    }
}

/// `-addTrackingArea:` and its two companions.
///
/// The area is kept in the subview-shaped list a view already has for it, so
/// `-removeTrackingArea:` can find the object it was given back. Nothing yet
/// *delivers* from one: mouse-entered and mouse-exited are the event pump's, and
/// a tracking area with no pump is a region that is watched by nobody - which is
/// what a program that has not yet had an event sees anyway.
unsafe extern "C" fn add_tracking_area(receiver: Id, _sel: Sel, area: Id) {
    let view = view_of(receiver);
    if view.tracking_areas.is_null() {
        let array = corefoundation::collections::new_array(
            &[],
            corefoundation::callbacks::Policy::from_type_callbacks(),
            true,
        );
        view.tracking_areas = array as Id;
    }
    // SAFETY: an array this module made.
    if let Some(array) =
        unsafe { corefoundation::collections::array(view.tracking_areas as *const c_void) }
    {
        array.values.push(array.policy.retain(area as *const c_void));
    }
}

unsafe extern "C" fn remove_tracking_area(receiver: Id, _sel: Sel, area: Id) {
    let list = view_of(receiver).tracking_areas;
    if list.is_null() {
        return;
    }
    // SAFETY: an array this module made.
    if let Some(array) = unsafe { corefoundation::collections::array(list as *const c_void) } {
        array.values.retain(|v| *v != area as *const c_void);
    }
}

unsafe extern "C" fn tracking_areas(receiver: Id, sel: Sel) -> Id {
    let view = view_of(receiver);
    if view.tracking_areas.is_null() {
        // SAFETY: `-addTrackingArea:` is `void (id, SEL, id)`, and adding nil
        // is what makes the empty list without adding anything to it.
        unsafe { add_tracking_area(receiver, sel, std::ptr::null_mut()) };
        // SAFETY: as above.
        unsafe { remove_tracking_area(receiver, sel, std::ptr::null_mut()) };
    }
    view.tracking_areas
}

unsafe extern "C" fn set_wants_best_resolution(receiver: Id, _sel: Sel, wants: u8) {
    view_of(receiver).wants_best_resolution = wants as usize;
}

unsafe extern "C" fn wants_best_resolution(receiver: Id, _sel: Sel) -> u8 {
    (view_of(receiver).wants_best_resolution != 0) as u8
}

/// Answered as true, because a view here is always ready to be drawn into.
unsafe extern "C" fn yes(_receiver: Id, _sel: Sel) -> u8 {
    1
}

unsafe extern "C" fn no(_receiver: Id, _sel: Sel) -> u8 {
    0
}

/// `-display`: draw the view now.
///
/// **This is what calls `-drawRect:`,** and a no-op here is a program that
/// renders nothing while looking like it is running. Deltarune's frame is
/// exactly this path: its display link sends `-[YYGLView display]`, AppKit turns
/// that into `-drawRect:`, and the guest's override makes its GL context current
/// and swaps. With `-display` empty the game ticks, the display link fires, and
/// the window stays black.
///
/// Sent rather than called, so the *guest's* override is what runs. A view with
/// no `-drawRect:` of its own draws nothing, which is what an empty view does.
unsafe extern "C" fn display(receiver: Id, sel: Sel) {
    let sel_draw = objc::sel::get("drawRect:");
    // SAFETY: a live view; `-bounds` is `NSRect (id, SEL)`.
    let rect = unsafe { bounds(receiver, sel) };
    // SAFETY: a live object this runtime allocated.
    let responds =
        unsafe { objc::object::class_of(receiver) }.is_some_and(|c| c.method(sel_draw).is_some());
    if !responds {
        return;
    }
    // SAFETY: `-drawRect:` is `void (id, SEL, NSRect)`.
    unsafe {
        let f: unsafe extern "C" fn(Id, Sel, NSRect) = std::mem::transmute(objc::msg::entry());
        f(receiver, sel_draw, rect);
    }
}

unsafe extern "C" fn ignored(_receiver: Id, _sel: Sel) {}

unsafe extern "C" fn ignored_flag(_receiver: Id, _sel: Sel, _flag: u8) {}

unsafe extern "C" fn ignored_rect(_receiver: Id, _sel: Sel, _rect: NSRect) {}

unsafe extern "C" fn ignored_object_object(_receiver: Id, _sel: Sel, _a: Id, _b: Id) {}

unsafe extern "C" fn backing_scale_factor(receiver: Id, _sel: Sel) -> CGFloat {
    scale(receiver)
}

pub(crate) fn install() {
    let class = objc::class::lookup(c"NSView").expect("declared before its methods");
    methods!(class, {
        "initWithFrame:" => init_with_frame, c"@48@0:8{CGRect={CGPoint=dd}{CGSize=dd}}16";
        "frame" => frame, c"{CGRect={CGPoint=dd}{CGSize=dd}}16@0:8";
        "setFrame:" => set_frame, c"v48@0:8{CGRect={CGPoint=dd}{CGSize=dd}}16";
        "setFrameSize:" => set_frame_size, c"v32@0:8{CGSize=dd}16";
        "setFrameOrigin:" => set_frame_origin, c"v32@0:8{CGPoint=dd}16";
        "bounds" => bounds, c"{CGRect={CGPoint=dd}{CGSize=dd}}16@0:8";
        "visibleRect" => visible_rect, c"{CGRect={CGPoint=dd}{CGSize=dd}}16@0:8";
        "setBounds:" => set_bounds, c"v48@0:8{CGRect={CGPoint=dd}{CGSize=dd}}16";
        "convertRectToBacking:" => convert_rect_to_backing,
            c"{CGRect={CGPoint=dd}{CGSize=dd}}48@0:8{CGRect={CGPoint=dd}{CGSize=dd}}16";
        "convertRectFromBacking:" => convert_rect_from_backing,
            c"{CGRect={CGPoint=dd}{CGSize=dd}}48@0:8{CGRect={CGPoint=dd}{CGSize=dd}}16";
        "convertSizeToBacking:" => convert_size_to_backing, c"{CGSize=dd}32@0:8{CGSize=dd}16";
        "convertPoint:fromView:" => convert_point_from_view, c"{CGPoint=dd}40@0:8{CGPoint=dd}16@32";
        "window" => window, c"@16@0:8";
        "isInFullScreenMode" => is_in_full_screen_mode, c"c16@0:8";
        "superview" => superview, c"@16@0:8";
        "subviews" => subviews, c"@16@0:8";
        "addSubview:" => add_subview, c"v24@0:8@16";
        "removeFromSuperview" => remove_from_superview, c"v16@0:8";
        "setWantsBestResolutionOpenGLSurface:" => set_wants_best_resolution, c"v20@0:8c16";
        "wantsBestResolutionOpenGLSurface" => wants_best_resolution, c"c16@0:8";
        "backingScaleFactor" => backing_scale_factor, c"d16@0:8";
        "acceptsFirstResponder" => yes, c"c16@0:8";
        "canBecomeKeyView" => yes, c"c16@0:8";
        "isOpaque" => yes, c"c16@0:8";
        "isFlipped" => no, c"c16@0:8";
        "isHidden" => no, c"c16@0:8";
        "display" => display, c"v16@0:8";
        "displayIfNeeded" => display, c"v16@0:8";
        "lockFocus" => ignored, c"v16@0:8";
        "unlockFocus" => ignored, c"v16@0:8";
        "setNeedsDisplay:" => ignored_flag, c"v20@0:8c16";
        "setHidden:" => ignored_flag, c"v20@0:8c16";
        "setAutoresizingMask:" => ignored_flag, c"v24@0:8Q16";
        "setNeedsDisplayInRect:" => ignored_rect, c"v48@0:8{CGRect={CGPoint=dd}{CGSize=dd}}16";
        "addCursorRect:cursor:" => ignored_object_object, c"v56@0:8{CGRect={CGPoint=dd}{CGSize=dd}}16@48";
        "discardCursorRects" => ignored, c"v16@0:8";
        "updateTrackingAreas" => ignored, c"v16@0:8";
        "addTrackingArea:" => add_tracking_area, c"v24@0:8@16";
        "removeTrackingArea:" => remove_tracking_area, c"v24@0:8@16";
        "trackingAreas" => tracking_areas, c"@16@0:8";
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view(frame: NSRect) -> Id {
        crate::bootstrap();
        let class = objc::class::lookup(c"NSView").expect("NSView");
        let object = objc::object::create_instance(class, 0);
        // SAFETY: `-initWithFrame:` is `id (id, SEL, NSRect)`.
        unsafe { init_with_frame(object, objc::sel::get("initWithFrame:"), frame) }
    }

    /// A view with no window scales by one, whatever it was asked - which is
    /// the case a headless test and a program before `-setContentView:` are in.
    #[test]
    fn a_view_with_no_window_does_not_scale_its_backing() {
        let rect = NSRect {
            origin: NSPoint::default(),
            size: NSSize {
                width: 640.0,
                height: 480.0,
            },
        };
        let v = view(rect);
        // SAFETY: `-convertRectToBacking:` is `NSRect (id, SEL, NSRect)`.
        let backing =
            unsafe { convert_rect_to_backing(v, objc::sel::get("convertRectToBacking:"), rect) };
        assert_eq!(backing, rect);
    }

    /// A subview knows its superview, and the superview lists it.
    #[test]
    fn a_subview_is_in_its_superviews_list() {
        let parent = view(NSRect::default());
        let child = view(NSRect::default());
        // SAFETY: `-addSubview:` is `void (id, SEL, id)`.
        unsafe { add_subview(parent, objc::sel::get("addSubview:"), child) };
        // SAFETY: `-superview` is `id (id, SEL)`.
        assert_eq!(unsafe { superview(child, objc::sel::get("superview")) }, parent);
        // SAFETY: `-subviews` is `id (id, SEL)`.
        let list = unsafe { subviews(parent, objc::sel::get("subviews")) };
        // SAFETY: an array this module made.
        let array = unsafe { corefoundation::collections::array(list as *const c_void) }
            .expect("an array");
        assert_eq!(array.values.len(), 1);

        // SAFETY: `-removeFromSuperview` is `void (id, SEL)`.
        unsafe { remove_from_superview(child, objc::sel::get("removeFromSuperview")) };
        // SAFETY: as above.
        let array = unsafe { corefoundation::collections::array(list as *const c_void) }
            .expect("an array");
        assert!(array.values.is_empty());
    }

    /// A view finds its window through its superview, which is how a guest's
    /// `YYGLView` inside a content view reaches one.
    #[test]
    fn a_view_finds_its_window_up_the_chain() {
        let parent = view(NSRect::default());
        let child = view(NSRect::default());
        // SAFETY: `-addSubview:` is `void (id, SEL, id)`.
        unsafe { add_subview(parent, objc::sel::get("addSubview:"), child) };
        let window = objc::object::create_instance(
            objc::class::lookup(c"NSWindow").expect("NSWindow"),
            0,
        );
        // SAFETY: both are objects this runtime allocated.
        unsafe { set_window(parent, window) };
        // SAFETY: as above.
        assert_eq!(unsafe { window_of(child) }, window);
    }
}
