//! Turning an archived AppKit object into a live one.
//!
//! This is what `-initWithCoder:` would be if the objects in this crate adopted
//! `NSCoding`. They do not, deliberately: a coder is a general mechanism for
//! reading a class this runtime has never seen, and what is actually needed is
//! the four archived classes a nib is made of. A general decoder would have to
//! answer for all of them and would answer wrongly for the ones it did not know.
//!
//! So the shape is a match on the class name the archive recorded, and a name
//! that is not in it stays [`crate::nib`]'s placeholder - an object with no
//! methods that reports itself the first time a program uses it.
//!
//! Interface Builder writes geometry as *text*: `NSWindowRect` is the string
//! `{{300, 300}, {1000, 665}}` and `NSFrameSize` is `{800, 600}`. Parsing that
//! is most of this module, and a rectangle read wrongly is a window in the wrong
//! place rather than an error, so [`rect`] and [`size`] return `None` rather
//! than a default on anything they do not understand.

use foundation::geometry::{NSPoint, NSRect, NSSize};
use foundation::unarchiver::{Archive, Uid};
use objc::object::Id;
use objc::sel::Sel;

/// The numbers in a brace-delimited string, in order.
fn numbers(text: &str) -> Vec<f64> {
    text.split(|c: char| !(c.is_ascii_digit() || c == '.' || c == '-' || c == '+' || c == 'e'))
        .filter(|part| !part.is_empty())
        .filter_map(|part| part.parse::<f64>().ok())
        .collect()
}

/// `{width, height}`.
pub fn size(text: &str) -> Option<NSSize> {
    match numbers(text)[..] {
        [width, height] => Some(NSSize { width, height }),
        _ => None,
    }
}

/// `{{x, y}, {width, height}}`.
pub fn rect(text: &str) -> Option<NSRect> {
    match numbers(text)[..] {
        [x, y, width, height] => Some(NSRect {
            origin: NSPoint { x, y },
            size: NSSize { width, height },
        }),
        _ => None,
    }
}

/// Send `-init...` to a freshly allocated object of `name`, or `None` if
/// nothing implements that class.
///
/// Allocation goes through the runtime rather than through `+alloc` so that a
/// guest class with its own `+alloc` override is not asked to allocate before
/// its `-init` runs, which is the order a nib's decoder uses.
fn allocate(name: &str) -> Option<Id> {
    let class = std::ffi::CString::new(name)
        .ok()
        .and_then(|n| objc::class::lookup(&n))?;
    Some(objc::object::create_instance(class, 0))
}

/// `[object selector:rect styleMask:... ]`, through the real dispatch so a
/// guest's override runs.
///
/// # Safety
///
/// The selector must name a method with this prototype.
unsafe fn send_init_window(object: Id, rect: NSRect, style: usize) -> Id {
    let sel = objc::sel::get("initWithContentRect:styleMask:backing:defer:");
    // SAFETY: the caller's obligation. `NSBackingStoreBuffered` is 2, and the
    // deferred flag is false because SDL makes the window either way.
    unsafe {
        let f: unsafe extern "C" fn(Id, Sel, NSRect, usize, usize, u8) -> Id =
            std::mem::transmute(objc::msg::entry());
        f(object, sel, rect, style, 2, 0)
    }
}

/// # Safety
///
/// The selector must name a method taking one `NSRect` and returning an object.
unsafe fn send_init_frame(object: Id, frame: NSRect) -> Id {
    let sel = objc::sel::get("initWithFrame:");
    // SAFETY: the caller's obligation.
    unsafe {
        let f: unsafe extern "C" fn(Id, Sel, NSRect) -> Id =
            std::mem::transmute(objc::msg::entry());
        f(object, sel, frame)
    }
}

/// # Safety
///
/// The selector must name a method taking one object and returning nothing.
unsafe fn send_object(receiver: Id, selector: &str, argument: Id) {
    // SAFETY: the caller's obligation.
    unsafe {
        let f: unsafe extern "C" fn(Id, Sel, Id) = std::mem::transmute(objc::msg::entry());
        f(receiver, objc::sel::get(selector), argument);
    }
}

/// What a decoder needs from its caller: the archive, and a way to build the
/// objects an entry refers to.
pub(crate) trait Builder {
    fn archive(&self) -> &Archive;
    fn object_at(&mut self, uid: Uid) -> Option<Id>;
    /// Report a class the archive named that nothing implements.
    fn note_missing(&mut self, name: &str);
}

/// Build the object an archived entry stands for, or `None` for a class this
/// module does not decode.
pub(crate) fn build(builder: &mut dyn Builder, uid: Uid, class_name: &str) -> Option<Id> {
    match class_name {
        "NSWindowTemplate" => window_template(builder, uid),
        "NSCustomView" => custom_view(builder, uid),
        "NSView" => view(builder, uid, "NSView"),
        _ => None,
    }
}

/// The class names [`build`] decodes, for the report and for the tests.
pub(crate) const DECODED: &[&str] = &["NSWindowTemplate", "NSCustomView", "NSView"];

