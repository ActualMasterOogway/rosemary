//! `NSScreen`, over SDL's displays.
//!
//! A screen object is one display id and nothing else: everything it answers is
//! asked of SDL when it is asked, because a display can be resized, rearranged
//! or unplugged between two questions and a cached rectangle would then be a
//! rectangle for a screen that no longer has it.

use std::ffi::c_void;

use foundation::geometry::{CGFloat, NSRect};
use objc::object::Id;
use objc::sel::Sel;

use crate::define::{class_methods, methods};

#[repr(C)]
pub struct Screen {
    isa: *const c_void,
    display: usize,
}

pub const INSTANCE_SIZE: usize = size_of::<Screen>();

/// The screen object for a display, made once per display and kept.
///
/// `+mainScreen` is compared against `-[NSWindow screen]` by programs deciding
/// whether they are on the display they meant to be, so two objects for one
/// display would make that comparison false on a single-monitor machine.
fn screen_for(display: u32) -> Id {
    static SCREENS: std::sync::Mutex<Vec<(u32, usize)>> = std::sync::Mutex::new(Vec::new());
    let mut screens = SCREENS.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((_, object)) = screens.iter().find(|(d, _)| *d == display) {
        return *object as Id;
    }
    let class = objc::class::lookup(c"NSScreen").expect("declared before its methods");
    // SAFETY: `Screen` is the layout the class was registered with.
    let screen = unsafe { corefoundation::base::allocate::<Screen>(class) };
    // SAFETY: freshly allocated.
    unsafe { (*screen).display = display as usize };
    // Retained forever: a screen a guest can release is one it can make dangle,
    // and these are handed out unowned by `+mainScreen`.
    objc::arc::retain(screen as Id);
    screens.push((display, screen as usize));
    screen as Id
}

fn display_of(receiver: Id) -> u32 {
    // SAFETY: a screen of this crate's.
    unsafe { (*(receiver as *const Screen)).display as u32 }
}

unsafe extern "C" fn main_screen(_class: Id, _sel: Sel) -> Id {
    match sdl3::video::displays().first() {
        Some(display) => screen_for(*display),
        // No display is not an error: a headless host has none, and a program
        // that asks gets the nil AppKit gives it rather than an invented screen.
        None => std::ptr::null_mut(),
    }
}

unsafe extern "C" fn deepest_screen(class: Id, sel: Sel) -> Id {
    // SAFETY: `+mainScreen` is `id (Class, SEL)`.
    unsafe { main_screen(class, sel) }
}

unsafe extern "C" fn screens(_class: Id, _sel: Sel) -> Id {
    let objects: Vec<corefoundation::base::CFTypeRef> = sdl3::video::displays()
        .into_iter()
        .map(|d| screen_for(d) as corefoundation::base::CFTypeRef)
        .collect();
    let array = corefoundation::collections::new_array(
        &objects,
        corefoundation::callbacks::Policy::from_type_callbacks(),
        false,
    );
    // SAFETY: a fresh array.
    unsafe { corefoundation::api::CFAutorelease(array) as Id }
}

unsafe extern "C" fn frame(receiver: Id, _sel: Sel) -> NSRect {
    let display = display_of(receiver);
    match sdl3::video::display_bounds(display) {
        Some(b) => crate::geometry::rect_from_sdl(b.x, b.y, b.w, b.h),
        None => NSRect::default(),
    }
}

/// `-visibleFrame`: the part not under the menu bar or the Dock.
///
/// SDL's usable bounds is the same question, and where it cannot answer the
/// whole frame is the honest fallback - a program using it to place a window
/// gets one that may sit under the menu bar, rather than one placed against a
/// margin this runtime invented.
unsafe extern "C" fn visible_frame(receiver: Id, sel: Sel) -> NSRect {
    let display = display_of(receiver);
    match sdl3::video::display_usable_bounds(display) {
        Some(b) => crate::geometry::rect_from_sdl(b.x, b.y, b.w, b.h),
        // SAFETY: `-frame` is `NSRect (id, SEL)`.
        None => unsafe { frame(receiver, sel) },
    }
}

