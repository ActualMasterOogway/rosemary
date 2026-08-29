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
    while let Some(event) = sdl3::video::poll() {
        deliver(event);
    }
}

fn deliver(event: sdl3::video::Event) {
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
            post(window, window::DID_RESIZE);
        }
        Event::WindowFocus { window, gained } => {
            let object = object_for(window);
            if !object.is_null() && gained {
                window::set_key_window(object);
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
            match is_modifier(scancode) {
                true => send(id, "flagsChanged:", |w| Fields { kind: kind::FLAGS_CHANGED, ..fields(w) }),
                false => send(id, match down {
                    true => "keyDown:",
                    false => "keyUp:",
                }, fields),
            }
        }
        // Text is what `-characters` already carries. A second delivery would
        // be the same keystroke twice.
        Event::Text { .. } => {}
        Event::MouseMotion { window: id, x, y, dx, dy, buttons } => {
            let held = held_button(buttons);
            let selector = match held {
                Some(0) => "mouseDragged:",
                Some(1) => "otherMouseDragged:",
                Some(_) => "rightMouseDragged:",
                None => "mouseMoved:",
            };
            if held.is_none() && !window::accepts_mouse_moved_events(object_for(id)) {
                return;
            }
            send(id, selector, |window| Fields {
                kind: match held {
                    Some(0) => kind::LEFT_MOUSE_DRAGGED,
                    Some(1) => kind::OTHER_MOUSE_DRAGGED,
                    Some(_) => kind::RIGHT_MOUSE_DRAGGED,
                    None => kind::MOUSE_MOVED,
                },
                modifiers: current_modifiers(),
                location: location(window, x, y),
                delta: NSPoint { x: dx as f64, y: dy as f64 },
                button: held.unwrap_or(0),
                window,
                ..Fields::default()
            });
        }
        Event::MouseButton { window: id, button, down, clicks, x, y } => {
            let (selector, kind) = match (button, down) {
                (1, true) => ("mouseDown:", kind::LEFT_MOUSE_DOWN),
                (1, false) => ("mouseUp:", kind::LEFT_MOUSE_UP),
                (3, true) => ("rightMouseDown:", kind::RIGHT_MOUSE_DOWN),
                (3, false) => ("rightMouseUp:", kind::RIGHT_MOUSE_UP),
                (_, true) => ("otherMouseDown:", kind::OTHER_MOUSE_DOWN),
                (_, false) => ("otherMouseUp:", kind::OTHER_MOUSE_UP),
            };
            send(id, selector, |window| Fields {
                kind,
                modifiers: current_modifiers(),
                location: location(window, x, y),
                // AppKit numbers from zero, SDL from one.
                button: i64::from(button).saturating_sub(1),
                clicks: i64::from(clicks),
                window,
                ..Fields::default()
            });
        }
        Event::MouseWheel { window: id, x, y } => {
            send(id, "scrollWheel:", |window| Fields {
                kind: kind::SCROLL_WHEEL,
                modifiers: current_modifiers(),
                delta: NSPoint { x: x as f64, y: y as f64 },
                window,
                ..Fields::default()
            });
        }
        Event::Other(_) => {}
    }
}

/// Whether a scancode is a modifier, which AppKit reports as `-flagsChanged:`
/// and never as a key.
fn is_modifier(scancode: u32) -> bool {
    (224..=231).contains(&scancode) || scancode == 57
}

/// The lowest button held, as AppKit numbers them, or `None` for none.
///
/// SDL's mask has bit 0 for the left button and bit 2 for the right, which is
/// AppKit's numbering already - the middle button is 1 in both.
fn held_button(buttons: u32) -> Option<i64> {
    (0..5).find(|b| buttons & (1 << b) != 0)
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

/// Deliver one input event to the window's responder.
///
/// The responder, not the window: AppKit walks the chain from the first
/// responder, and every method here is one a view implements. A window whose
/// responder does not implement the selector drops the event, which is what
/// AppKit does once the chain runs out.
fn send(id: u32, selector: &str, fields: impl FnOnce(Id) -> Fields) {
    let window = object_for(id);
    if window.is_null() {
        return;
    }
    let responder = responder_of(window);
    let sel = objc::sel::get(selector);
    // SAFETY: null or a live object this runtime allocated.
    let Some(method) = (unsafe { objc::object::class_of(responder) }).and_then(|c| c.method(sel))
    else {
        return;
    };
    let event = nsevent::make(fields(window));
    // SAFETY: every responder method here is `void (id, SEL, NSEvent *)`.
    unsafe {
        let f: unsafe extern "C" fn(Id, objc::sel::Sel, Id) =
            std::mem::transmute(method.implementation());
        f(responder, sel, event);
    }
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
fn post(id: u32, name: &str) {
    let object = object_for(id);
    if object.is_null() {
        return;
    }
    let delegate = window::delegate_of(object);
    if !delegate.is_null() {
        let selector = delegate_selector(name);
        let notification = foundation::notification::make(name, object, std::ptr::null_mut());
        // SAFETY: a window delegate method is
        // `void (id, SEL, NSNotification *)`.
        let sel = objc::sel::get(selector);
        // SAFETY: null or a live object this runtime allocated.
        if let Some(method) = unsafe { objc::object::class_of(delegate) }.and_then(|c| c.method(sel))
        {
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
