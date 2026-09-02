//! `NSCursor` and `NSTrackingArea`.
//!
//! Both are here because both are about the pointer, and both are almost all
//! bookkeeping. SDL has one cursor for the process and no notion of a tracking
//! region, so what a program can actually change is whether the pointer is
//! visible - and that is the one thing a full-screen game does change.
//!
//! A tracking area therefore remembers what it was made with and does nothing
//! else. It has to exist as an object, because a view holds one and compares it
//! against the one it removes; making it a placeholder instead would refuse
//! `-removeTrackingArea:` on a program that had already got its area added.

use std::ffi::c_void;

use foundation::geometry::{NSPoint, NSRect};
use objc::object::Id;
use objc::sel::Sel;

use crate::define::{class_methods, methods};

#[repr(C)]
pub struct TrackingArea {
    isa: *const c_void,
    rect: NSRect,
    options: usize,
    owner: Id,
    user_info: Id,
}

pub const TRACKING_AREA_INSTANCE_SIZE: usize = size_of::<TrackingArea>();

#[repr(C)]
pub struct Cursor {
    isa: *const c_void,
    /// Which of the named cursors this is, for `-set` to act on. SDL has no
    /// shape to switch to, so the value is kept and not used.
    shape: usize,
    /// The image a custom cursor was made from, retained, and where its point
    /// is. Kept for the same reason the shape is: a program reads them back.
    image: Id,
    hot_spot: foundation::geometry::NSPoint,
}

pub const CURSOR_INSTANCE_SIZE: usize = size_of::<Cursor>();

unsafe extern "C" fn init_tracking_area(
    receiver: Id,
    _sel: Sel,
    rect: NSRect,
    options: usize,
    owner: Id,
    user_info: Id,
) -> Id {
    // SAFETY: a tracking area of this crate's.
    let area = unsafe { &mut *(receiver as *mut TrackingArea) };
    area.rect = rect;
    area.options = options;
    // The owner is unretained, as AppKit's is: a view owns its tracking areas
    // and is usually the owner itself, so retaining would be a cycle.
    area.owner = owner;
    area.user_info = objc::arc::retain(user_info);
    receiver
}

unsafe extern "C" fn tracking_rect(receiver: Id, _sel: Sel) -> NSRect {
    // SAFETY: a tracking area of this crate's.
    unsafe { (*(receiver as *const TrackingArea)).rect }
}

unsafe extern "C" fn tracking_options(receiver: Id, _sel: Sel) -> usize {
    // SAFETY: a tracking area of this crate's.
    unsafe { (*(receiver as *const TrackingArea)).options }
}

unsafe extern "C" fn tracking_owner(receiver: Id, _sel: Sel) -> Id {
    // SAFETY: a tracking area of this crate's.
    unsafe { (*(receiver as *const TrackingArea)).owner }
}

unsafe extern "C" fn tracking_user_info(receiver: Id, _sel: Sel) -> Id {
    // SAFETY: a tracking area of this crate's.
    unsafe { (*(receiver as *const TrackingArea)).user_info }
}

/// `-initWithImage:hotSpot:`.
///
/// The image is kept and the pointer keeps whatever shape it had: this tree
/// changes only whether the pointer is *visible*, which is the module note
/// above and applies to a custom cursor exactly as it does to `+IBeamCursor`.
unsafe extern "C" fn init_with_image(receiver: Id, _sel: Sel, image: Id, hot_spot: NSPoint) -> Id {
    // SAFETY: a cursor of this crate's.
    let cursor = unsafe { &mut *(receiver as *mut Cursor) };
    cursor.image = objc::arc::retain(image);
    cursor.hot_spot = hot_spot;
    receiver
}

unsafe extern "C" fn image(receiver: Id, _sel: Sel) -> Id {
    // SAFETY: as above.
    unsafe { (*(receiver as *const Cursor)).image }
}

unsafe extern "C" fn hot_spot(receiver: Id, _sel: Sel) -> NSPoint {
    // SAFETY: as above.
    unsafe { (*(receiver as *const Cursor)).hot_spot }
}

/// The named cursors, made once each.
///
/// The shape is the index in this list. Every one is the same pointer as far as
/// SDL is concerned, and they are distinct objects because a program compares
/// `[NSCursor currentCursor]` against the one it set.
static SHAPES: &[&str] = &[
    "arrowCursor",
    "IBeamCursor",
    "pointingHandCursor",
    "crosshairCursor",
    "closedHandCursor",
    "openHandCursor",
    "resizeLeftRightCursor",
    "resizeUpDownCursor",
];

fn cursor_for(shape: usize) -> Id {
    static CURSORS: std::sync::Mutex<Vec<(usize, usize)>> = std::sync::Mutex::new(Vec::new());
    let mut cursors = CURSORS.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((_, object)) = cursors.iter().find(|(s, _)| *s == shape) {
        return *object as Id;
    }
    let class = objc::class::lookup(c"NSCursor").expect("declared before its methods");
    // SAFETY: `Cursor` is the layout the class was registered with.
    let cursor = unsafe { corefoundation::base::allocate::<Cursor>(class) };
    // SAFETY: freshly allocated.
    unsafe { (*cursor).shape = shape };
    // Retained forever: `+arrowCursor` hands one out unowned.
    objc::arc::retain(cursor as Id);
    cursors.push((shape, cursor as usize));
    cursor as Id
}

unsafe extern "C" fn named_cursor(_class: Id, sel: Sel) -> Id {
    // SAFETY: a registered selector is a NUL-terminated interned name.
    let name = unsafe { std::ffi::CStr::from_ptr(sel) };
    let name = name.to_string_lossy();
    match SHAPES.iter().position(|s| *s == name) {
        Some(shape) => cursor_for(shape),
        // Unreachable through dispatch: the selector came from this table.
        None => objc::fail!("+[NSCursor {name}]: not one of this runtime's cursors"),
    }
}

