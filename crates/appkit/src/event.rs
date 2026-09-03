use foundation::geometry::NSPoint;
use objc::object::Id;

use crate::nsevent::{self, Fields, kind};
use crate::window;

/// Drain SDL's queue once, delivering what this tree can deliver.
///
/// `app` is the application, which is the object a window notification is posted
/// about when the event names no window this crate made.
pub fn pump(app: Id) {
    let _ = app;
    debug_resize();
    debug_key();
    crate::script::run();
    trace_views();
    while let Some(event) = sdl3::video::poll() {
        deliver(event);
    }
}

/// `ROSETTA3_DEBUG_RESIZE=WxH@SECONDS`: resize the program's window once, that
/// many seconds in.
///
/// A resize is the one window event that cannot be produced without a person
/// dragging a corner, which makes "does the program follow its window" the one
/// thing about a window that is untestable from a script. `SDL_SetWindowSize`
/// on a resizable window raises the same `SDL_EVENT_WINDOW_RESIZED` the
/// compositor would, so everything downstream of the pump - the content view's
/// frame, the subviews, the delegate's `-windowDidResize:` - is exercised for
/// real.
fn debug_resize() {
    use std::sync::OnceLock;
    use std::time::Instant;

    static REQUEST: OnceLock<Option<(i32, i32, f64)>> = OnceLock::new();
    static START: OnceLock<Instant> = OnceLock::new();
    static DONE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

    let request = *REQUEST.get_or_init(|| {
        let value = std::env::var("ROSETTA3_DEBUG_RESIZE").ok()?;
        let (size, after) = value.split_once('@').unwrap_or((value.as_str(), "5"));
        let (width, height) = size.split_once('x')?;
        Some((
            width.trim().parse().ok()?,
            height.trim().parse().ok()?,
            after.trim().parse().unwrap_or(5.0),
        ))
    });
    let Some((width, height, after)) = request else {
        return;
    };
    if DONE.load(std::sync::atomic::Ordering::Relaxed)
        || START.get_or_init(Instant::now).elapsed().as_secs_f64() < after
    {
        return;
    }
    DONE.store(true, std::sync::atomic::Ordering::Relaxed);
    let Some(sdl) = window::sdl_windows().into_iter().next() else {
        return;
    };
    eprintln!("rosetta3: appkit: ROSETTA3_DEBUG_RESIZE: resizing to {width}x{height}");
    sdl3::video::set_window_size(sdl, width, height);
}

/// `ROSETTA3_DEBUG_KEY=NAME[,NAME...]@SECONDS`: press and release each key in
/// turn, that many seconds in.
///
/// The companion to `ROSETTA3_DEBUG_RESIZE`, and there for the same reason: a
/// keystroke cannot be produced from a script under a compositor that does not
/// implement the virtual-keyboard protocol, so "does the program answer the
/// keyboard" is otherwise only testable by hand. The event goes through
/// [`deliver`], so the keymap, `-keyDown:` and the responder chain are exercised
/// exactly as a real press would exercise them.
fn debug_key() {
    use std::sync::OnceLock;
    use std::time::Instant;

    static REQUEST: OnceLock<Option<(Vec<u32>, f64)>> = OnceLock::new();
    static START: OnceLock<Instant> = OnceLock::new();
    static DONE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

    let request = REQUEST.get_or_init(|| {
        let value = std::env::var("ROSETTA3_DEBUG_KEY").ok()?;
        let (names, after) = value.split_once('@').unwrap_or((value.as_str(), "5"));
        let keys: Vec<u32> = names.split(',').filter_map(|n| scancode(n.trim())).collect();
        match keys.is_empty() {
            true => None,
            false => Some((keys, after.trim().parse().unwrap_or(5.0))),
        }
    });
    let Some((keys, after)) = request else {
        return;
    };
    let after = *after;
    if DONE.load(std::sync::atomic::Ordering::Relaxed)
        || START.get_or_init(Instant::now).elapsed().as_secs_f64() < after
    {
        return;
    }
    DONE.store(true, std::sync::atomic::Ordering::Relaxed);
    let Some(window) = window::sdl_windows()
        .into_iter()
        .map(sdl3::video::window_id)
        .next()
    else {
        return;
    };
    eprintln!("rosetta3: appkit: ROSETTA3_DEBUG_KEY: pressing scancodes {keys:?}");
    for scancode in keys {
        for down in [true, false] {
            deliver(sdl3::video::Event::Key {
                window,
                scancode: *scancode,
                key: 0,
                modifiers: 0,
                down,
                repeat: false,
            });
        }
    }
}

