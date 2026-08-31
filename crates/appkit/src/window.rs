//! `NSWindow`, over an SDL window.

use std::ffi::c_void;

use foundation::geometry::{CGFloat, NSPoint, NSRect, NSSize};
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
    /// `NSWindowCollectionBehavior`, which a program reads back to decide
    /// whether it may go full screen.
    collection_behavior: usize,
    first_responder: Id,
    minimum: NSSize,
    maximum: NSSize,
    fullscreen: usize,
    miniaturized: usize,
}

pub const INSTANCE_SIZE: usize = size_of::<Window>();

/// The notification names a window posts.
///
/// Named here rather than where they are posted, because publishing the symbol
/// and posting the notification are two different jobs and the first has to
/// happen whether or not the second does yet.
pub const DID_RESIZE: &str = "NSWindowDidResizeNotification";
pub const DID_MOVE: &str = "NSWindowDidMoveNotification";
pub const WILL_CLOSE: &str = "NSWindowWillCloseNotification";
pub const DID_BECOME_KEY: &str = "NSWindowDidBecomeKeyNotification";
pub const DID_RESIGN_KEY: &str = "NSWindowDidResignKeyNotification";
pub const DID_MINIATURIZE: &str = "NSWindowDidMiniaturizeNotification";
pub const DID_DEMINIATURIZE: &str = "NSWindowDidDeminiaturizeNotification";
pub const WILL_ENTER_FULL_SCREEN: &str = "NSWindowWillEnterFullScreenNotification";
pub const WILL_EXIT_FULL_SCREEN: &str = "NSWindowWillExitFullScreenNotification";
pub const DID_ENTER_FULL_SCREEN: &str = "NSWindowDidEnterFullScreenNotification";
pub const DID_EXIT_FULL_SCREEN: &str = "NSWindowDidExitFullScreenNotification";
/// Posted when the window moves to a screen of a different backing scale, which
/// is what a guest recomputes its drawable size from.
pub const DID_CHANGE_BACKING_PROPERTIES: &str = "NSWindowDidChangeBackingPropertiesNotification";

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

