//! `NSEvent`, as the fields a responder method reads out of one.
//!
//! Not a queue. AppKit's `NSEvent` is also the thing `-nextEventMatchingMask:`
//! hands back, and this one is only ever built by [`crate::event`] on its way to
//! `-keyDown:` or `-mouseDown:` - because that is how a program of this shape
//! receives input, and a queue nothing dequeues from would be a second, silent
//! path for the same events.
//!
//! `-locationInWindow` is in the window's coordinates, which is AppKit's
//! bottom-left origin and not SDL's top-left one. The conversion is
//! [`crate::geometry`]'s, in the one place it belongs.

use std::ffi::c_void;

use foundation::geometry::{CGFloat, NSPoint};
use objc::object::Id;
use objc::sel::Sel;

use crate::define::methods;

/// `NSEventType`, the values a guest compares `-type` against.
pub mod kind {
    pub const LEFT_MOUSE_DOWN: u64 = 1;
    pub const LEFT_MOUSE_UP: u64 = 2;
    pub const RIGHT_MOUSE_DOWN: u64 = 3;
    pub const RIGHT_MOUSE_UP: u64 = 4;
    pub const MOUSE_MOVED: u64 = 5;
    pub const LEFT_MOUSE_DRAGGED: u64 = 6;
    pub const RIGHT_MOUSE_DRAGGED: u64 = 7;
    pub const MOUSE_ENTERED: u64 = 8;
    pub const MOUSE_EXITED: u64 = 9;
    pub const KEY_DOWN: u64 = 10;
    pub const KEY_UP: u64 = 11;
    pub const FLAGS_CHANGED: u64 = 12;
    pub const SCROLL_WHEEL: u64 = 22;
    pub const OTHER_MOUSE_DOWN: u64 = 25;
    pub const OTHER_MOUSE_UP: u64 = 26;
    pub const OTHER_MOUSE_DRAGGED: u64 = 27;
}

#[repr(C)]
struct Event {
    isa: *const c_void,
    kind: u64,
    modifiers: u64,
    location: NSPoint,
    delta: NSPoint,
    /// The window the event is in, unretained: an event lives for one dispatch
    /// and the window that made it outlives that.
    window: Id,
    /// `-characters`, kept as an object so `-length` and `-characterAtIndex:`
    /// answer without this module knowing what a string is.
    characters: Id,
    key_code: u16,
    button: i64,
    clicks: i64,
    repeat: bool,
}

pub const INSTANCE_SIZE: usize = size_of::<Event>();

/// The event a receiver is, or `None` for anything else.
///
/// # Safety
///
/// `event` must be null or an object this runtime allocated.
unsafe fn event_of<'a>(event: Id) -> Option<&'a Event> {
    // SAFETY: the caller's obligation.
    let is_event = unsafe { objc::object::class_of(event) }.is_some_and(|c| {
        objc::class::lookup(c"NSEvent").is_some_and(|k| c.chain().any(|x| std::ptr::eq(x, k)))
    });
    // SAFETY: the check above says the storage is this module's.
    is_event.then(|| unsafe { &*(event as *const Event) })
}

/// Read one field, or its zero value for a receiver that is not an event.
///
/// A macro because the alternative is twelve near-identical five-line functions,
/// which is the case where a near-copy hides a wrong field.
macro_rules! reader {
    ($($name:ident -> $ty:ty { $read:expr } ),* $(,)?) => {
        $(
            unsafe extern "C" fn $name(receiver: Id, _sel: Sel) -> $ty {
                // SAFETY: the caller sends to a live object.
                let read: fn(&Event) -> $ty = $read;
                unsafe { event_of(receiver) }.map_or(<$ty>::default(), read)
            }
        )*
    };
}

reader! {
    kind -> u64 { |e| e.kind },
    modifier_flags -> u64 { |e| e.modifiers },
    location_in_window -> NSPoint { |e| e.location },
    key_code -> u16 { |e| e.key_code },
    button_number -> i64 { |e| e.button },
    click_count -> i64 { |e| e.clicks },
    delta_x -> CGFloat { |e| e.delta.x },
    delta_y -> CGFloat { |e| e.delta.y },
    characters -> Id { |e| e.characters },
    window -> Id { |e| e.window },
    is_a_repeat -> u8 { |e| e.repeat as u8 },
}