/// The USB HID usage id `ROSETTA3_DEBUG_KEY`'s value names, or a raw number.
pub(crate) fn scancode(name: &str) -> Option<u32> {
    use sdl3::keycodes::scancode;
    Some(match name.to_ascii_lowercase().as_str() {
        "return" | "enter" => scancode::RETURN,
        "escape" => scancode::ESCAPE,
        "tab" => scancode::TAB,
        "space" => scancode::SPACE,
        "right" => scancode::RIGHT,
        "left" => scancode::LEFT,
        "down" => scancode::DOWN,
        "up" => scancode::UP,
        other => other.parse().ok()?,
    })
}

/// `ROSETTA3_TRACE_VIEWS=SECONDS`: print each window and the tree of views under
/// it, once, that many seconds in.
///
/// Companion to `ROSETTA3_DEBUG_RESIZE`, and needed for the same reason: the
/// question a resize raises is which view ended up the wrong size, and the view
/// that matters is one the *guest* made - so it is in no debugger's symbol
/// table, and a release build has inlined every accessor that would name it.
/// Printed after the window's own size so the two can be compared on sight.
fn trace_views() {
    use std::sync::OnceLock;
    use std::time::Instant;

    static AFTER: OnceLock<Option<f64>> = OnceLock::new();
    static START: OnceLock<Instant> = OnceLock::new();
    static DONE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

    let Some(after) = *AFTER.get_or_init(|| {
        std::env::var("ROSETTA3_TRACE_VIEWS")
            .ok()
            .map(|value| value.trim().parse().unwrap_or(5.0))
    }) else {
        return;
    };
    if DONE.load(std::sync::atomic::Ordering::Relaxed)
        || START.get_or_init(Instant::now).elapsed().as_secs_f64() < after
    {
        return;
    }
    DONE.store(true, std::sync::atomic::Ordering::Relaxed);

    let mut out = String::new();
    for object in window::all() {
        // SAFETY: a window this crate made.
        let sdl = unsafe { window::sdl_window(object) };
        let size = sdl3::video::window_size(sdl).unwrap_or((0, 0));
        out.push_str(&format!(
            "rosetta3: appkit: window {object:p} sdl {}x{}\n",
            size.0, size.1
        ));
        // SAFETY: null or a view this runtime allocated.
        unsafe { crate::view::describe(window::content_view_of(object), 1, &mut out) };
    }
    eprint!("{out}");
}

