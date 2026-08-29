//! `NSWindow`, over an SDL window.

use std::ffi::c_void;

use foundation::geometry::{CGFloat, NSRect, NSSize};
use objc::object::Id;
use objc::sel::Sel;

use crate::define::methods;

#[repr(C)]
pub struct Window {
    isa: *const c_void,
    /// The SDL window, or null until `-initWithContentRect:` has run.
    sdl: sdl3::Window,
    content_view: Id,
    delegate: Id,
    background: Id,
    /// `NSWindowStyleMask`, kept because `-styleMask` is asked and because the
    /// resizable bit is the one SDL needs told separately.
    style_mask: usize,
    level: isize,
    accepts_mouse_moved: usize,
    shown: usize,
}

pub const INSTANCE_SIZE: usize = size_of::<Window>();

/// `NSWindowStyleMask`.
mod style {
    pub const TITLED: usize = 1;
    pub const RESIZABLE: usize = 1 << 3;
}

fn window_of<'a>(receiver: Id) -> &'a mut Window {
    // SAFETY: a window of this crate's, which is what every method here is sent
    // to.
    unsafe { &mut *(receiver as *mut Window) }
}

/// Every live window, so an SDL event's window id can find the object it belongs
/// to.
///
/// SDL identifies a window by a small integer in every event; AppKit identifies
/// one by the object. The table is the join, and it is here rather than in the
/// event module because this is what creates and destroys the entries.
/// Both halves are addresses rather than pointers, because the table is shared
/// between threads and neither is dereferenced while it is in it.
static WINDOWS: std::sync::Mutex<Vec<(usize, usize)>> = std::sync::Mutex::new(Vec::new());

/// The `NSWindow` for an SDL window, or nil.
pub fn window_for(sdl: sdl3::Window) -> Id {
    WINDOWS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .find(|(w, _)| *w == sdl as usize)
        .map_or(std::ptr::null_mut(), |(_, object)| *object as Id)
}

/// Every window this crate has made, most recent last.
pub fn all() -> Vec<Id> {
    WINDOWS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .map(|(_, object)| *object as Id)
        .collect()
}

/// The SDL window behind an `NSWindow`, or null.
///
/// # Safety
///
/// `object` must be null or an object this runtime allocated.
pub unsafe fn sdl_window(object: Id) -> sdl3::Window {
    if object.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: the caller's obligation.
    let is_window = unsafe { objc::object::class_of(object) }.is_some_and(|c| {
        objc::class::lookup(c"NSWindow").is_some_and(|k| c.chain().any(|x| std::ptr::eq(x, k)))
    });
    match is_window {
        true => window_of(object).sdl,
        false => std::ptr::null_mut(),
    }
}

/// The window an SDL window id names, for the event pump.
pub fn delegate_of(object: Id) -> Id {
    match object.is_null() {
        true => std::ptr::null_mut(),
        false => window_of(object).delegate,
    }
}

unsafe extern "C" fn init_with_content_rect(
    receiver: Id,
    _sel: Sel,
    rect: NSRect,
    style_mask: usize,
    _backing: usize,
    _defer: u8,
) -> Id {
    let window = window_of(receiver);
    window.style_mask = style_mask;

    let mut flags = sdl3::video::flags::OPENGL
        | sdl3::video::flags::HIDDEN
        | sdl3::video::flags::HIGH_PIXEL_DENSITY;
    if style_mask & style::RESIZABLE != 0 {
        flags |= sdl3::video::flags::RESIZABLE;
    }
    if style_mask & style::TITLED == 0 {
        flags |= sdl3::video::flags::BORDERLESS;
    }
    let (x, y, width, height) = crate::geometry::rect_to_sdl(rect);
    let Some(sdl) = sdl3::video::create_window(c"", width.max(1), height.max(1), flags) else {
        // A window this runtime cannot make is not something to continue past:
        // every later method would be sent to a window with no SDL window
        // behind it, and each would answer plausibly and do nothing.
        objc::fail!(
            "-[NSWindow initWithContentRect:...]: SDL could not create a window: {}",
            sdl3::error()
        );
    };
    sdl3::video::set_window_position(sdl, x, y);
    window.sdl = sdl;
    WINDOWS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push((sdl as usize, receiver as usize));
    receiver
}