fn text<'a>(builder: &'a dyn Builder, uid: Uid, key: &str) -> Option<&'a str> {
    let value = builder.archive().field_object(uid, key)?;
    builder.archive().string(value)
}

fn window_template(builder: &mut dyn Builder, uid: Uid) -> Option<Id> {
    // `NSWindowClass` is the class the document asked for, which is
    // `NSWindow` unless the program subclassed it. A subclass nothing
    // implements falls back to `NSWindow` and is reported, because a window of
    // the wrong class still shows the right pixels where a placeholder shows
    // none.
    let wanted = text(builder, uid, "NSWindowClass")
        .unwrap_or("NSWindow")
        .to_string();
    let object = match allocate(&wanted) {
        Some(object) => object,
        None => {
            builder.note_missing(&wanted);
            allocate("NSWindow")?
        }
    };

    let frame = text(builder, uid, "NSWindowRect")
        .and_then(rect)
        .unwrap_or(NSRect {
            origin: NSPoint { x: 0.0, y: 0.0 },
            size: NSSize {
                width: 640.0,
                height: 480.0,
            },
        });
    let style = builder
        .archive()
        .field(uid, "NSWindowStyleMask")
        .and_then(foundation::plist::Value::as_i64)
        .unwrap_or(0) as usize;
    // SAFETY: the selector's prototype is the one stated at `send_init_window`.
    let window = unsafe { send_init_window(object, frame, style) };
    if window.is_null() {
        return None;
    }

    if let Some(title) = text(builder, uid, "NSWindowTitle") {
        let title = foundation::string::from_str(title);
        // SAFETY: `-setTitle:` takes one object.
        unsafe { send_object(window, "setTitle:", title) };
    }
    if let Some(view) = builder.archive().field_object(uid, "NSWindowView")
        && let Some(view) = builder.object_at(view)
    {
        // SAFETY: `-setContentView:` takes one object.
        unsafe { send_object(window, "setContentView:", view) };
    }
    Some(window)
}

fn custom_view(builder: &mut dyn Builder, uid: Uid) -> Option<Id> {
    let wanted = text(builder, uid, "NSClassName")
        .unwrap_or("NSView")
        .to_string();
    view(builder, uid, &wanted)
}

fn view(builder: &mut dyn Builder, uid: Uid, class_name: &str) -> Option<Id> {
    let object = match allocate(class_name) {
        Some(object) => object,
        None => {
            builder.note_missing(class_name);
            return None;
        }
    };
    // A view's own frame is its size at the origin: the archive records where a
    // view sits through its superview's subview list, which nothing here reads.
    let size = text(builder, uid, "NSFrameSize")
        .and_then(size)
        .or_else(|| text(builder, uid, "NSFrame").and_then(rect).map(|r| r.size))
        .unwrap_or_default();
    let frame = NSRect {
        origin: NSPoint::default(),
        size,
    };
    // SAFETY: the selector's prototype is the one stated at `send_init_frame`.
    let view = unsafe { send_init_frame(object, frame) };
    if view.is_null() {
        return None;
    }

    if builder
        .archive()
        .field(uid, "NSViewWantsBestResolutionOpenGLSurface")
        .and_then(foundation::plist::Value::as_i64)
        .unwrap_or(0)
        != 0
    {
        // SAFETY: `-setWantsBestResolutionOpenGLSurface:` takes one `BOOL`.
        unsafe {
            let f: unsafe extern "C" fn(Id, Sel, u8) = std::mem::transmute(objc::msg::entry());
            f(
                view,
                objc::sel::get("setWantsBestResolutionOpenGLSurface:"),
                1,
            );
        }
    }

    if let Some(subviews) = builder.archive().field_object(uid, "NSSubviews") {
        for entry in builder.archive().array(subviews) {
            if let Some(subview) = builder.object_at(entry) {
                // SAFETY: `-addSubview:` takes one object.
                unsafe { send_object(view, "addSubview:", subview) };
            }
        }
    }
    Some(view)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interface_builders_geometry_strings_parse() {
        assert_eq!(
            rect("{{300, 300}, {1000, 665}}"),
            Some(NSRect {
                origin: NSPoint { x: 300.0, y: 300.0 },
                size: NSSize {
                    width: 1000.0,
                    height: 665.0
                },
            })
        );
        assert_eq!(
            size("{800, 600}"),
            Some(NSSize {
                width: 800.0,
                height: 600.0
            })
        );
        // A negative origin is ordinary - a window on a display left of the
        // primary one has one.
        assert_eq!(
            rect("{{-100, 0}, {10, 20}}").map(|r| r.origin.x),
            Some(-100.0)
        );
    }

    /// Anything that is not the shape asked for is `None`, not a default: a
    /// zero rectangle would be a window at the origin with no size, which looks
    /// like a window the program asked for.
    #[test]
    fn a_string_of_the_wrong_shape_is_not_a_rectangle() {
        assert_eq!(rect("{800, 600}"), None);
        assert_eq!(rect(""), None);
        assert_eq!(size("{{0, 0}, {1, 1}}"), None);
    }
}