pub(crate) fn deliver(event: sdl3::video::Event) {
    use sdl3::video::Event;
    match event {
        // A quit is the whole application, and is the one event that is not
        // about a window: it is what a program's `-applicationShouldTerminate:`
        // would be asked about, and until there is one it ends the run loop the
        // way `-terminate:` does.
        Event::Quit => {
            let app = crate::application::shared();
            if !app.is_null() {
                // SAFETY: `-terminate:` is `void (id, SEL, id)` on the shared
                // application, which this crate installed.
                unsafe {
                    let f: unsafe extern "C" fn(Id, objc::sel::Sel, Id) =
                        std::mem::transmute(objc::msg::entry());
                    f(app, objc::sel::get("terminate:"), std::ptr::null_mut());
                }
            }
        }
        Event::WindowClosed { window } => {
            post(window, window::WILL_CLOSE);
        }
        Event::WindowResized {
            window,
            width,
            height,
        } => {
            // The content view follows the window, as it does on
            // `-setContentView:` and for the same reason: a view that kept its
            // old size after a resize would draw into part of the window and
            // report a backing store the window does not have.
            let object = object_for(window);
            let content = window::content_view_of(object);
            if !content.is_null() {
                let frame = foundation::geometry::NSRect {
                    origin: foundation::geometry::NSPoint::default(),
                    size: foundation::geometry::NSSize {
                        width: width as f64,
                        height: height as f64,
                    },
                };
                // SAFETY: `-setFrame:` is `void (id, SEL, NSRect)`.
                unsafe {
                    let f: unsafe extern "C" fn(Id, objc::sel::Sel, foundation::geometry::NSRect) =
                        std::mem::transmute(objc::msg::entry());
                    f(content, objc::sel::get("setFrame:"), frame);
                }
            }
            // A resized view's tracking rectangle is the wrong shape, and
            // AppKit asks for a new one here rather than resizing the old.
            // SAFETY: null or a view this crate made.
            unsafe { crate::view::update_tracking_areas(content) };
            post(window, window::DID_RESIZE);
        }
        Event::WindowFocus { window, gained } => {
            let object = object_for(window);
            if !object.is_null() && gained {
                window::set_key_window(object);
                // SDL drops relative mouse mode when focus goes, and a window
                // made while the guest was disassociated never entered it.
                crate::screen::apply_mouse_association();
            }
            post(
                window,
                match gained {
                    true => window::DID_BECOME_KEY,
                    false => window::DID_RESIGN_KEY,
                },
            );
        }
        Event::Key {
            window: id,
            scancode,
            key,
            modifiers,
            down,
            repeat,
        } => {
            let modifiers = crate::keymap::modifiers(modifiers);
            let code = crate::keymap::virtual_key(scancode);
            let text = crate::keymap::characters(scancode, key);
            let fields = |window| Fields {
                kind: match down {
                    true => kind::KEY_DOWN,
                    false => kind::KEY_UP,
                },
                modifiers,
                key_code: code,
                characters: objc::arc::autorelease(foundation::string::from_str(&text)),
                repeat,
                window,
                ..Fields::default()
            };
            // A modifier is `-flagsChanged:`, not a key down: AppKit has no
            // key-up for one, and a guest that treated it as a key would see
            // shift as a character.
            match crate::keymap::is_modifier(scancode) {
                true => send(id, None, |w, _| Fields {
                    kind: kind::FLAGS_CHANGED,
                    ..fields(w)
                }),
                false => send(id, None, |w, _| fields(w)),
            }
        }
        // Text is what `-characters` already carries. A second delivery would
        // be the same keystroke twice.
        Event::Text { .. } => {}
        Event::MouseMotion {
            window: id,
            x,
            y,
            dx,
            dy,
            buttons,
        } => {
            let held = crate::button::held(buttons);
            // A disassociated pointer reports the deltas and none of the
            // movement: the location a guest reads back is where the cursor was
            // when it broke the link, which is what CoreGraphics does and what
            // SDL, still tracking a position inside the window, does not.
            let (x, y) = track_pointer(x, y);
            // Before the drop below: a cursor rectangle applies whether or not
            // anything asked for `-mouseMoved:`, and AppKit re-decides the
            // pointer on every move rather than only when a rectangle changes.
            crate::cursor::apply();
            if held.is_none() && !wants_mouse_moved(object_for(id)) {
                if tracing() {
                    eprintln!(
                        "rosetta3: appkit: -mouseMoved: dropped: the window did not ask for it \
                         and no view is tracking"
                    );
                }
                return;
            }
            send(id, Some((x, y)), |window, location| Fields {
                kind: crate::button::dragged_kind(held),
                modifiers: current_modifiers(),
                location,
                delta: NSPoint {
                    x: dx as f64,
                    y: dy as f64,
                },
                button: held.map_or(0, crate::button::number),
                window,
                ..Fields::default()
            });
        }
        Event::MouseButton {
            window: id,
            button,
            down,
            clicks,
            x,
            y,
        } => {
            let kind = crate::button::event_kind(button, down);
            let (x, y) = track_pointer(x, y);
            send(id, Some((x, y)), |window, location| Fields {
                kind,
                modifiers: current_modifiers(),
                location,
                button: crate::button::number(button),
                clicks: i64::from(clicks),
                window,
                ..Fields::default()
            });
        }
        // A wheel event carries no position, so it goes to the view the pointer
        // was last over - which is where the wheel turned.
        Event::MouseWheel { window: id, x, y } => {
            let at = *POINTER.lock().unwrap_or_else(|e| e.into_inner());
            send(id, Some(at), |window, location| Fields {
                kind: kind::SCROLL_WHEEL,
                modifiers: current_modifiers(),
                location,
                delta: NSPoint {
                    x: x as f64,
                    y: y as f64,
                },
                window,
                ..Fields::default()
            });
        }
        Event::Other(_) => {}
    }
}