/// The window's frame, asked of SDL rather than remembered.
///
/// A user can drag or resize a window, and a remembered rectangle would then be
/// where the program last put it rather than where it is.
unsafe extern "C" fn frame(receiver: Id, _sel: Sel) -> NSRect {
    let sdl = window_of(receiver).sdl;
    if sdl.is_null() {
        return NSRect::default();
    }
    let (x, y) = sdl3::video::window_position(sdl).unwrap_or((0, 0));
    let (w, h) = sdl3::video::window_size(sdl).unwrap_or((0, 0));
    crate::geometry::rect_from_sdl(x, y, w, h)
}

/// `-contentRectForFrameRect:` and its inverse are the identity here.
///
/// SDL's window size is already the content size - the title bar is outside it -
/// so the two rectangles AppKit distinguishes are the same one, and converting
/// between them would subtract a title bar height this runtime does not know.
unsafe extern "C" fn identity_rect(_receiver: Id, _sel: Sel, rect: NSRect) -> NSRect {
    rect
}

unsafe extern "C" fn set_frame(receiver: Id, _sel: Sel, rect: NSRect) {
    let sdl = window_of(receiver).sdl;
    if sdl.is_null() {
        return;
    }
    let (x, y, width, height) = crate::geometry::rect_to_sdl(rect);
    sdl3::video::set_window_size(sdl, width.max(1), height.max(1));
    sdl3::video::set_window_position(sdl, x, y);
}

unsafe extern "C" fn set_frame_display(receiver: Id, sel: Sel, rect: NSRect, _display: u8) {
    // SAFETY: `-setFrame:` is `void (id, SEL, NSRect)`.
    unsafe { set_frame(receiver, sel, rect) };
}

unsafe extern "C" fn set_frame_origin(receiver: Id, _sel: Sel, origin: foundation::geometry::NSPoint) {
    let sdl = window_of(receiver).sdl;
    if sdl.is_null() {
        return;
    }
    let (w, h) = sdl3::video::window_size(sdl).unwrap_or((0, 0));
    let rect = NSRect {
        origin,
        size: NSSize {
            width: w as f64,
            height: h as f64,
        },
    };
    let (x, y, _, _) = crate::geometry::rect_to_sdl(rect);
    sdl3::video::set_window_position(sdl, x, y);
}

unsafe extern "C" fn set_content_size(receiver: Id, _sel: Sel, size: NSSize) {
    let sdl = window_of(receiver).sdl;
    if !sdl.is_null() {
        sdl3::video::set_window_size(sdl, (size.width as i32).max(1), (size.height as i32).max(1));
    }
}

unsafe extern "C" fn content_view(receiver: Id, _sel: Sel) -> Id {
    window_of(receiver).content_view
}

unsafe extern "C" fn set_content_view(receiver: Id, _sel: Sel, view: Id) {
    let window = window_of(receiver);
    objc::arc::release(window.content_view);
    window.content_view = objc::arc::retain(view);
    // SAFETY: `view` is null or an object this runtime allocated.
    unsafe { crate::view::set_window(view, receiver) };
}

unsafe extern "C" fn delegate(receiver: Id, _sel: Sel) -> Id {
    window_of(receiver).delegate
}

/// As `-[NSApplication setDelegate:]`, this does not retain: Apple's rule, and
/// the reason a window delegate outliving its window is the program's business.
unsafe extern "C" fn set_delegate(receiver: Id, _sel: Sel, value: Id) {
    window_of(receiver).delegate = value;
}

unsafe extern "C" fn set_title(receiver: Id, _sel: Sel, title: Id) {
    let sdl = window_of(receiver).sdl;
    if sdl.is_null() || title.is_null() {
        return;
    }
    // SAFETY: an `NSString`.
    let bytes = unsafe { foundation::string::bytes(title) };
    let Ok(text) = std::ffi::CString::new(bytes) else {
        return;
    };
    sdl3::video::set_window_title(sdl, &text);
}

unsafe extern "C" fn title(_receiver: Id, _sel: Sel) -> Id {
    // SDL has no way to read a title back, so this answers with what a window
    // with none has rather than with a title it might not have.
    foundation::string::from_str("")
}

