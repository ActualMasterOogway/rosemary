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
    /// Retained, as AppKit's is: a layer outlives the call that set it.
    layer: Id,
    /// `-setAllowedTouchTypes:`, stored so `-allowedTouchTypes` reads back what
    /// was set. Nothing in this tree delivers a touch event, so the mask only
    /// ever says which ones the view *would* accept.
    allowed_touch_types: usize,
    frame: NSRect,
    wants_best_resolution: usize,
    /// `-setPostsFrameChangedNotifications:` and its bounds twin, as the two
    /// low bits. A view posts nothing until it is asked to, which is AppKit's
    /// default for a view built with `-initWithFrame:`.
    posts: usize,
    /// `NSAutoresizingMaskOptions`, which says which of the six lengths around
    /// and inside this view give when its superview resizes. See
    /// [`resize_subviews`].
    autoresizing_mask: usize,
    /// `-setAutoresizesSubviews:`, which is `YES` on a fresh view - so the
    /// field is stored inverted, and zero means the AppKit default.
    does_not_autoresize_subviews: usize,
}

/// The bits of [`View::posts`].
const POSTS_FRAME: usize = 1;
const POSTS_BOUNDS: usize = 2;

/// `NSAutoresizingMaskOptions`.
mod autoresizing {
    pub const MIN_X_MARGIN: usize = 1;
    pub const WIDTH_SIZABLE: usize = 2;
    pub const MAX_X_MARGIN: usize = 4;
    pub const MIN_Y_MARGIN: usize = 8;
    pub const HEIGHT_SIZABLE: usize = 16;
    pub const MAX_Y_MARGIN: usize = 32;
}

/// The notification a view posts when its frame changes.
pub const FRAME_DID_CHANGE: &str = "NSViewFrameDidChangeNotification";
/// ...and when its bounds do.
pub const BOUNDS_DID_CHANGE: &str = "NSViewBoundsDidChangeNotification";

/// Post the frame and bounds notifications a resized view owes its observers.
///
/// Both, because this tree's `-bounds` is the frame's size at the origin: a
/// change to one *is* a change to the other, and a guest watching only the
/// bounds - which is what `-setPostsBoundsChangedNotifications:` asks for -
/// would otherwise never hear that the view resized.
fn note_geometry_change(receiver: Id, before: NSRect) {
    let view = view_of(receiver);
    if view.frame == before {
        return;
    }
    if view.frame.size != before.size {
        resize_subviews(receiver, before.size);
    }
    let view = view_of(receiver);
    let posts = view.posts;
    if posts & POSTS_FRAME != 0 {
        foundation::notification::post(FRAME_DID_CHANGE, receiver, std::ptr::null_mut());
    }
    if posts & POSTS_BOUNDS != 0 && view.frame.size != before.size {
        foundation::notification::post(BOUNDS_DID_CHANGE, receiver, std::ptr::null_mut());
    }
}

/// Resize `receiver`'s subviews for a superview that went from `old` to its
/// current size, as `-resizeSubviewsWithOldSize:` does.
///
/// This is what makes a window resize reach the view a program actually draws
/// into. AppKit resizes subviews on *every* frame change of their superview,
/// and a program's GL view is a subview of the window's content view - so
/// without this the content view follows the window and the view underneath it
/// keeps its original frame forever. The program then asks its own view how big
/// it is, gets the size the window had when it opened, and renders that size
/// for the rest of the run: a window that resizes and a picture that does not.
///
/// Each subview is *sent* `-resizeWithOldSuperviewSize:` rather than having its
/// frame written here, because that is the method AppKit documents as the one a
/// view overrides to lay itself out - so a guest's override has to be what runs.
fn resize_subviews(receiver: Id, old: NSSize) {
    let view = view_of(receiver);
    if view.does_not_autoresize_subviews != 0 || view.subviews.is_null() {
        return;
    }
    let list = view.subviews;
    // SAFETY: an array this module made and only ever fills with views.
    let Some(array) = (unsafe { corefoundation::collections::array(list as *const c_void) }) else {
        return;
    };
    let subviews: Vec<Id> = array.values.iter().map(|v| *v as Id).collect();
    let sel = objc::sel::get("resizeWithOldSuperviewSize:");
    for subview in subviews {
        if !is_view(subview) {
            continue;
        }
        // SAFETY: `-resizeWithOldSuperviewSize:` is `void (id, SEL, NSSize)`.
        unsafe {
            let f: unsafe extern "C" fn(Id, Sel, NSSize) = std::mem::transmute(objc::msg::entry());
            f(subview, sel, old);
        }
    }
}

