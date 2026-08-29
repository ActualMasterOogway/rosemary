use objc::object::Id;

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
        // The input events, which have nowhere to go yet. Dropped rather than
        // turned into an `NSEvent`: see the module note.
        Event::Key { .. }
        | Event::Text { .. }
        | Event::MouseMotion { .. }
        | Event::MouseButton { .. }
        | Event::MouseWheel { .. }
        | Event::Other(_) => {}
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