unsafe extern "C" fn make_key_and_order_front(receiver: Id, _sel: Sel, _sender: Id) {
    let sdl = window_of(receiver).sdl;
    if sdl.is_null() {
        return;
    }
    sdl3::video::show_window(sdl);
    sdl3::video::raise_window(sdl);
    window_of(receiver).shown = 1;
    KEY.store(receiver as usize, std::sync::atomic::Ordering::Release);
}

unsafe extern "C" fn order_out(receiver: Id, _sel: Sel, _sender: Id) {
    let sdl = window_of(receiver).sdl;
    if !sdl.is_null() {
        sdl3::video::hide_window(sdl);
    }
    window_of(receiver).shown = 0;
    let _ = KEY.compare_exchange(
        receiver as usize,
        0,
        std::sync::atomic::Ordering::AcqRel,
        std::sync::atomic::Ordering::Acquire,
    );
}

unsafe extern "C" fn close(receiver: Id, sel: Sel) {
    // SAFETY: `-orderOut:` is `void (id, SEL, id)`.
    unsafe { order_out(receiver, sel, std::ptr::null_mut()) };
}

/// The window a program's keystrokes go to, which is the last one ordered front.
static KEY: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

pub fn key_window() -> Id {
    KEY.load(std::sync::atomic::Ordering::Acquire) as Id
}

/// Say which window is key, from the event pump when focus moves.
pub fn set_key_window(window: Id) {
    KEY.store(window as usize, std::sync::atomic::Ordering::Release);
}

unsafe extern "C" fn is_key_window(receiver: Id, _sel: Sel) -> u8 {
    (key_window() == receiver) as u8
}

unsafe extern "C" fn is_visible(receiver: Id, _sel: Sel) -> u8 {
    (window_of(receiver).shown != 0) as u8
}

unsafe extern "C" fn screen(receiver: Id, _sel: Sel) -> Id {
    let class = objc::class::ptr(objc::class::lookup(c"NSScreen").expect("NSScreen")) as Id;
    // SAFETY: `+mainScreen` is `id (Class, SEL)`. The display a window is
    // actually on is a question SDL can answer and this does not ask, because
    // nothing in this tree yet distinguishes them.
    let _ = receiver;
    unsafe {
        let f: unsafe extern "C" fn(Id, Sel) -> Id = std::mem::transmute(objc::msg::entry());
        f(class, objc::sel::get("mainScreen"))
    }
}

unsafe extern "C" fn backing_scale_factor(receiver: Id, _sel: Sel) -> CGFloat {
    let sdl = window_of(receiver).sdl;
    match sdl.is_null() {
        true => 1.0,
        false => sdl3::video::window_pixel_density(sdl) as f64,
    }
}

unsafe extern "C" fn set_background_color(receiver: Id, _sel: Sel, color: Id) {
    let window = window_of(receiver);
    objc::arc::release(window.background);
    window.background = objc::arc::retain(color);
}

unsafe extern "C" fn background_color(receiver: Id, _sel: Sel) -> Id {
    window_of(receiver).background
}

unsafe extern "C" fn set_accepts_mouse_moved(receiver: Id, _sel: Sel, accepts: u8) {
    window_of(receiver).accepts_mouse_moved = accepts as usize;
}

unsafe extern "C" fn accepts_mouse_moved(receiver: Id, _sel: Sel) -> u8 {
    (window_of(receiver).accepts_mouse_moved != 0) as u8
}

unsafe extern "C" fn set_level(receiver: Id, _sel: Sel, level: isize) {
    window_of(receiver).level = level;
}

unsafe extern "C" fn level(receiver: Id, _sel: Sel) -> isize {
    window_of(receiver).level
}

unsafe extern "C" fn style_mask(receiver: Id, _sel: Sel) -> usize {
    window_of(receiver).style_mask
}

unsafe extern "C" fn set_style_mask(receiver: Id, _sel: Sel, mask: usize) {
    let window = window_of(receiver);
    window.style_mask = mask;
    if !window.sdl.is_null() {
        sdl3::video::set_window_resizable(window.sdl, mask & style::RESIZABLE != 0);
    }
}