fn current_modifiers() -> u64 {
    crate::keymap::modifiers(sdl3::video::keyboard_modifiers())
}

/// A pointer position SDL reports against the window's top-left, in the
/// bottom-left frame `-locationInWindow` promises.
fn location(window: Id, x: f32, y: f32) -> NSPoint {
    let content = window::content_view_of(window);
    let height = match content.is_null() {
        true => 0.0,
        // SAFETY: `-bounds` is `NSRect (id, SEL)` on a view this crate made.
        // `NSRect` returns through a buffer, so this is the `_stret` entry:
        // through the other one the receiver would be the return slot.
        false => unsafe {
            let f: unsafe extern "C" fn(Id, objc::sel::Sel) -> foundation::geometry::NSRect =
                std::mem::transmute(objc::msg::entry_stret());
            f(content, objc::sel::get("bounds")).size.height
        },
    };
    crate::geometry::point_in_window(x as f64, y as f64, height)
}

/// Hand one input event to its window, as `-[NSApplication sendEvent:]` does.
///
/// Through the window and not straight to a responder, because `-sendEvent:` is
/// where a program gets to see every event before anything else does - and
/// Roblox overrides it on its own `RBXWindow`. That override is how the engine
/// receives the keyboard at all: the first responder while a text field is
/// focused is Roblox's `InputMethodHandler`, so a runtime that dispatched
/// straight to the responder handed every keystroke to a text buffer and gave
/// the engine nothing.
///
/// `at` is where the pointer was, in SDL's window coordinates, for the events
/// that have a position.
fn send(id: u32, at: Option<(f32, f32)>, fields: impl FnOnce(Id, NSPoint) -> Fields) {
    let window = object_for(id);
    if window.is_null() {
        return;
    }
    let point = at.map(|(x, y)| location(window, x, y)).unwrap_or_default();
    let event = nsevent::make(fields(window, point));
    let sel = objc::sel::get("sendEvent:");
    // SAFETY: `-sendEvent:` is `void (id, SEL, NSEvent *)`, and every window
    // has one - this crate installs the default on `NSWindow`.
    unsafe {
        let f: unsafe extern "C" fn(Id, objc::sel::Sel, Id) =
            std::mem::transmute(objc::msg::entry());
        f(window, sel, event);
    }
}