/// Every live SDL window, for a change that applies to all of them.
pub fn sdl_windows() -> Vec<sdl3::Window> {
    WINDOWS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .map(|(sdl, _)| *sdl as sdl3::Window)
        .collect()
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

/// A window's content view, for the event pump.
pub fn content_view_of(object: Id) -> Id {
    match object.is_null() {
        true => std::ptr::null_mut(),
        false => window_of(object).content_view,
    }
}

/// Whether `-mouseMoved:` should be delivered to this window, for the event
/// pump: AppKit drops those unless a window asked for them, and a game that
/// never asked would otherwise get a stream it does not expect.
pub fn accepts_mouse_moved_events(object: Id) -> bool {
    !object.is_null() && window_of(object).accepts_mouse_moved != 0
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
    crate::icon::apply(sdl);
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

/// `-convertBaseToScreen:`, and its inverse below.
///
/// A window's base coordinates have their origin at its own bottom-left, so the
/// conversion is the frame origin - which is already in screen coordinates,
/// flipped the way AppKit wants by `geometry::rect_from_sdl`. The two are the
/// deprecated spellings of `-convertRectToScreen:`; a program that positions a
/// popover or reads the mouse in screen space still uses them.
unsafe extern "C" fn convert_base_to_screen(receiver: Id, sel: Sel, point: NSPoint) -> NSPoint {
    // SAFETY: the receiver is a window of this crate's.
    let origin = unsafe { frame(receiver, sel) }.origin;
    NSPoint {
        x: point.x + origin.x,
        y: point.y + origin.y,
    }
}

unsafe extern "C" fn convert_screen_to_base(receiver: Id, sel: Sel, point: NSPoint) -> NSPoint {
    // SAFETY: the receiver is a window of this crate's.
    let origin = unsafe { frame(receiver, sel) }.origin;
    NSPoint {
        x: point.x - origin.x,
        y: point.y - origin.y,
    }
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

unsafe extern "C" fn set_frame_origin(
    receiver: Id,
    _sel: Sel,
    origin: foundation::geometry::NSPoint,
) {
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

/// `-setContentView:`, which **resizes the view to the window**.
///
/// AppKit does that, and it is not a detail: a nib's view carries the size it
/// had in Interface Builder, and a window carries the size the document asked
/// for. Deltarune's are 640x480 and 966x620, so a view left at its own size
/// draws into a fraction of the window and reports a backing store that does not
/// match it.
unsafe extern "C" fn set_content_view(receiver: Id, _sel: Sel, view: Id) {
    let window = window_of(receiver);
    objc::arc::release(window.content_view);
    window.content_view = objc::arc::retain(view);
    // SAFETY: `view` is null or an object this runtime allocated.
    unsafe { crate::view::set_window(view, receiver) };
    if view.is_null() || window.sdl.is_null() {
        return;
    }
    let (width, height) = sdl3::video::window_size(window.sdl).unwrap_or((0, 0));
    let frame = NSRect {
        origin: foundation::geometry::NSPoint::default(),
        size: NSSize {
            width: width as f64,
            height: height as f64,
        },
    };
    // Sent rather than written, so a view that tracks its own size - which a
    // guest's `-setFrame:` override is for - hears about it.
    // SAFETY: `-setFrame:` is `void (id, SEL, NSRect)`.
    unsafe {
        let f: unsafe extern "C" fn(Id, Sel, NSRect) = std::mem::transmute(objc::msg::entry());
        f(view, objc::sel::get("setFrame:"), frame);
    }
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

unsafe extern "C" fn collection_behavior(receiver: Id, _sel: Sel) -> usize {
    window_of(receiver).collection_behavior
}

unsafe extern "C" fn set_collection_behavior(receiver: Id, _sel: Sel, behavior: usize) {
    window_of(receiver).collection_behavior = behavior;
}

unsafe extern "C" fn first_responder(receiver: Id, _sel: Sel) -> Id {
    let window = window_of(receiver);
    match window.first_responder.is_null() {
        // A window with no first responder of its own answers with itself, as
        // AppKit's does - it is the end of its own responder chain.
        true => receiver,
        false => window.first_responder,
    }
}

unsafe extern "C" fn make_first_responder(receiver: Id, _sel: Sel, responder: Id) -> u8 {
    window_of(receiver).first_responder = responder;
    1
}

/// `-toggleFullScreen:`, which SDL does have.
///
/// The one window operation in this file that is not bookkeeping: a game asks
/// for it from a menu item or a keystroke, and a window that stayed the same
/// size would look like the request was lost.
unsafe extern "C" fn toggle_full_screen(receiver: Id, _sel: Sel, _sender: Id) {
    let window = window_of(receiver);
    if window.sdl.is_null() {
        return;
    }
    let wanted = window.fullscreen == 0;
    if sdl3::video::set_window_fullscreen(window.sdl, wanted) {
        window.fullscreen = wanted as usize;
    }
}

unsafe extern "C" fn is_full_screen(receiver: Id, _sel: Sel) -> u8 {
    (window_of(receiver).fullscreen != 0) as u8
}

unsafe extern "C" fn is_miniaturized(receiver: Id, _sel: Sel) -> u8 {
    (window_of(receiver).miniaturized != 0) as u8
}

unsafe extern "C" fn miniaturize(receiver: Id, sel: Sel, sender: Id) {
    window_of(receiver).miniaturized = 1;
    // SAFETY: `-orderOut:` is `void (id, SEL, id)`.
    unsafe { order_out(receiver, sel, sender) };
}

unsafe extern "C" fn deminiaturize(receiver: Id, sel: Sel, sender: Id) {
    window_of(receiver).miniaturized = 0;
    // SAFETY: `-makeKeyAndOrderFront:` is `void (id, SEL, id)`.
    unsafe { make_key_and_order_front(receiver, sel, sender) };
}

/// `-isZoomed`: whether the window fills its screen.
///
/// Asked of the window's actual size rather than remembered, because a user can
/// zoom one by dragging it and a remembered flag would then be a claim about
/// what the program last did.
unsafe extern "C" fn is_zoomed(receiver: Id, sel: Sel) -> u8 {
    // SAFETY: `-frame` is `NSRect (id, SEL)`.
    let frame = unsafe { frame(receiver, sel) };
    let screen = crate::geometry::primary_height();
    (frame.size.height >= screen) as u8
}

/// The size limits, kept and reported.
///
/// SDL can be told a minimum and a maximum, and is not: this runtime does not
/// resize the window itself, so the limits only matter when the *user* drags an
/// edge - and a limit enforced here but not there would let the drag through and
/// then report a size the window does not have.
unsafe extern "C" fn min_size(receiver: Id, _sel: Sel) -> NSSize {
    window_of(receiver).minimum
}

unsafe extern "C" fn set_min_size(receiver: Id, _sel: Sel, size: NSSize) {
    window_of(receiver).minimum = size;
}

unsafe extern "C" fn max_size(receiver: Id, _sel: Sel) -> NSSize {
    window_of(receiver).maximum
}

unsafe extern "C" fn set_max_size(receiver: Id, _sel: Sel, size: NSSize) {
    window_of(receiver).maximum = size;
}

/// `-standardWindowButton:`: the close, minimise and zoom buttons.
///
/// Nil, which is what AppKit answers for a button the window's style mask does
/// not include - and an SDL window's decorations are the platform's, not
/// objects this runtime has. A program asking for one is usually about to hide
/// it, which is already the case.
unsafe extern "C" fn standard_window_button(_receiver: Id, _sel: Sel, _which: usize) -> Id {
    std::ptr::null_mut()
}

/// `-windowNumber`, which is SDL's own window id.
///
/// A number a program compares and passes around rather than one it interprets,
/// so any stable per-window integer will do - and SDL's is the one this runtime
/// already uses to route events.
unsafe extern "C" fn window_number(receiver: Id, _sel: Sel) -> isize {
    let sdl = window_of(receiver).sdl;
    match sdl.is_null() {
        true => 0,
        false => sdl3::video::window_id(sdl) as isize,
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

unsafe extern "C" fn ignored_size(_receiver: Id, _sel: Sel, _size: NSSize) {}

unsafe extern "C" fn ignored_integer(_receiver: Id, _sel: Sel, _value: isize) {}

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
        "convertBaseToScreen:" => convert_base_to_screen, c"{CGPoint=dd}32@0:8{CGPoint=dd}16";
        "convertScreenToBase:" => convert_screen_to_base, c"{CGPoint=dd}32@0:8{CGPoint=dd}16";
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
        "standardWindowButton:" => standard_window_button, c"@24@0:8Q16";
        "windowNumber" => window_number, c"q16@0:8";
        "setTitlebarAppearsTransparent:" => ignored_flag, c"v20@0:8c16";
        "setTitleVisibility:" => ignored_integer, c"v24@0:8q16";
        "setMovableByWindowBackground:" => ignored_flag, c"v20@0:8c16";
        "setIgnoresMouseEvents:" => ignored_flag, c"v20@0:8c16";
        "collectionBehavior" => collection_behavior, c"Q16@0:8";
        "setCollectionBehavior:" => set_collection_behavior, c"v24@0:8Q16";
        "firstResponder" => first_responder, c"@16@0:8";
        "makeFirstResponder:" => make_first_responder, c"c24@0:8@16";
        "toggleFullScreen:" => toggle_full_screen, c"v24@0:8@16";
        "isInFullScreenMode" => is_full_screen, c"c16@0:8";
        "minSize" => min_size, c"{CGSize=dd}16@0:8";
        "setMinSize:" => set_min_size, c"v32@0:8{CGSize=dd}16";
        "maxSize" => max_size, c"{CGSize=dd}16@0:8";
        "setMaxSize:" => set_max_size, c"v32@0:8{CGSize=dd}16";
        "contentMinSize" => min_size, c"{CGSize=dd}16@0:8";
        "setContentMinSize:" => set_min_size, c"v32@0:8{CGSize=dd}16";
        "contentMaxSize" => max_size, c"{CGSize=dd}16@0:8";
        "setContentMaxSize:" => set_max_size, c"v32@0:8{CGSize=dd}16";
        "setContentAspectRatio:" => ignored_size, c"v32@0:8{CGSize=dd}16";
        "setAspectRatio:" => ignored_size, c"v32@0:8{CGSize=dd}16";
        "setContentResizeIncrements:" => ignored_size, c"v32@0:8{CGSize=dd}16";
        "zoom:" => ignored_object, c"v24@0:8@16";
        "performZoom:" => ignored_object, c"v24@0:8@16";
        "miniaturize:" => miniaturize, c"v24@0:8@16";
        "performMiniaturize:" => miniaturize, c"v24@0:8@16";
        "deminiaturize:" => deminiaturize, c"v24@0:8@16";
        "isMiniaturized" => is_miniaturized, c"c16@0:8";
        "isZoomed" => is_zoomed, c"c16@0:8";
        "center" => ignored, c"v16@0:8";
        "display" => ignored, c"v16@0:8";
        "update" => ignored, c"v16@0:8";
        "invalidateCursorRectsForView:" => ignored_object, c"v24@0:8@16";
    });
}
