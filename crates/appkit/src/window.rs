//! `NSWindow`, over an SDL window.

use std::ffi::c_void;

use foundation::geometry::{CGFloat, NSPoint, NSRect, NSSize};
use objc::object::Id;
use objc::sel::Sel;

use crate::define::{class_methods, methods};

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
pub const DID_BECOME_MAIN: &str = "NSWindowDidBecomeMainNotification";
pub const DID_RESIGN_MAIN: &str = "NSWindowDidResignMainNotification";
pub const DID_MINIATURIZE: &str = "NSWindowDidMiniaturizeNotification";
pub const DID_DEMINIATURIZE: &str = "NSWindowDidDeminiaturizeNotification";
pub const WILL_ENTER_FULL_SCREEN: &str = "NSWindowWillEnterFullScreenNotification";
pub const WILL_EXIT_FULL_SCREEN: &str = "NSWindowWillExitFullScreenNotification";
pub const DID_ENTER_FULL_SCREEN: &str = "NSWindowDidEnterFullScreenNotification";
pub const DID_EXIT_FULL_SCREEN: &str = "NSWindowDidExitFullScreenNotification";
/// Posted when the window moves to a screen of a different backing scale, which
/// is what a guest recomputes its drawable size from.
pub const DID_CHANGE_BACKING_PROPERTIES: &str = "NSWindowDidChangeBackingPropertiesNotification";
/// The pair that brackets a user-driven resize. Published and not yet posted:
/// nothing here distinguishes a drag of the window's edge from a programmatic
/// resize, so a toolkit that batches work between the two does it for every resize
/// instead - which is the slower path and not the wrong one.
pub const WILL_START_LIVE_RESIZE: &str = "NSWindowWillStartLiveResizeNotification";
pub const DID_END_LIVE_RESIZE: &str = "NSWindowDidEndLiveResizeNotification";