/// The cursor a program last set, which `+currentCursor` answers.
static CURRENT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

unsafe extern "C" fn set(receiver: Id, _sel: Sel) {
    CURRENT.store(receiver as usize, std::sync::atomic::Ordering::Release);
}

unsafe extern "C" fn current_cursor(_class: Id, _sel: Sel) -> Id {
    match CURRENT.load(std::sync::atomic::Ordering::Acquire) {
        0 => cursor_for(0),
        object => object as Id,
    }
}

/// `+hide` and `+unhide`, which SDL does have.
///
/// AppKit's are *counted* - two hides need two unhides - and SDL's are not, so
/// the count is kept here. A program that hides twice and unhides once expects
/// the pointer to stay hidden, and a game hiding the cursor per frame would
/// otherwise show it again on the first unhide.
static HIDDEN: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);

unsafe extern "C" fn hide(_class: Id, _sel: Sel) {
    if HIDDEN.fetch_add(1, std::sync::atomic::Ordering::AcqRel) == 0 {
        sdl3::video::set_cursor_visible(false);
    }
}

unsafe extern "C" fn unhide(_class: Id, _sel: Sel) {
    let previous = HIDDEN.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
    if previous <= 0 {
        // AppKit clamps at zero rather than letting the count go negative, or a
        // stray unhide would take an extra hide to undo.
        HIDDEN.store(0, std::sync::atomic::Ordering::Release);
        return;
    }
    if previous == 1 {
        sdl3::video::set_cursor_visible(true);
    }
}

unsafe extern "C" fn set_hidden_until_mouse_moves(class: Id, sel: Sel, hidden: u8) {
    // The "until it moves" half needs the event pump, which is M2's next piece;
    // until then this is the hide it asks for, which is the visible effect.
    match hidden != 0 {
        // SAFETY: both are `void (Class, SEL)`.
        true => unsafe { hide(class, sel) },
        false => unsafe { unhide(class, sel) },
    }
}

pub(crate) fn install() {
    let area = objc::class::lookup(c"NSTrackingArea").expect("declared before its methods");
    methods!(area, {
        "initWithRect:options:owner:userInfo:" => init_tracking_area,
            c"@64@0:8{CGRect={CGPoint=dd}{CGSize=dd}}16Q48@56@64";
        "rect" => tracking_rect, c"{CGRect={CGPoint=dd}{CGSize=dd}}16@0:8";
        "options" => tracking_options, c"Q16@0:8";
        "owner" => tracking_owner, c"@16@0:8";
        "userInfo" => tracking_user_info, c"@16@0:8";
    });

    let cursor = objc::class::lookup(c"NSCursor").expect("declared before its methods");
    let meta = cursor.metaclass();
    for name in SHAPES {
        meta.replace_method(
            objc::sel::get(name),
            named_cursor as *const c_void,
            c"@16@0:8",
        );
    }
    class_methods!(cursor, {
        "currentCursor" => current_cursor, c"@16@0:8";
        "hide" => hide, c"v16@0:8";
        "unhide" => unhide, c"v16@0:8";
        "setHiddenUntilMouseMoves:" => set_hidden_until_mouse_moves, c"v20@0:8c16";
    });
    methods!(cursor, {
        "initWithImage:hotSpot:" => init_with_image, c"@40@0:8@16{CGPoint=dd}24";
        "image" => image, c"@16@0:8";
        "hotSpot" => hot_spot, c"{CGPoint=dd}16@0:8";
        "set" => set, c"v16@0:8";
        "push" => set, c"v16@0:8";
        "pop" => set, c"v16@0:8";
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The hide count is what makes a program that hides twice stay hidden.
    #[test]
    fn hiding_is_counted() {
        crate::bootstrap();
        let class = objc::class::ptr(objc::class::lookup(c"NSCursor").expect("NSCursor")) as Id;
        HIDDEN.store(0, std::sync::atomic::Ordering::Release);
        // SAFETY: all three are `void (Class, SEL)`.
        unsafe {
            hide(class, objc::sel::get("hide"));
            hide(class, objc::sel::get("hide"));
            unhide(class, objc::sel::get("unhide"));
        }
        assert_eq!(HIDDEN.load(std::sync::atomic::Ordering::Acquire), 1);
        // SAFETY: as above.
        unsafe { unhide(class, objc::sel::get("unhide")) };
        assert_eq!(HIDDEN.load(std::sync::atomic::Ordering::Acquire), 0);
        // And a stray unhide does not go negative.
        // SAFETY: as above.
        unsafe { unhide(class, objc::sel::get("unhide")) };
        assert_eq!(HIDDEN.load(std::sync::atomic::Ordering::Acquire), 0);
    }

    /// Each named cursor is its own object, which is what a program comparing
    /// against `+currentCursor` depends on.
    #[test]
    fn each_named_cursor_is_one_object() {
        crate::bootstrap();
        let class = objc::class::ptr(objc::class::lookup(c"NSCursor").expect("NSCursor")) as Id;
        // SAFETY: `+namedCursor` is `id (Class, SEL)`.
        let arrow = unsafe { named_cursor(class, objc::sel::get("arrowCursor")) };
        // SAFETY: as above.
        let again = unsafe { named_cursor(class, objc::sel::get("arrowCursor")) };
        // SAFETY: as above.
        let beam = unsafe { named_cursor(class, objc::sel::get("IBeamCursor")) };
        assert_eq!(arrow, again);
        assert_ne!(arrow, beam);
    }
}