/// Accepted and ignored, one line each.
///
/// Every one is a request about how a window *looks* that SDL either does by
/// default or cannot be told. Present rather than absent because a program calls
/// them in a row while setting a window up, and a refusal on any would stop it
/// on a line that changes nothing about whether it can draw.
unsafe extern "C" fn ignored_flag(_receiver: Id, _sel: Sel, _flag: u8) {}

unsafe extern "C" fn ignored(_receiver: Id, _sel: Sel) {}

unsafe extern "C" fn ignored_object(_receiver: Id, _sel: Sel, _object: Id) {}

pub(crate) fn install() {
    let class = objc::class::lookup(c"NSWindow").expect("declared before its methods");
    methods!(class, {
        "initWithContentRect:styleMask:backing:defer:" => init_with_content_rect,
            c"@56@0:8{CGRect={CGPoint=dd}{CGSize=dd}}16Q48Q56c64";
        "frame" => frame, c"{CGRect={CGPoint=dd}{CGSize=dd}}16@0:8";
        "contentRectForFrameRect:" => identity_rect,
            c"{CGRect={CGPoint=dd}{CGSize=dd}}48@0:8{CGRect={CGPoint=dd}{CGSize=dd}}16";
        "frameRectForContentRect:" => identity_rect,
            c"{CGRect={CGPoint=dd}{CGSize=dd}}48@0:8{CGRect={CGPoint=dd}{CGSize=dd}}16";
        "setFrame:" => set_frame, c"v48@0:8{CGRect={CGPoint=dd}{CGSize=dd}}16";
        "setFrame:display:" => set_frame_display, c"v52@0:8{CGRect={CGPoint=dd}{CGSize=dd}}16c48";
        "setFrameOrigin:" => set_frame_origin, c"v32@0:8{CGPoint=dd}16";
        "setContentSize:" => set_content_size, c"v32@0:8{CGSize=dd}16";
        "contentView" => content_view, c"@16@0:8";
        "setContentView:" => set_content_view, c"v24@0:8@16";
        "delegate" => delegate, c"@16@0:8";
        "setDelegate:" => set_delegate, c"v24@0:8@16";
        "title" => title, c"@16@0:8";
        "setTitle:" => set_title, c"v24@0:8@16";
        "makeKeyAndOrderFront:" => make_key_and_order_front, c"v24@0:8@16";
        "makeKeyWindow" => ignored, c"v16@0:8";
        "makeMainWindow" => ignored, c"v16@0:8";
        "orderFront:" => make_key_and_order_front, c"v24@0:8@16";
        "orderOut:" => order_out, c"v24@0:8@16";
        "close" => close, c"v16@0:8";
        "isKeyWindow" => is_key_window, c"c16@0:8";
        "isMainWindow" => is_key_window, c"c16@0:8";
        "isVisible" => is_visible, c"c16@0:8";
        "screen" => screen, c"@16@0:8";
        "backingScaleFactor" => backing_scale_factor, c"d16@0:8";
        "backgroundColor" => background_color, c"@16@0:8";
        "setBackgroundColor:" => set_background_color, c"v24@0:8@16";
        "acceptsMouseMovedEvents" => accepts_mouse_moved, c"c16@0:8";
        "setAcceptsMouseMovedEvents:" => set_accepts_mouse_moved, c"v20@0:8c16";
        "level" => level, c"q16@0:8";
        "setLevel:" => set_level, c"v24@0:8q16";
        "styleMask" => style_mask, c"Q16@0:8";
        "setStyleMask:" => set_style_mask, c"v24@0:8Q16";
        "setReleasedWhenClosed:" => ignored_flag, c"v20@0:8c16";
        "setOpaque:" => ignored_flag, c"v20@0:8c16";
        "setHidesOnDeactivate:" => ignored_flag, c"v20@0:8c16";
        "setRestorable:" => ignored_flag, c"v20@0:8c16";
        "setShowsResizeIndicator:" => ignored_flag, c"v20@0:8c16";
        "setCollectionBehavior:" => ignored_flag, c"v24@0:8Q16";
        "center" => ignored, c"v16@0:8";
        "display" => ignored, c"v16@0:8";
        "update" => ignored, c"v16@0:8";
        "invalidateCursorRectsForView:" => ignored_object, c"v24@0:8@16";
    });
}