/// `-resizeWithOldSuperviewSize:`: put this view where its autoresizing mask
/// says it belongs, now that its superview has gone from `old` to its current
/// size.
///
/// A view whose mask is `NSViewNotSizable` keeps its frame, which is a real
/// answer and not a gap: a program that positions a fixed-size control in a
/// resizing window means it.
unsafe extern "C" fn resize_with_old_superview_size(receiver: Id, _sel: Sel, old: NSSize) {
    let view = view_of(receiver);
    let superview = view.superview;
    if !is_view(superview) {
        return;
    }
    let new = view_of(superview).frame.size;
    let mask = view.autoresizing_mask;
    let before = view.frame;
    let (x, width) = distribute(
        mask & autoresizing::MIN_X_MARGIN != 0,
        mask & autoresizing::WIDTH_SIZABLE != 0,
        mask & autoresizing::MAX_X_MARGIN != 0,
        before.origin.x,
        before.size.width,
        old.width,
        new.width,
    );
    let (y, height) = distribute(
        mask & autoresizing::MIN_Y_MARGIN != 0,
        mask & autoresizing::HEIGHT_SIZABLE != 0,
        mask & autoresizing::MAX_Y_MARGIN != 0,
        before.origin.y,
        before.size.height,
        old.height,
        new.height,
    );
    let frame = NSRect {
        origin: NSPoint { x, y },
        size: NSSize { width, height },
    };
    if frame == before {
        return;
    }
    // Sent rather than written, for the reason `-[NSWindow setContentView:]`
    // sends it: a view that tracks its own size overrides `-setFrame:`, and
    // this path also has to reach the subviews and observers of *this* view.
    // SAFETY: `-setFrame:` is `void (id, SEL, NSRect)`.
    unsafe {
        let f: unsafe extern "C" fn(Id, Sel, NSRect) = std::mem::transmute(objc::msg::entry());
        f(receiver, objc::sel::get("setFrame:"), frame);
    }
}

/// Spread a superview's change along one axis over the three lengths that make
/// it up: the leading margin, the subview itself, and the trailing margin.
///
/// Only the flexible ones give, and they give in proportion to their current
/// sizes - which is AppKit's rule and the one that matters, because a view that
/// is `WidthSizable` with two fixed margins must absorb the whole change. When
/// every flexible length is currently zero the change is split evenly between
/// them instead, since proportional shares of nothing are all nothing and the
/// view would not move at all.
///
/// Nothing flexible means nothing moves: `NSViewNotSizable` is a real answer
/// and a view that asked for it keeps its frame.
#[allow(clippy::too_many_arguments)]
fn distribute(
    leading: bool,
    sizable: bool,
    trailing: bool,
    origin: CGFloat,
    size: CGFloat,
    old: CGFloat,
    new: CGFloat,
) -> (CGFloat, CGFloat) {
    let delta = new - old;
    if delta == 0.0 {
        return (origin, size);
    }
    let trailing_margin = old - origin - size;
    let parts = [
        (leading, origin),
        (sizable, size),
        (trailing, trailing_margin),
    ];
    let flexible: Vec<CGFloat> = parts
        .iter()
        .filter(|(is, _)| *is)
        .map(|(_, length)| *length)
        .collect();
    if flexible.is_empty() {
        return (origin, size);
    }
    let total: CGFloat = flexible.iter().sum();
    let share = |length: CGFloat| match total > 0.0 {
        true => delta * (length / total),
        false => delta / flexible.len() as CGFloat,
    };
    let origin = match leading {
        true => origin + share(origin),
        false => origin,
    };
    let size = match sizable {
        true => (size + share(size)).max(0.0),
        false => size,
    };
    (origin, size)
}