/// `-[NSWindow sendEvent:]`: route one event to the responder it belongs to.
///
/// The default AppKit routing, and the two halves of it are not the same rule:
/// an event with a position goes to the view under it, and a key event goes to
/// the first responder. Either way the search walks up the responder chain from
/// there, and an event nothing implements is dropped.
pub fn route(window: Id, event: Id) {
    let kind = nsevent::kind_of(event);
    let Some(selector) = responder_selector(kind) else {
        return;
    };
    let sel = objc::sel::get(selector);
    let start = match positional(kind) {
        // SAFETY: null or the content view, which this crate made.
        true => unsafe {
            crate::view::hit_test(
                window::content_view_of(window),
                nsevent::location_of(event),
            )
        },
        false => responder_of(window),
    };
    let Some((responder, method)) = chain(start, sel) else {
        if tracing() {
            eprintln!("rosetta3: appkit: -{selector}: nothing from {start:p} up implements it");
        }
        return;
    };
    if tracing() {
        // SAFETY: a live object this runtime allocated.
        let class = unsafe { objc::object::class_of(responder) };
        eprintln!(
            "rosetta3: appkit: -{selector} to {responder:p} ({})",
            class.map_or("no class", |c| c.name_str()),
        );
    }
    // SAFETY: every responder method here is `void (id, SEL, NSEvent *)`.
    unsafe {
        let f: unsafe extern "C" fn(Id, objc::sel::Sel, Id) =
            std::mem::transmute(method.implementation());
        f(responder, sel, event);
    }
}

/// The responder method an event type is delivered through.
fn responder_selector(kind: u64) -> Option<&'static str> {
    Some(match kind {
        kind::LEFT_MOUSE_DOWN => "mouseDown:",
        kind::LEFT_MOUSE_UP => "mouseUp:",
        kind::RIGHT_MOUSE_DOWN => "rightMouseDown:",
        kind::RIGHT_MOUSE_UP => "rightMouseUp:",
        kind::OTHER_MOUSE_DOWN => "otherMouseDown:",
        kind::OTHER_MOUSE_UP => "otherMouseUp:",
        kind::MOUSE_MOVED => "mouseMoved:",
        kind::LEFT_MOUSE_DRAGGED => "mouseDragged:",
        kind::RIGHT_MOUSE_DRAGGED => "rightMouseDragged:",
        kind::OTHER_MOUSE_DRAGGED => "otherMouseDragged:",
        kind::MOUSE_ENTERED => "mouseEntered:",
        kind::MOUSE_EXITED => "mouseExited:",
        kind::SCROLL_WHEEL => "scrollWheel:",
        kind::KEY_DOWN => "keyDown:",
        kind::KEY_UP => "keyUp:",
        kind::FLAGS_CHANGED => "flagsChanged:",
        _ => return None,
    })
}

/// Whether an event is about a place rather than about the keyboard.
fn positional(kind: u64) -> bool {
    !matches!(kind, kind::KEY_DOWN | kind::KEY_UP | kind::FLAGS_CHANGED)
}

/// Walk the responder chain from `start` for something that implements `sel`.
///
/// The chain is the superview chain and then the window, which is AppKit's for
/// a view in a window. Bounded, because a subview list a guest built with a
/// cycle in it would otherwise hang inside event delivery.
fn chain(start: Id, sel: objc::sel::Sel) -> Option<(Id, &'static objc::class::Method)> {
    let mut responder = start;
    let mut fallback = None;
    for _ in 0..64 {
        if responder.is_null() {
            return fallback;
        }
        // SAFETY: null or a live object this runtime allocated.
        if let Some(method) = unsafe { objc::object::class_of(responder) }.and_then(|c| c.method(sel))
        {
            // A text view's own key handling is the *last* resort, not the
            // first. Roblox makes its `NSTextView` the first responder while a
            // field is focused and still expects its engine view to see the
            // keystroke - it reads the text view back afterwards for what an
            // input method composed. Taking the text view's implementation here
            // would end the search at a buffer nothing else is told about.
            let ours = crate::text::is_default_key_handling(method.implementation() as *const ());
            match ours {
                false => return Some((responder, method)),
                true => fallback = fallback.or(Some((responder, method))),
            }
        }
        // SAFETY: as above.
        let next = unsafe { crate::view::superview_of(responder) };
        responder = match next.is_null() {
            // SAFETY: as above.
            true => unsafe { crate::view::window_of(responder) },
            false => next,
        };
    }
    fallback
}