/// `NSWindowStyleMask`.
mod style {
    pub const TITLED: usize = 1;
    pub const CLOSABLE: usize = 1 << 1;
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

/// `ROSETTA3_WINDOW_SIZE=WIDTHxHEIGHT`, the size every window is created at.
fn forced_size() -> Option<(i32, i32)> {
    static SIZE: std::sync::OnceLock<Option<(i32, i32)>> = std::sync::OnceLock::new();
    *SIZE.get_or_init(|| {
        let value = std::env::var("ROSETTA3_WINDOW_SIZE").ok()?;
        let (width, height) = value.split_once(['x', 'X'])?;
        Some((width.trim().parse().ok()?, height.trim().parse().ok()?))
    })
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

    let flags = creation_flags(style_mask, Api::Undecided);
    let (x, y, width, height) = crate::geometry::rect_to_sdl(rect);
    let (width, height) = match forced_size() {
        // A guest which sizes its first window to a fraction of the screen is
        // unusable in a headless or scripted session, where nothing can drag
        // it larger. This says how large every window it makes must be.
        Some((forced_width, forced_height)) => (forced_width, forced_height),
        None => (width, height),
    };
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

/// Run `-initWithContentRect:...` for a test, answering whether SDL could
/// make the window at all - a host with no display cannot, and a test which
/// needs one skips rather than fails.
///
/// # Safety
///
/// `window` is a fresh `NSWindow` this runtime allocated.
#[cfg(test)]
pub unsafe fn init_with_content_rect_for_tests(window: Id, rect: NSRect) -> bool {
    if sdl3::video::create_window(c"", 1, 1, sdl3::video::flags::HIDDEN).is_none() {
        return false;
    }
    // SAFETY: the caller's obligation; the selector's signature is
    // `id (id, SEL, NSRect, NSUInteger, NSUInteger, BOOL)`.
    unsafe {
        init_with_content_rect(
            window,
            objc::sel::get("initWithContentRect:styleMask:backing:defer:"),
            rect,
            style::TITLED,
            0,
            0,
        )
    };
    // SAFETY: a window this call just initialised.
    !unsafe { sdl_window(window) }.is_null()
}

/// The SDL flags for a window of `style_mask`, drawn with `api`.
///
/// A window is created with **no** graphics API by default, which is the
/// whole of how this runtime avoids having to guess which one a guest wants.
/// SDL commits a window to an API at creation and mostly cannot change its
/// mind afterwards: it will convert an OpenGL window into a Vulkan one in
/// place, but it converts the flags and the loaded libraries and not the
/// backend's own state, so on Wayland the `EGLSurface`
/// `Wayland_CreateWindow` made for an `SDL_WINDOW_OPENGL` window stays alive
/// on the `wl_surface`, both drivers claim it, and the compositor answers
/// `wp_linux_drm_syncobj_manager_v1: surface already exists` and closes the
/// connection.
///
/// A window with neither flag has no `EGLSurface`, so nothing has claimed its
/// surface: `SDL_Vulkan_CreateSurface` converts it safely and Metal
/// presentation needs nothing decided in advance. OpenGL is the direction SDL
/// will not convert - `SDL_GL_CreateContext` refuses a non-OpenGL window
/// outright - so a guest which asks for an `NSOpenGLContext` has its window
/// rebuilt as an OpenGL one; see [`rebuild_for_opengl`].
fn creation_flags(style_mask: usize, api: Api) -> u64 {
    let mut flags = sdl3::video::flags::HIDDEN | sdl3::video::flags::HIGH_PIXEL_DENSITY;
    if matches!(api, Api::OpenGl) {
        flags |= sdl3::video::flags::OPENGL;
    }
    if style_mask & style::RESIZABLE != 0 {
        flags |= sdl3::video::flags::RESIZABLE;
    }
    if style_mask & style::TITLED == 0 {
        flags |= sdl3::video::flags::BORDERLESS;
    }
    flags
}

/// The graphics API a window is created for.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Api {
    /// Neither OpenGL nor Vulkan: the window a guest gets until it says which
    /// it draws with, and the one a Vulkan surface can be made for.
    Undecided,
    OpenGl,
}

/// Rebuild `receiver`'s SDL window as an OpenGL one, keeping the `NSWindow`.
///
/// `SDL_GL_CreateContext` refuses a window that was not created with
/// `SDL_WINDOW_OPENGL`, and SDL has no public way to convert one, so the
/// window has to be made again. The `NSWindow` is what the guest holds and it
/// does not change; only the SDL window behind it does, so everything keyed
/// on that - the event table, the object's own handle - is moved over.
///
/// A guest asks for a GL context before it draws anything, which is while the
/// window is usually still hidden, so this is nearly always invisible.
/// Answers whether the window is an OpenGL window afterwards.
pub fn rebuild_for_opengl(receiver: Id) -> bool {
    let window = window_of(receiver);
    let old = window.sdl;
    if old.is_null() {
        return false;
    }
    if sdl3::video::window_flags(old) & sdl3::video::flags::OPENGL != 0 {
        return true;
    }
    // What the replacement has to come back as. Asked of SDL rather than
    // remembered, for the reason `-frame` is: a user may have moved or
    // resized the window since it was made.
    let (x, y) = sdl3::video::window_position(old).unwrap_or((0, 0));
    let (width, height) = sdl3::video::window_size(old).unwrap_or((1, 1));
    let title = sdl3::video::window_title(old);
    let shown = window.shown != 0;

    let flags = creation_flags(window.style_mask, Api::OpenGl);
    let Some(new) = sdl3::video::create_window(&title, width.max(1), height.max(1), flags) else {
        return false;
    };
    sdl3::video::set_window_position(new, x, y);
    crate::icon::apply(new);
    window.sdl = new;
    {
        let mut windows = WINDOWS.lock().unwrap_or_else(|e| e.into_inner());
        for entry in windows.iter_mut() {
            if entry.0 == old as usize {
                entry.0 = new as usize;
            }
        }
    }
    // The old window goes only once nothing points at it any more: its SDL
    // window id appears in events already queued, and the table above is what
    // those are resolved through.
    sdl3::video::destroy_window(old);
    update_text_input(receiver);
    if shown {
        sdl3::video::show_window(new);
        sdl3::video::raise_window(new);
    }
    true
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

/// `+windowNumberAtPoint:belowWindowWithWindowNumber:`, over this runtime's own
/// windows.
///
/// Apple's answers about every window on the screen, this process's or not.
/// Nothing here can: the windows of other applications belong to the window
/// server and SDL does not report them. A program that asks whether the pointer
/// is over one of *its* windows - which is what this is for - gets the right
/// answer; one asking whether some other application is in front is told no.
unsafe extern "C" fn window_number_at_point(
    _class: Id,
    _sel: Sel,
    point: NSPoint,
    below: isize,
) -> isize {
    // Front to back, which is the order the answer is defined in and the
    // reverse of the order windows were made in.
    let windows: Vec<Id> = all().into_iter().rev().collect();
    let start = match below {
        0 => 0,
        // SAFETY: `-windowNumber` is `NSInteger (id, SEL)`.
        _ => match windows
            .iter()
            .position(|w| unsafe { window_number(*w, objc::sel::get("windowNumber")) == below })
        {
            Some(at) => at + 1,
            // A number naming no window of this process's orders nothing.
            None => return 0,
        },
    };
    for window in &windows[start.min(windows.len())..] {
        // SAFETY: `-isVisible` and `-frame` are this module's own, called with
        // the receivers they were installed for.
        unsafe {
            if is_visible(*window, objc::sel::get("isVisible")) == 0 {
                continue;
            }
            let rect = frame(*window, objc::sel::get("frame"));
            if crate::geometry::contains(rect, point) {
                return window_number(*window, objc::sel::get("windowNumber"));
            }
        }
    }
    0
}

/// `-sendEvent:`, the default routing.
unsafe extern "C" fn send_event(receiver: Id, _sel: Sel, event: Id) {
    crate::event::route(receiver, event);
}

/// `-mouseLocationOutsideOfEventStream`: where the pointer is now, in this
/// window's coordinates.
unsafe extern "C" fn mouse_location_outside_of_event_stream(receiver: Id, _sel: Sel) -> NSPoint {
    pointer_location(receiver)
}

/// Where the pointer is in a window's base coordinates, for the cursor
/// rectangles as well as for the method above: [`crate::cursor`] has to decide
/// which rectangle holds the pointer, and asking through `objc_msgSend` would
/// reach a program's own override of a method it never called.
pub fn pointer_location(window: Id) -> NSPoint {
    if window.is_null() {
        return NSPoint::default();
    }
    let point = match crate::script::pointer().or_else(crate::event::frozen_pointer) {
        Some((x, y)) => crate::geometry::point_from_sdl(x as f64, y as f64),
        None => match sdl3::video::global_mouse_state() {
            Some((x, y, _)) => crate::geometry::point_from_sdl(x as f64, y as f64),
            None => return NSPoint::default(),
        },
    };
    let sel = objc::sel::get("frame");
    // SAFETY: the receiver is a window of this crate's, and both are its own
    // methods called with the receiver they were installed for.
    unsafe { convert_screen_to_base(window, sel, point) }
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

/// The rectangle forms of the two above: only the origin moves, because the two
/// coordinate systems differ by a translation and nothing else.
unsafe extern "C" fn convert_rect_from_screen(receiver: Id, sel: Sel, rect: NSRect) -> NSRect {
    NSRect {
        // SAFETY: the receiver is a window of this crate's.
        origin: unsafe { convert_screen_to_base(receiver, sel, rect.origin) },
        size: rect.size,
    }
}

unsafe extern "C" fn convert_rect_to_screen(receiver: Id, sel: Sel, rect: NSRect) -> NSRect {
    NSRect {
        // SAFETY: as above.
        origin: unsafe { convert_base_to_screen(receiver, sel, rect.origin) },
        size: rect.size,
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
    // SAFETY: a view this crate made, now this window's.
    unsafe { crate::view::update_tracking_areas(view) };
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

/// Consult the delegate before closing, falling back to the window itself.
unsafe extern "C" fn perform_close(receiver: Id, _sel: Sel, _sender: Id) {
    if window_of(receiver).style_mask & style::CLOSABLE == 0 {
        return;
    }
    let should_close = objc::sel::get("windowShouldClose:");
    let target = [delegate_of(receiver), receiver]
        .into_iter()
        .find(|object| {
            // SAFETY: the window and its delegate are live runtime objects or nil.
            unsafe { objc::object::class_of(*object) }
                .is_some_and(|class| class.method(should_close).is_some())
        });
    if let Some(target) = target {
        // SAFETY: `-windowShouldClose:` is `BOOL (id, SEL, id)`.
        let agrees = unsafe {
            let send: unsafe extern "C" fn(Id, Sel, Id) -> u8 =
                std::mem::transmute(objc::msg::entry());
            send(target, should_close, receiver)
        };
        if agrees == 0 {
            return;
        }
    }
    // Send the message so a subclass's close override runs too.
    // SAFETY: `-close` is `void (id, SEL)`.
    unsafe {
        let send: unsafe extern "C" fn(Id, Sel) = std::mem::transmute(objc::msg::entry());
        send(receiver, objc::sel::get("close"));
    }
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
    if crate::event::tracing() {
        // SAFETY: null or a live object this runtime allocated.
        let class = unsafe { objc::object::class_of(responder) };
        eprintln!(
            "rosetta3: appkit: -makeFirstResponder: {responder:p} ({})",
            class.map_or("nil", |c| c.name_str())
        );
    }
    let responder = crate::text::editing_responder(receiver, responder);
    let current = window_of(receiver).first_responder;
    if current == responder {
        update_text_input(receiver);
        return 1;
    }
    // SAFETY: null or a live object this runtime allocated.
    if !unsafe { responder_agrees(current, "resignFirstResponder") } {
        return 0;
    }
    window_of(receiver).first_responder = responder;
    // SAFETY: as above.
    if unsafe { responder_agrees(responder, "becomeFirstResponder") } {
        update_text_input(receiver);
        return 1;
    }
    // A refusal leaves the window itself as the responder, which is the null
    // this file spells it with - unless the refusal already moved it on.
    if window_of(receiver).first_responder == responder {
        window_of(receiver).first_responder = std::ptr::null_mut();
    }
    update_text_input(receiver);
    0
}

/// Keep SDL's text-input stream paired with AppKit's first responder.
///
/// SDL leaves it disabled by default. Key events still arrive in that state,
/// which made text appear to work at the engine layer while every committed
/// character and IME composition was silently absent from the text client.
fn update_text_input(receiver: Id) {
    let window = window_of(receiver);
    if window.sdl.is_null() {
        return;
    }
    match crate::text::is_input_client(window.first_responder) {
        true => {
            set_text_input_area(window);
            sdl3::video::start_text_input(window.sdl);
        }
        false => {
            sdl3::video::stop_text_input(window.sdl);
        }
    }
}

fn set_text_input_area(window: &Window) {
    let area = crate::text::input_area(window.first_responder);
    // SAFETY: an input client or the text field whose editor it is; both are
    // live views from this runtime.
    if let Some((_, area)) = unsafe { crate::view::input_frame(area) } {
        sdl3::video::set_text_input_area(window.sdl, area, 0);
    }
}

/// Refresh SDL's IME rectangle after the visible editor changes geometry.
///
/// # Safety
///
/// `view` must be a live view from this runtime.
pub(crate) unsafe fn refresh_text_input_area_for(view: Id) {
    // SAFETY: the caller's obligation.
    let receiver = unsafe { crate::view::window_of(view) };
    if receiver.is_null() {
        return;
    }
    let window = window_of(receiver);
    if !window.sdl.is_null()
        && crate::text::is_input_client(window.first_responder)
        && crate::text::input_area(window.first_responder) == view
    {
        set_text_input_area(window);
    }
}

/// Send one of the two first-responder messages, or take silence for consent.
///
/// `NSResponder`'s own answer to both is yes, so a responder that implements
/// neither is not refusing - and neither is the window at the end of the chain,
/// which this file spells as a null responder.
///
/// # Safety
///
/// `responder` must be null or an object this runtime allocated.
unsafe fn responder_agrees(responder: Id, selector: &str) -> bool {
    if responder.is_null() {
        return true;
    }
    let sel = objc::sel::get(selector);
    // SAFETY: the caller's guarantee.
    let Some(method) = unsafe { objc::object::class_of(responder) }.and_then(|c| c.method(sel))
    else {
        return true;
    };
    // SAFETY: both messages are `BOOL (id, SEL)`.
    unsafe {
        let f: unsafe extern "C" fn(Id, Sel) -> u8 = std::mem::transmute(method.implementation());
        f(responder, sel) != 0
    }
}

unsafe extern "C" fn accepts_responder_change(_receiver: Id, _sel: Sel) -> u8 {
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

/// `-invalidateCursorRectsForView:`, which is the whole of a program's custom
/// cursor: AppKit answers it by discarding that view's rectangles and asking
/// the view to install them again through `-resetCursorRects`, and the cursor
/// a view hands to `-addCursorRect:cursor:` there is the one the pointer shows
/// over it. A runtime that ignored this left `-resetCursorRects` uncalled, so
/// the program's cursor was built and never reached the screen.
unsafe extern "C" fn invalidate_cursor_rects_for_view(_receiver: Id, _sel: Sel, view: Id) {
    // SAFETY: null or an object this runtime allocated.
    unsafe { crate::cursor::reset_rects(view) };
}

/// `-resetCursorRects` on the *window*, which rebuilds every view's.
unsafe extern "C" fn reset_cursor_rects(receiver: Id, _sel: Sel) {
    // SAFETY: null or the content view, which this crate made.
    unsafe { crate::cursor::reset_rects_in_tree(content_view_of(receiver)) };
}

unsafe extern "C" fn discard_cursor_rects(receiver: Id, _sel: Sel) {
    // SAFETY: as above.
    unsafe { crate::cursor::discard_rects_in_tree(content_view_of(receiver)) };
}

unsafe extern "C" fn enable_cursor_rects(_receiver: Id, _sel: Sel) {
    crate::cursor::set_rects_enabled(true);
}

/// `-disableCursorRects`, which a program uses while it is drawing its own
/// pointer: the rectangles stay, and none of them applies until it enables them
/// again.
unsafe extern "C" fn disable_cursor_rects(_receiver: Id, _sel: Sel) {
    crate::cursor::set_rects_enabled(false);
}

unsafe extern "C" fn are_cursor_rects_enabled(_receiver: Id, _sel: Sel) -> u8 {
    crate::cursor::rects_enabled() as u8
}

pub(crate) fn install() {
    let responder = objc::class::lookup(c"NSResponder").expect("declared before its methods");
    methods!(responder, {
        "becomeFirstResponder" => accepts_responder_change, c"c16@0:8";
        "resignFirstResponder" => accepts_responder_change, c"c16@0:8";
    });
    let class = objc::class::lookup(c"NSWindow").expect("declared before its methods");
    class_methods!(class, {
        "windowNumberAtPoint:belowWindowWithWindowNumber:" => window_number_at_point,
            c"q40@0:8{CGPoint=dd}16q32";
    });
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
        "convertPointFromScreen:" => convert_screen_to_base, c"{CGPoint=dd}32@0:8{CGPoint=dd}16";
        "convertPointToScreen:" => convert_base_to_screen, c"{CGPoint=dd}32@0:8{CGPoint=dd}16";
        "sendEvent:" => send_event, c"v24@0:8@16";
        "mouseLocationOutsideOfEventStream" => mouse_location_outside_of_event_stream,
            c"{CGPoint=dd}16@0:8";
        "convertRectFromScreen:" => convert_rect_from_screen,
            c"{CGRect={CGPoint=dd}{CGSize=dd}}48@0:8{CGRect={CGPoint=dd}{CGSize=dd}}16";
        "convertRectToScreen:" => convert_rect_to_screen,
            c"{CGRect={CGPoint=dd}{CGSize=dd}}48@0:8{CGRect={CGPoint=dd}{CGSize=dd}}16";
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
        "performClose:" => perform_close, c"v24@0:8@16";
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
        "invalidateCursorRectsForView:" => invalidate_cursor_rects_for_view, c"v24@0:8@16";
        "resetCursorRects" => reset_cursor_rects, c"v16@0:8";
        "enableCursorRects" => enable_cursor_rects, c"v16@0:8";
        "disableCursorRects" => disable_cursor_rects, c"v16@0:8";
        "areCursorRectsEnabled" => are_cursor_rects_enabled, c"c16@0:8";
        "discardCursorRects" => discard_cursor_rects, c"v16@0:8";
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn perform_close_respects_style_and_delegate_and_dispatches_close() {
        crate::bootstrap();
        let base = objc::class::lookup(c"NSWindow").unwrap();
        let subclass = objc::class::allocate_pair(Some(base), c"PerformCloseWindow", 0).unwrap();
        unsafe extern "C" fn veto(receiver: Id, sel: Sel, window: Id) -> u8 {
            assert_eq!(sel, objc::sel::get("windowShouldClose:"));
            assert_eq!(receiver, window);
            0
        }
        unsafe extern "C" fn allow(_receiver: Id, sel: Sel, window: Id) -> u8 {
            assert_eq!(sel, objc::sel::get("windowShouldClose:"));
            assert!(!window.is_null());
            1
        }
        unsafe extern "C" fn overridden_close(receiver: Id, sel: Sel) {
            assert_eq!(sel, objc::sel::get("close"));
            window_of(receiver).shown = 2;
        }
        subclass.add_method(
            objc::sel::get("windowShouldClose:"),
            veto as objc::class::Imp,
            c"c24@0:8@16",
        );
        subclass.add_method(
            objc::sel::get("close"),
            overridden_close as objc::class::Imp,
            c"v16@0:8",
        );
        objc::class::register_pair(subclass);
        let delegate_class = objc::class::allocate_pair(
            objc::class::lookup(c"NSObject"),
            c"PerformCloseDelegate",
            0,
        )
        .unwrap();
        delegate_class.add_method(
            objc::sel::get("windowShouldClose:"),
            allow as objc::class::Imp,
            c"c24@0:8@16",
        );
        objc::class::register_pair(delegate_class);
        let delegate = objc::object::create_instance(delegate_class, 0);
        let window = objc::object::create_instance(subclass, 0);
        let plain = objc::object::create_instance(base, 0);
        // SAFETY: live runtime windows; `-performClose:` is `void (id, SEL, id)`.
        unsafe {
            let send: unsafe extern "C" fn(Id, Sel, Id) = std::mem::transmute(objc::msg::entry());
            let sel = objc::sel::get("performClose:");
            window_of(window).shown = 1;
            send(window, sel, delegate);
            assert_eq!(window_of(window).shown, 1);
            window_of(window).style_mask = style::TITLED | style::CLOSABLE;
            send(window, sel, delegate);
            assert_eq!(window_of(window).shown, 1);
            window_of(window).delegate = delegate;
            send(window, sel, std::ptr::null_mut());
            assert_eq!(window_of(window).shown, 2);
            window_of(plain).shown = 1;
            window_of(plain).style_mask = style::TITLED | style::CLOSABLE;
            send(plain, sel, delegate);
            assert_eq!(window_of(plain).shown, 0);
            window_of(plain).shown = 1;
            window_of(plain).delegate =
                objc::object::create_instance(objc::class::lookup(c"NSObject").unwrap(), 0);
            send(plain, sel, std::ptr::null_mut());
            assert_eq!(window_of(plain).shown, 0);
        }
    }

    static REDIRECT: std::sync::Mutex<(usize, usize)> = std::sync::Mutex::new((0, 0));

    unsafe extern "C" fn redirecting_become(_receiver: Id, _sel: Sel) -> u8 {
        let (window, to) = *REDIRECT.lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: the window and view the test made.
        unsafe { make_first_responder(window as Id, objc::sel::get("x"), to as Id) };
        1
    }

    #[test]
    fn a_window_claims_no_graphics_api_until_one_is_asked_for() {
        use sdl3::video::flags;

        // The default. A window with neither flag has no `EGLSurface`, so
        // `SDL_Vulkan_CreateSurface` can convert it and Metal presentation
        // needs nothing known in advance about the guest.
        let plain = creation_flags(style::TITLED | style::RESIZABLE, Api::Undecided);
        assert_eq!(plain & flags::OPENGL, 0);
        assert_eq!(plain & flags::VULKAN, 0);

        // A guest that asks for an `NSOpenGLContext` gets the window rebuilt
        // as this, because SDL will not convert to OpenGL.
        let gl = creation_flags(style::TITLED | style::RESIZABLE, Api::OpenGl);
        assert_ne!(gl & flags::OPENGL, 0);
        assert_eq!(gl & flags::VULKAN, 0);

        // Never both, whatever the style: SDL refuses such a window, and the
        // whole point is that at most one driver owns the surface.
        for style in [0, style::TITLED, style::RESIZABLE, style::CLOSABLE] {
            for api in [Api::Undecided, Api::OpenGl] {
                let flags = creation_flags(style, api);
                assert!(
                    flags & flags::OPENGL == 0 || flags & flags::VULKAN == 0,
                    "a window was asked for both graphics APIs"
                );
            }
        }

        // The rest of the mapping is unchanged by any of it.
        let api = Api::Undecided;
        assert_ne!(creation_flags(style::RESIZABLE, api) & flags::RESIZABLE, 0);
        assert_eq!(creation_flags(style::TITLED, api) & flags::BORDERLESS, 0);
        assert_ne!(creation_flags(0, api) & flags::BORDERLESS, 0);
        assert_ne!(creation_flags(0, api) & flags::HIDDEN, 0);
    }

    /// A window becomes an OpenGL window only when one is asked for, and
    /// keeps its identity and its place when it does.
    #[test]
    fn a_window_rebuilt_for_opengl_keeps_what_it_had() {
        crate::bootstrap();
        let object =
            objc::object::create_instance(objc::class::lookup(c"NSWindow").expect("NSWindow"), 0);
        // SAFETY: a fresh window this test made.
        let made = unsafe {
            init_with_content_rect_for_tests(
                object,
                NSRect {
                    origin: NSPoint { x: 0.0, y: 0.0 },
                    size: NSSize {
                        width: 320.0,
                        height: 240.0,
                    },
                },
            )
        };
        if !made {
            return;
        }
        // SAFETY: a window this test made.
        let before = unsafe { sdl_window(object) };
        assert_eq!(
            sdl3::video::window_flags(before) & sdl3::video::flags::OPENGL,
            0,
            "a window claimed OpenGL before anything asked for it"
        );
        sdl3::video::set_window_title(before, c"a title");
        sdl3::video::set_window_position(before, 64, 48);
        let size = sdl3::video::window_size(before).expect("a window");

        assert!(rebuild_for_opengl(object));
        // SAFETY: as above.
        let after = unsafe { sdl_window(object) };
        assert_ne!(after, before, "the window was not rebuilt");
        assert_ne!(
            sdl3::video::window_flags(after) & sdl3::video::flags::OPENGL,
            0,
            "the rebuilt window is not an OpenGL window"
        );
        assert_eq!(sdl3::video::window_size(after), Some(size));
        // The title carries across exactly. `set_window_title` brands what
        // it is given and `create_window` does not, which is what keeps the
        // rebuild from branding an already branded title a second time.
        let title = sdl3::video::window_title(after);
        let title = String::from_utf8_lossy(title.to_bytes());
        assert!(
            title.starts_with("a title"),
            "the rebuilt window lost its title: {title:?}"
        );
        assert_eq!(
            title.matches("Rosetta 3").count(),
            1,
            "the rebuilt window's title was branded twice: {title:?}"
        );
        // The event table has to follow the window, or every event for it
        // stops resolving to this object.
        assert_eq!(window_for(after), object);
        assert!(sdl_windows().contains(&after));
        assert!(!sdl_windows().contains(&before));

        // Asking twice is not two rebuilds.
        assert!(rebuild_for_opengl(object));
        // SAFETY: as above.
        assert_eq!(unsafe { sdl_window(object) }, after);
    }

    #[test]
    fn a_new_first_responder_may_hand_it_on() {
        crate::bootstrap();
        let window =
            objc::object::create_instance(objc::class::lookup(c"NSWindow").expect("NSWindow"), 0);
        let view = objc::class::lookup(c"NSView").expect("NSView");
        let handler = objc::object::create_instance(view, 0);
        let redirecting = objc::class::allocate_pair(Some(view), c"RedirectingView", 0)
            .expect("a class of this name is not declared elsewhere");
        redirecting.add_method(
            objc::sel::get("becomeFirstResponder"),
            redirecting_become as objc::class::Imp,
            c"c16@0:8",
        );
        objc::class::register_pair(redirecting);
        let engine = objc::object::create_instance(redirecting, 0);
        *REDIRECT.lock().unwrap() = (window as usize, handler as usize);

        // SAFETY: the window and view above, and `-makeFirstResponder:` is
        // `BOOL (id, SEL, id)`.
        let answered = unsafe { make_first_responder(window, objc::sel::get("x"), engine) };
        assert_eq!(answered, 1);
        assert_eq!(window_of(window).first_responder, handler);
    }
}