unsafe extern "C" fn set_autoresizing_mask(receiver: Id, _sel: Sel, mask: usize) {
    view_of(receiver).autoresizing_mask = mask;
}

unsafe extern "C" fn autoresizing_mask(receiver: Id, _sel: Sel) -> usize {
    view_of(receiver).autoresizing_mask
}

unsafe extern "C" fn set_autoresizes_subviews(receiver: Id, _sel: Sel, flag: u8) {
    view_of(receiver).does_not_autoresize_subviews = usize::from(flag == 0);
}

unsafe extern "C" fn autoresizes_subviews(receiver: Id, _sel: Sel) -> u8 {
    u8::from(view_of(receiver).does_not_autoresize_subviews == 0)
}

/// `-resizeSubviewsWithOldSize:`, which a program may call itself and which an
/// `NSView` subclass overriding `-resizeWithOldSuperviewSize:` expects to be
/// the thing that drives it.
unsafe extern "C" fn resize_subviews_with_old_size(receiver: Id, _sel: Sel, old: NSSize) {
    resize_subviews(receiver, old);
}

unsafe extern "C" fn set_posts_frame_changed(receiver: Id, _sel: Sel, flag: u8) {
    set_posts(receiver, POSTS_FRAME, flag);
}

unsafe extern "C" fn set_posts_bounds_changed(receiver: Id, _sel: Sel, flag: u8) {
    set_posts(receiver, POSTS_BOUNDS, flag);
}

fn set_posts(receiver: Id, bit: usize, flag: u8) {
    let view = view_of(receiver);
    view.posts = match flag != 0 {
        true => view.posts | bit,
        false => view.posts & !bit,
    };
}

unsafe extern "C" fn posts_frame_changed(receiver: Id, _sel: Sel) -> u8 {
    u8::from(view_of(receiver).posts & POSTS_FRAME != 0)
}

unsafe extern "C" fn posts_bounds_changed(receiver: Id, _sel: Sel) -> u8 {
    u8::from(view_of(receiver).posts & POSTS_BOUNDS != 0)
}

/// `-setLayer:`, which stores the layer and nothing more.
///
/// Nothing in this tree composites one: a guest that hands its view a
/// `CAMetalLayer` draws through the layer's own drawables, which are Metal's
/// business rather than the view's. What the view owes it is to give the same
/// object back from `-layer`, because that is where the guest reaches for it on
/// every frame.
unsafe extern "C" fn set_layer(receiver: Id, _sel: Sel, layer: Id) {
    let view = view_of(receiver);
    if view.layer == layer {
        return;
    }
    let previous = view.layer;
    view.layer = objc::arc::retain(layer);
    objc::arc::release(previous);
}

unsafe extern "C" fn layer(receiver: Id, _sel: Sel) -> Id {
    view_of(receiver).layer
}

unsafe extern "C" fn set_allowed_touch_types(receiver: Id, _sel: Sel, mask: usize) {
    view_of(receiver).allowed_touch_types = mask;
}