/// `-timestamp`: seconds since this process started, which is what the same
/// method means on Apple's - the value is only ever differenced.
unsafe extern "C" fn timestamp(_receiver: Id, _sel: Sel) -> CGFloat {
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    START.get_or_init(std::time::Instant::now).elapsed().as_secs_f64()
}

/// `+[NSEvent modifierFlags]`, which asks about the keyboard *now* rather than
/// about an event. SDL is the only thing here that knows.
unsafe extern "C" fn current_modifier_flags(_class: Id, _sel: Sel) -> u64 {
    crate::keymap::modifiers(sdl3::video::keyboard_modifiers())
}

/// What an event is made of, named rather than positional: eleven fields of
/// which most are zero for any one event, and a builder is how a caller says
/// which.
#[derive(Default)]
pub struct Fields {
    pub kind: u64,
    pub modifiers: u64,
    pub location: NSPoint,
    pub delta: NSPoint,
    pub window: Id,
    pub characters: Id,
    pub key_code: u16,
    pub button: i64,
    pub clicks: i64,
    pub repeat: bool,
}

/// Build an autoreleased event.
///
/// Autoreleased because it is handed to one responder method and never stored:
/// a guest that keeps it retains it, and the pool the pump runs under is what
/// frees the rest.
pub fn make(fields: Fields) -> Id {
    let class = objc::class::lookup(c"NSEvent").expect("declared before its methods");
    // SAFETY: `Event` is the layout the class was registered with.
    let event = unsafe { corefoundation::base::allocate::<Event>(class) };
    // SAFETY: freshly allocated; every field is written before it is read.
    unsafe {
        (*event).kind = fields.kind;
        (*event).modifiers = fields.modifiers;
        (*event).location = fields.location;
        (*event).delta = fields.delta;
        (*event).window = fields.window;
        (*event).characters = fields.characters;
        (*event).key_code = fields.key_code;
        (*event).button = fields.button;
        (*event).clicks = fields.clicks;
        (*event).repeat = fields.repeat;
    }
    objc::arc::autorelease(event as Id)
}

pub(crate) fn install() {
    let class = objc::class::lookup(c"NSEvent").expect("declared before its methods");
    crate::define::class_methods!(class, {
        "modifierFlags" => current_modifier_flags, c"Q16@0:8";
    });
    methods!(class, {
        "type" => kind, c"Q16@0:8";
        "modifierFlags" => modifier_flags, c"Q16@0:8";
        "locationInWindow" => location_in_window, c"{CGPoint=dd}16@0:8";
        "keyCode" => key_code, c"S16@0:8";
        "buttonNumber" => button_number, c"q16@0:8";
        "clickCount" => click_count, c"q16@0:8";
        "deltaX" => delta_x, c"d16@0:8";
        "deltaY" => delta_y, c"d16@0:8";
        "scrollingDeltaX" => delta_x, c"d16@0:8";
        "scrollingDeltaY" => delta_y, c"d16@0:8";
        "characters" => characters, c"@16@0:8";
        "charactersIgnoringModifiers" => characters, c"@16@0:8";
        "window" => window, c"@16@0:8";
        "isARepeat" => is_a_repeat, c"c16@0:8";
        "timestamp" => timestamp, c"d16@0:8";
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_event_reports_what_it_was_built_with() {
        crate::bootstrap();
        let event = make(Fields {
            kind: kind::KEY_DOWN,
            key_code: 0x7E,
            modifiers: crate::keymap::flags::SHIFT,
            location: NSPoint { x: 3.0, y: 4.0 },
            ..Fields::default()
        });
        // SAFETY: each prototype is the one its selector declares.
        unsafe {
            assert_eq!(super::kind(event, objc::sel::get("type")), kind::KEY_DOWN);
            assert_eq!(super::key_code(event, objc::sel::get("keyCode")), 0x7E);
            assert_eq!(
                super::location_in_window(event, objc::sel::get("locationInWindow")),
                NSPoint { x: 3.0, y: 4.0 }
            );
        }
    }

    /// A receiver that is not an event answers zero rather than reading a field
    /// out of whatever it is.
    #[test]
    fn a_non_event_reads_as_zero() {
        crate::bootstrap();
        let string = foundation::string::from_str("not an event");
        // SAFETY: `-type` is `NSUInteger (id, SEL)`.
        assert_eq!(unsafe { super::kind(string, objc::sel::get("type")) }, 0);
    }
}