/// Where the pointer was last seen, in SDL's window coordinates.
///
/// A wheel event carries no position and a click carries one only where SDL
/// filled it in, so the last motion is what says which view the event is about.
static POINTER: std::sync::Mutex<(f32, f32)> = std::sync::Mutex::new((0.0, 0.0));

/// Record where an event says the pointer is, and answer where a guest is to be
/// told it is.
///
/// The two differ only while the pointer is disassociated, and then they differ
/// for every event and not only for motion: CoreGraphics leaves the cursor
/// exactly where it was, so a click and a wheel turn arrive at that point too.
/// SDL keeps tracking a position inside the window through relative mode, and a
/// click that took SDL's answer would move the frozen point to somewhere the
/// cursor never was - which is a hit test, and so a first responder, against the
/// wrong view.
fn track_pointer(x: f32, y: f32) -> (f32, f32) {
    let mut pointer = POINTER.lock().unwrap_or_else(|e| e.into_inner());
    if crate::screen::mouse_is_associated() {
        *pointer = (x, y);
    }
    *pointer
}

/// Where the guest is to be told the pointer is on the desktop, for the calls
/// that ask the window system rather than reading an event.
///
/// `None` while the pointer and cursor move together, when the window system is
/// the better answer. While they do not, this is the whole point of the call:
/// `+[NSEvent mouseLocation]` answering a pointer that is still moving, against
/// events that all carry the frozen one, is a program being told two different
/// things about the same cursor - and a program that decides whether its own
/// window is focused from the pointer (Roblox does) then hands the keyboard
/// away in the middle of mouse look.
pub(crate) fn frozen_pointer() -> Option<(f32, f32)> {
    if crate::screen::mouse_is_associated() {
        return None;
    }
    let (x, y) = *POINTER.lock().unwrap_or_else(|e| e.into_inner());
    let origin = crate::window::sdl_windows()
        .into_iter()
        .next()
        .and_then(sdl3::video::window_position)
        .unwrap_or((0, 0));
    Some((x + origin.0 as f32, y + origin.1 as f32))
}

/// Whether `-mouseMoved:` should be delivered at all.
///
/// AppKit drops it unless the window asked for it *or* a view is tracking, and
/// the second half is not optional here: Roblox never sends
/// `-setAcceptsMouseMovedEvents:YES` and installs an `NSTrackingArea` instead,
/// so a runtime that only honoured the window flag has a program whose pointer
/// never moves.
fn wants_mouse_moved(window: Id) -> bool {
    // SAFETY: null or the content view, which this crate made.
    window::accepts_mouse_moved_events(window)
        || unsafe { crate::view::any_tracking_area(window::content_view_of(window)) }
}

/// The object an input event goes to: the window's first responder, or its
/// content view when nothing has been made one.
///
/// `-firstResponder` answers with the window itself at the end of its own
/// chain, and a window is not what draws - so that answer means "the view".
fn responder_of(window: Id) -> Id {
    let sel = objc::sel::get("firstResponder");
    // SAFETY: `-firstResponder` is `id (id, SEL)` on a window this crate made.
    let responder = unsafe {
        let f: unsafe extern "C" fn(Id, objc::sel::Sel) -> Id =
            std::mem::transmute(objc::msg::entry());
        f(window, sel)
    };
    match responder == window || responder.is_null() {
        true => window::content_view_of(window),
        false => responder,
    }
}

/// The `NSWindow` an SDL window id names.
///
/// SDL identifies a window by a small integer in an event and by a pointer
/// everywhere else, and `crates/sdl3` does not expose the mapping - so this
/// walks the windows this crate made and asks each one. There are never more
/// than a handful.
fn object_for(id: u32) -> Id {
    for object in window::all() {
        // SAFETY: a window this crate made.
        let sdl = unsafe { window::sdl_window(object) };
        if !sdl.is_null() && sdl3::video::window_id(sdl) == id {
            return object;
        }
    }
    std::ptr::null_mut()
}