unsafe extern "C" fn backing_scale_factor(receiver: Id, _sel: Sel) -> CGFloat {
    sdl3::video::display_content_scale(display_of(receiver)) as f64
}

/// `-deviceDescription`, which a program reads `NSScreenNumber` out of to get a
/// `CGDirectDisplayID` for `CVDisplayLink`.
unsafe extern "C" fn device_description(receiver: Id, _sel: Sel) -> Id {
    let display = display_of(receiver);
    let key = foundation::string::from_str("NSScreenNumber") as corefoundation::base::CFTypeRef;
    let number = corefoundation::collections::new_number(
        corefoundation::collections::Number::Integer(display as i64),
    );
    let dictionary = corefoundation::collections::new_dictionary(
        &[(key, number)],
        corefoundation::callbacks::Policy::from_key_callbacks(),
        corefoundation::callbacks::Policy::from_type_callbacks(),
        false,
    );
    // SAFETY: a fresh dictionary.
    unsafe { corefoundation::api::CFAutorelease(dictionary) as Id }
}

/// `CGDirectDisplayID CGMainDisplayID(void)`.
///
/// CoreGraphics is below AppKit and this is its only entry point here, which is
/// the argument for it living in this file rather than a crate of its own: what
/// a `CGDirectDisplayID` *is* in this tree is decided by [`device_description`],
/// which publishes an SDL display id under `NSScreenNumber` for `CVDisplayLink`
/// to open. Two definitions of that would be two display numbering schemes.
#[allow(non_snake_case)]
pub unsafe extern "C" fn CGMainDisplayID() -> u32 {
    sdl3::video::primary_display()
}

/// The display id a screen stands for, for `CVDisplayLink` and its relatives.
///
/// # Safety
///
/// `screen` must be null or a screen this crate made.
pub unsafe fn display_id(screen: Id) -> Option<u32> {
    if screen.is_null() {
        return None;
    }
    // SAFETY: the caller's obligation.
    let is_screen = unsafe { objc::object::class_of(screen) }.is_some_and(|c| {
        objc::class::lookup(c"NSScreen").is_some_and(|k| std::ptr::eq(c, k))
    });
    is_screen.then(|| display_of(screen))
}

pub(crate) fn install() {
    let class = objc::class::lookup(c"NSScreen").expect("declared before its methods");
    class_methods!(class, {
        "mainScreen" => main_screen, c"@16@0:8";
        "deepestScreen" => deepest_screen, c"@16@0:8";
        "screens" => screens, c"@16@0:8";
    });
    methods!(class, {
        "frame" => frame, c"{CGRect={CGPoint=dd}{CGSize=dd}}16@0:8";
        "visibleFrame" => visible_frame, c"{CGRect={CGPoint=dd}{CGSize=dd}}16@0:8";
        "backingScaleFactor" => backing_scale_factor, c"d16@0:8";
        "deviceDescription" => device_description, c"@16@0:8";
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One object per display, which is what makes `+mainScreen ==
    /// [window screen]` mean anything.
    #[test]
    fn a_display_has_one_screen_object() {
        crate::bootstrap();
        let Some(display) = sdl3::video::displays().first().copied() else {
            eprintln!("skipped: this host has no display");
            return;
        };
        assert_eq!(screen_for(display), screen_for(display));
    }

    /// And a screen's frame has the display's size, whatever the origin
    /// conversion did to it.
    #[test]
    fn a_screens_frame_is_its_displays_size() {
        crate::bootstrap();
        let Some(display) = sdl3::video::displays().first().copied() else {
            eprintln!("skipped: this host has no display");
            return;
        };
        let bounds = sdl3::video::display_bounds(display).expect("a display has bounds");
        let screen = screen_for(display);
        // SAFETY: `-frame` is `NSRect (id, SEL)`.
        let rect = unsafe { frame(screen, objc::sel::get("frame")) };
        assert_eq!(rect.size.width, bounds.w as f64);
        assert_eq!(rect.size.height, bounds.h as f64);
    }
}