unsafe extern "C" fn allowed_touch_types(receiver: Id, _sel: Sel) -> usize {
    view_of(receiver).allowed_touch_types
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

/// One line per view, describing the tree under `view`.
///
/// The question this exists for is "what size does the *guest's* view think it
/// is", and it cannot be answered any other way: a release build inlines the
/// whole geometry path out of gdb's reach - `appkit::event::post` and
/// `foundation::notification::post` both resolve to "Function not defined" -
/// and the view that matters is one the guest made, so it has no symbol either.
///
/// # Safety
///
/// `view` must be null or an object this runtime allocated.
pub unsafe fn describe(view: Id, depth: usize, out: &mut String) {
    use std::fmt::Write;

    if !is_view(view) || depth > 16 {
        return;
    }
    let v = view_of(view);
    // The class is what names it: `NSView` is this crate's, and anything else is
    // the guest's own subclass, which is the one the question is about.
    // SAFETY: a live object.
    let class = unsafe { objc::object::class_of(view) }.map_or("?", objc::class::Class::name_str);
    let _ = writeln!(
        out,
        "rosetta3: appkit: {:indent$}{class} {view:p} frame {}x{} at ({},{}) mask {:#x}{}",
        "",
        v.frame.size.width,
        v.frame.size.height,
        v.frame.origin.x,
        v.frame.origin.y,
        v.autoresizing_mask,
        match v.does_not_autoresize_subviews {
            0 => "",
            _ => " autoresizesSubviews=NO",
        },
        indent = depth * 2,
    );
    // SAFETY: the subview list is an array of this runtime's, holding views.
    let children = unsafe { corefoundation::collections::array(v.subviews as *const c_void) };
    for child in children.map(|a| a.values.clone()).unwrap_or_default() {
        // SAFETY: an object this runtime allocated.
        unsafe { describe(child as Id, depth + 1, out) };
    }
}

/// The deepest view under a point, as `-[NSView hitTest:]` finds it.
///
/// `point` is in the *superview's* coordinates, which is the argument AppKit's
/// takes. Subviews are tried last-first, because the last one added is the one
/// drawn on top.
///
/// This is what routes a mouse event, and routing one any other way is not a
/// detail: AppKit sends a click to the view it landed on and a key to the first
/// responder, so a runtime that sent both to the responder delivers every click
/// to whatever was focused last. Roblox focuses a zero-sized
/// `InputMethodHandler` that implements no mouse method at all, so every click
/// in the program was being dropped.
///
/// # Safety
///
/// `view` must be null or an object this runtime allocated.
pub unsafe fn hit_test(view: Id, point: NSPoint) -> Id {
    // SAFETY: the caller's obligation.
    unsafe { hit(view, point, 0) }
}

/// Where the pointer is in this view's own coordinates, or `None` for a view
/// in no window - which is every view before it is installed, and the point
/// where a cursor rectangle cannot be about the pointer at all.
///
/// The walk up the superviews does both halves: it finds the window, since only
/// a content view carries one directly and Roblox installs its view with
/// `-addSubview:` rather than `-setContentView:`, and it converts, since a
/// view's frame is stated in its superview's coordinates.
///
/// # Safety
///
/// `view` must be null or an object this runtime allocated.
pub unsafe fn pointer_in(view: Id) -> Option<NSPoint> {
    if !is_view(view) {
        return None;
    }
    // SAFETY: the caller's obligation.
    let window = unsafe { window_of(view) };
    if window.is_null() {
        return None;
    }
    let mut point = crate::window::pointer_location(window);
    let mut node = view;
    for _ in 0..32 {
        if !is_view(node) {
            break;
        }
        let origin = view_of(node).frame.origin;
        point.x -= origin.x;
        point.y -= origin.y;
        node = view_of(node).superview;
    }
    Some(point)
}

/// Whether a rectangle of a view's holds a point, for [`crate::cursor`].
pub fn holds(view: Id, rect: NSRect, point: NSPoint) -> bool {
    is_view(view) && contains(rect, point)
}

/// The subviews of a view, for a caller walking the tree outside this module.
///
/// # Safety
///
/// `view` must be null or an object this runtime allocated.
pub unsafe fn children(view: Id) -> Vec<Id> {
    if !is_view(view) {
        return Vec::new();
    }
    // SAFETY: the subview list is an array of this runtime's, holding views.
    let list =
        unsafe { corefoundation::collections::array(view_of(view).subviews as *const c_void) };
    list.map(|a| a.values.iter().map(|v| *v as Id).collect())
        .unwrap_or_default()
}

unsafe fn hit(view: Id, point: NSPoint, depth: usize) -> Id {
    if !is_view(view) || depth > 32 {
        return std::ptr::null_mut();
    }
    // A guest's own `-hitTest:` is the answer where it has one: a view that
    // reshapes its hit region - a round button, a transparent overlay - is
    // saying so in code this crate cannot see from the frame.
    let sel = objc::sel::get("hitTest:");
    // SAFETY: a live object this runtime allocated.
    if let Some(method) = unsafe { objc::object::class_of(view) }.and_then(|c| c.method(sel)) {
        // SAFETY: `-hitTest:` is `id (id, SEL, NSPoint)`.
        return unsafe {
            let f: unsafe extern "C" fn(Id, Sel, NSPoint) -> Id =
                std::mem::transmute(method.implementation());
            f(view, sel, point)
        };
    }
    let frame = view_of(view).frame;
    if !contains(frame, point) {
        return std::ptr::null_mut();
    }
    let local = NSPoint {
        x: point.x - frame.origin.x,
        y: point.y - frame.origin.y,
    };
    // SAFETY: the subview list is an array of this runtime's, holding views.
    let children =
        unsafe { corefoundation::collections::array(view_of(view).subviews as *const c_void) };
    for child in children
        .map(|a| a.values.clone())
        .unwrap_or_default()
        .into_iter()
        .rev()
    {
        // SAFETY: an object this runtime allocated.
        let hit = unsafe { hit(child as Id, local, depth + 1) };
        if !hit.is_null() {
            return hit;
        }
    }
    view
}

/// Whether a rectangle holds a point, on AppKit's half-open rule: the minimum
/// edges are inside and the maximum ones are not, so two views that share an
/// edge do not both claim the click.
fn contains(rect: NSRect, point: NSPoint) -> bool {
    point.x >= rect.origin.x
        && point.y >= rect.origin.y
        && point.x < rect.origin.x + rect.size.width
        && point.y < rect.origin.y + rect.size.height
}

/// Send `-updateTrackingAreas` down a view tree.
///
/// AppKit sends it when a view is added to a window and whenever the geometry
/// changes, and it is the *only* time a program is asked to install its
/// tracking areas: a view's `-updateTrackingAreas` is where an application
/// removes the old area and adds one for its new bounds. A runtime that never
/// sends it has a program that never adds one - so `-mouseMoved:`,
/// `-mouseEntered:` and `-mouseExited:` are all events nothing ever asked for,
/// and the pointer appears to a game to be standing still.
///
/// # Safety
///
/// `view` must be null or an object this runtime allocated.
pub unsafe fn update_tracking_areas(view: Id) {
    if !is_view(view) {
        return;
    }
    let sel = objc::sel::get("updateTrackingAreas");
    // SAFETY: a live object this runtime allocated.
    if let Some(method) = unsafe { objc::object::class_of(view) }.and_then(|c| c.method(sel)) {
        // SAFETY: `-updateTrackingAreas` is `void (id, SEL)`.
        unsafe {
            let f: unsafe extern "C" fn(Id, Sel) = std::mem::transmute(method.implementation());
            f(view, sel);
        }
    }
    // SAFETY: the subview list is an array of this runtime's, holding views.
    let children =
        unsafe { corefoundation::collections::array(view_of(view).subviews as *const c_void) };
    for child in children.map(|a| a.values.clone()).unwrap_or_default() {
        // SAFETY: an object this runtime allocated.
        unsafe { update_tracking_areas(child as Id) };
    }
}

/// Whether any view in this tree has a tracking area.
///
/// Not *which* area and not its rectangle: the question a caller has is whether
/// the program asked to be told about the pointer at all, and a view that added
/// one covering part of itself still wants `-mouseMoved:` while the pointer is
/// in it. Reporting per-rectangle would need enter and exit events, which this
/// crate does not deliver.
///
/// # Safety
///
/// `view` must be null or an object this runtime allocated.
pub unsafe fn any_tracking_area(view: Id) -> bool {
    if !is_view(view) {
        return false;
    }
    let v = view_of(view);
    // SAFETY: the list is an array of this runtime's.
    let areas = unsafe { corefoundation::collections::array(v.tracking_areas as *const c_void) };
    if areas.is_some_and(|a| !a.values.is_empty()) {
        return true;
    }
    // SAFETY: the subview list is an array of this runtime's, holding views.
    let children = unsafe { corefoundation::collections::array(v.subviews as *const c_void) };
    children
        .map(|a| a.values.clone())
        .unwrap_or_default()
        .into_iter()
        // SAFETY: an object this runtime allocated.
        .any(|child| unsafe { any_tracking_area(child as Id) })
}

/// The view above this one, for walking the responder chain.
///
/// # Safety
///
/// As [`hit_test`].
pub unsafe fn superview_of(view: Id) -> Id {
    match is_view(view) {
        true => view_of(view).superview,
        false => std::ptr::null_mut(),
    }
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
    let before = view_of(receiver).frame;
    view_of(receiver).frame = frame;
    note_geometry_change(receiver, before);
}

unsafe extern "C" fn set_frame_size(receiver: Id, _sel: Sel, size: NSSize) {
    let before = view_of(receiver).frame;
    view_of(receiver).frame.size = size;
    note_geometry_change(receiver, before);
}

unsafe extern "C" fn set_frame_origin(receiver: Id, _sel: Sel, origin: NSPoint) {
    let before = view_of(receiver).frame;
    view_of(receiver).frame.origin = origin;
    note_geometry_change(receiver, before);
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
    let before = view_of(receiver).frame;
    view_of(receiver).frame.size = bounds.size;
    note_geometry_change(receiver, before);
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
        array
            .values
            .push(array.policy.retain(subview as *const c_void));
    }
    // SAFETY: a view this crate made, now in a window.
    unsafe { update_tracking_areas(subview) };
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
        array
            .values
            .push(array.policy.retain(area as *const c_void));
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

unsafe extern "C" fn ignored_integer(_receiver: Id, _sel: Sel, _value: isize) {}

unsafe extern "C" fn ignored_rect(_receiver: Id, _sel: Sel, _rect: NSRect) {}

/// `-addCursorRect:cursor:`, which is only ever called from inside a view's own
/// `-resetCursorRects`: the rectangle it records lasts until the next
/// `-discardCursorRects`, and [`crate::cursor`] decides from the whole set
/// which cursor the pointer is over.
unsafe extern "C" fn add_cursor_rect(receiver: Id, _sel: Sel, rect: NSRect, cursor: Id) {
    crate::cursor::add_rect(receiver, rect, cursor);
}

unsafe extern "C" fn discard_cursor_rects(receiver: Id, _sel: Sel) {
    crate::cursor::discard_rects(receiver);
}

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
        "setAutoresizingMask:" => set_autoresizing_mask, c"v24@0:8Q16";
        "autoresizingMask" => autoresizing_mask, c"Q16@0:8";
        "setAutoresizesSubviews:" => set_autoresizes_subviews, c"v20@0:8c16";
        "autoresizesSubviews" => autoresizes_subviews, c"c16@0:8";
        "resizeSubviewsWithOldSize:" => resize_subviews_with_old_size,
            c"v32@0:8{CGSize=dd}16";
        "resizeWithOldSuperviewSize:" => resize_with_old_superview_size,
            c"v32@0:8{CGSize=dd}16";
        "setNeedsDisplayInRect:" => ignored_rect, c"v48@0:8{CGRect={CGPoint=dd}{CGSize=dd}}16";
        "addCursorRect:cursor:" => add_cursor_rect,
            c"v56@0:8{CGRect={CGPoint=dd}{CGSize=dd}}16@48";
        "discardCursorRects" => discard_cursor_rects, c"v16@0:8";
        "updateTrackingAreas" => ignored, c"v16@0:8";
        "addTrackingArea:" => add_tracking_area, c"v24@0:8@16";
        "removeTrackingArea:" => remove_tracking_area, c"v24@0:8@16";
        "trackingAreas" => tracking_areas, c"@16@0:8";
        "setPostsFrameChangedNotifications:" => set_posts_frame_changed, c"v20@0:8c16";
        "setPostsBoundsChangedNotifications:" => set_posts_bounds_changed, c"v20@0:8c16";
        "postsFrameChangedNotifications" => posts_frame_changed, c"c16@0:8";
        "postsBoundsChangedNotifications" => posts_bounds_changed, c"c16@0:8";
        "setLayer:" => set_layer, c"v24@0:8@16";
        "layer" => layer, c"@16@0:8";
        "setWantsLayer:" => ignored_flag, c"v20@0:8c16";
        // A hint about when the backing store may be reused across a resize.
        // Nothing here keeps one, so every policy draws the same.
        "setLayerContentsRedrawPolicy:" => ignored_integer, c"v24@0:8q16";
        "setLayerContentsPlacement:" => ignored_integer, c"v24@0:8q16";
        "wantsLayer" => yes, c"c16@0:8";
        "setAllowedTouchTypes:" => set_allowed_touch_types, c"v24@0:8Q16";
        "allowedTouchTypes" => allowed_touch_types, c"Q16@0:8";
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

    fn rect(x: f64, y: f64, width: f64, height: f64) -> NSRect {
        NSRect {
            origin: NSPoint { x, y },
            size: NSSize { width, height },
        }
    }

    /// A superview and one subview of the given mask, wired as `-addSubview:`
    /// wires them.
    fn tree(parent: NSRect, child: NSRect, mask: usize) -> (Id, Id) {
        let superview = view(parent);
        let subview = view(child);
        // SAFETY: `-addSubview:` is `void (id, SEL, id)`.
        unsafe { add_subview(superview, objc::sel::get("addSubview:"), subview) };
        view_of(subview).autoresizing_mask = mask;
        (superview, subview)
    }

    fn set_frame_of(view: Id, frame: NSRect) {
        // SAFETY: `-setFrame:` is `void (id, SEL, NSRect)`.
        unsafe { set_frame(view, objc::sel::get("setFrame:"), frame) };
    }

    /// The case every windowed program is in: a render view that fills the
    /// content view, and a window the person resizes. Without this the program
    /// asks its own view how big it is and gets the size the window opened at.
    #[test]
    fn a_sizable_subview_follows_its_superview() {
        let (superview, subview) = tree(
            rect(0.0, 0.0, 1000.0, 665.0),
            rect(0.0, 0.0, 1000.0, 665.0),
            autoresizing::WIDTH_SIZABLE | autoresizing::HEIGHT_SIZABLE,
        );
        set_frame_of(superview, rect(0.0, 0.0, 1200.0, 760.0));
        assert_eq!(view_of(subview).frame, rect(0.0, 0.0, 1200.0, 760.0));
    }

    /// `NSViewNotSizable` is a real answer: a fixed-size control in a resizing
    /// window keeps both its size and its place.
    #[test]
    fn a_not_sizable_subview_keeps_its_frame() {
        let (superview, subview) = tree(
            rect(0.0, 0.0, 400.0, 400.0),
            rect(10.0, 10.0, 80.0, 20.0),
            0,
        );
        set_frame_of(superview, rect(0.0, 0.0, 800.0, 400.0));
        assert_eq!(view_of(subview).frame, rect(10.0, 10.0, 80.0, 20.0));
    }

    /// A view that is sizable with a fixed leading margin absorbs the whole
    /// change and stays where it started.
    #[test]
    fn a_fixed_margin_holds_a_sizable_subview_in_place() {
        let (superview, subview) = tree(
            rect(0.0, 0.0, 200.0, 100.0),
            rect(20.0, 0.0, 180.0, 100.0),
            autoresizing::WIDTH_SIZABLE,
        );
        set_frame_of(superview, rect(0.0, 0.0, 300.0, 100.0));
        assert_eq!(view_of(subview).frame, rect(20.0, 0.0, 280.0, 100.0));
    }

    /// A resize reaches all the way down, because a subview that resized has
    /// subviews of its own to tell.
    #[test]
    fn a_resize_reaches_a_grandchild() {
        let both = autoresizing::WIDTH_SIZABLE | autoresizing::HEIGHT_SIZABLE;
        let (superview, subview) = tree(
            rect(0.0, 0.0, 100.0, 100.0),
            rect(0.0, 0.0, 100.0, 100.0),
            both,
        );
        let grandchild = view(rect(0.0, 0.0, 100.0, 100.0));
        // SAFETY: `-addSubview:` is `void (id, SEL, id)`.
        unsafe { add_subview(subview, objc::sel::get("addSubview:"), grandchild) };
        view_of(grandchild).autoresizing_mask = both;
        set_frame_of(superview, rect(0.0, 0.0, 200.0, 50.0));
        assert_eq!(view_of(grandchild).frame, rect(0.0, 0.0, 200.0, 50.0));
    }

    /// A superview told not to autoresize its subviews does not.
    #[test]
    fn autoresizes_subviews_no_leaves_the_subviews_alone() {
        let (superview, subview) = tree(
            rect(0.0, 0.0, 100.0, 100.0),
            rect(0.0, 0.0, 100.0, 100.0),
            autoresizing::WIDTH_SIZABLE | autoresizing::HEIGHT_SIZABLE,
        );
        // SAFETY: `-setAutoresizesSubviews:` is `void (id, SEL, BOOL)`.
        unsafe {
            set_autoresizes_subviews(superview, objc::sel::get("setAutoresizesSubviews:"), 0)
        };
        set_frame_of(superview, rect(0.0, 0.0, 200.0, 200.0));
        assert_eq!(view_of(subview).frame, rect(0.0, 0.0, 100.0, 100.0));
    }

    static OVERRIDE_RAN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

    unsafe extern "C" fn overridden_resize(_receiver: Id, _sel: Sel, _old: NSSize) {
        OVERRIDE_RAN.store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// The subview is *sent* `-resizeWithOldSuperviewSize:`, so a program that
    /// overrides it lays itself out and the mask is not consulted behind its
    /// back. AppKit's contract, and the reason a view can size itself against
    /// something the mask cannot express.
    #[test]
    fn a_subclasss_own_resize_method_is_what_runs() {
        crate::bootstrap();
        let class = corefoundation::base::register(
            c"R3TestResizeView",
            objc::class::lookup(c"NSView"),
            INSTANCE_SIZE,
            false,
        );
        methods!(class, {
            "resizeWithOldSuperviewSize:" => overridden_resize, c"v32@0:8{CGSize=dd}16";
        });
        let superview = view(rect(0.0, 0.0, 100.0, 100.0));
        let subview = objc::object::create_instance(class, 0);
        // SAFETY: `-initWithFrame:` is `id (id, SEL, NSRect)`.
        unsafe {
            init_with_frame(
                subview,
                objc::sel::get("initWithFrame:"),
                rect(0.0, 0.0, 100.0, 100.0),
            )
        };
        // SAFETY: `-addSubview:` is `void (id, SEL, id)`.
        unsafe { add_subview(superview, objc::sel::get("addSubview:"), subview) };
        view_of(subview).autoresizing_mask =
            autoresizing::WIDTH_SIZABLE | autoresizing::HEIGHT_SIZABLE;

        set_frame_of(superview, rect(0.0, 0.0, 200.0, 200.0));
        assert!(OVERRIDE_RAN.load(std::sync::atomic::Ordering::Relaxed));
        // The override did not call through, so nothing moved the frame.
        assert_eq!(view_of(subview).frame, rect(0.0, 0.0, 100.0, 100.0));
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
        assert_eq!(
            unsafe { superview(child, objc::sel::get("superview")) },
            parent
        );
        // SAFETY: `-subviews` is `id (id, SEL)`.
        let list = unsafe { subviews(parent, objc::sel::get("subviews")) };
        // SAFETY: an array this module made.
        let array =
            unsafe { corefoundation::collections::array(list as *const c_void) }.expect("an array");
        assert_eq!(array.values.len(), 1);

        // SAFETY: `-removeFromSuperview` is `void (id, SEL)`.
        unsafe { remove_from_superview(child, objc::sel::get("removeFromSuperview")) };
        // SAFETY: as above.
        let array =
            unsafe { corefoundation::collections::array(list as *const c_void) }.expect("an array");
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
        let window =
            objc::object::create_instance(objc::class::lookup(c"NSWindow").expect("NSWindow"), 0);
        // SAFETY: both are objects this runtime allocated.
        unsafe { set_window(parent, window) };
        // SAFETY: as above.
        assert_eq!(unsafe { window_of(child) }, window);
    }
}