/// Post a window notification, to the centre and to the window's delegate.
///
/// Both, for the reason `-finishLaunching` sends the application's delegate
/// method directly as well: AppKit registers a window's delegate for the
/// notifications it declares the moment it is set, so a program that only
/// implements `-windowDidResize:` never registers as an observer of anything.
/// Set to print every window notification, and whether anything took it.
///
/// A window event that reaches nobody is invisible from both ends - the program
/// simply does not react - and a release build inlines every function on this
/// path, so a breakpoint cannot answer it either. This is the answer: the name,
/// the window, its delegate, and whether that delegate implements the method.
pub const TRACE_ENV: &str = "ROSETTA3_TRACE_EVENT";

pub(crate) fn tracing() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os(TRACE_ENV).is_some())
}

fn post(id: u32, name: &str) {
    let object = object_for(id);
    if object.is_null() {
        if tracing() {
            eprintln!("rosetta3: appkit: {name}: SDL window {id} is no window this crate made");
        }
        return;
    }
    let delegate = window::delegate_of(object);
    if tracing() {
        eprintln!("rosetta3: appkit: {name}: window {object:p}, delegate {delegate:p}");
    }
    if !delegate.is_null() {
        let selector = delegate_selector(name);
        let notification = foundation::notification::make(name, object, std::ptr::null_mut());
        // SAFETY: a window delegate method is
        // `void (id, SEL, NSNotification *)`.
        let sel = objc::sel::get(selector);
        // SAFETY: null or a live object this runtime allocated.
        let method = unsafe { objc::object::class_of(delegate) }.and_then(|c| c.method(sel));
        if tracing() {
            eprintln!(
                "rosetta3: appkit:   -{selector} is {}",
                match method.is_some() {
                    true => "implemented by the delegate",
                    false => "NOT implemented by the delegate",
                }
            );
        }
        if let Some(method) = method {
            // SAFETY: the prototype above.
            unsafe {
                let f: unsafe extern "C" fn(Id, objc::sel::Sel, Id) =
                    std::mem::transmute(method.implementation());
                f(delegate, sel, notification);
            }
        }
    }
    foundation::notification::post(name, object, std::ptr::null_mut());
}

/// The delegate method a notification name goes with.
///
/// The mapping is Apple's and is not derivable: `NSWindowDidResizeNotification`
/// is `-windowDidResize:`, which is the name without `NS` and without
/// `Notification` - but only because every one of them happens to be, and a
/// table is what makes that a decision rather than a coincidence.
fn delegate_selector(name: &str) -> &'static str {
    match name {
        window::DID_RESIZE => "windowDidResize:",
        window::DID_MOVE => "windowDidMove:",
        window::WILL_CLOSE => "windowWillClose:",
        window::DID_BECOME_KEY => "windowDidBecomeKey:",
        window::DID_RESIGN_KEY => "windowDidResignKey:",
        window::DID_MINIATURIZE => "windowDidMiniaturize:",
        window::DID_DEMINIATURIZE => "windowDidDeminiaturize:",
        window::WILL_ENTER_FULL_SCREEN => "windowWillEnterFullScreen:",
        window::WILL_EXIT_FULL_SCREEN => "windowWillExitFullScreen:",
        window::DID_ENTER_FULL_SCREEN => "windowDidEnterFullScreen:",
        window::DID_EXIT_FULL_SCREEN => "windowDidExitFullScreen:",
        window::DID_CHANGE_BACKING_PROPERTIES => "windowDidChangeBackingProperties:",
        other => objc::fail!("appkit::event: {other} has no delegate method in this table"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every name this crate publishes has a delegate method, so a notification
    /// cannot be posted with nowhere to send it.
    #[test]
    fn every_window_notification_has_a_delegate_method() {
        for name in crate::NOTIFICATION_NAMES {
            if name.starts_with("NSWindow") {
                assert!(!delegate_selector(name).is_empty(), "{name}");
            }
        }
    }
}
