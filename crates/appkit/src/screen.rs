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

/// `-maximumFramesPerSecond`: how often the display can present, which a game
/// uses to pick a frame cap. Sixty where SDL has no mode to report, because the
/// answer is a divisor a caller will use either way.
unsafe extern "C" fn maximum_frames_per_second(receiver: Id, _sel: Sel) -> isize {
    match sdl3::video::display_refresh_rate(display_of(receiver)) {
        Some(rate) => rate.round() as isize,
        None => 60,
    }
}

unsafe extern "C" fn backing_scale_factor(receiver: Id, _sel: Sel) -> CGFloat {
    sdl3::video::display_content_scale(display_of(receiver)) as f64
}

/// `-deviceDescription`: `NSScreenNumber` for a `CGDirectDisplayID`, and
/// `NSDeviceSize`/`NSDeviceResolution` for anything measuring the display.
///
/// The two size keys are not decoration. A program that wants its window in
/// *physical* units has to divide pixels by dots-per-inch, and this dictionary
/// is where both numbers come from - Roblox reads `NSDeviceSize` by name, in
/// `-updateWindowSizeMM:`. A missing key is `nil`, `-[nil sizeValue]` is
/// `{0, 0}`, and a caller dividing by that keeps whatever size it already had:
/// the failure is a window measurement that never updates, with nothing
/// anywhere reporting a missing key.
///
/// `NSDeviceResolution` is dots per *inch* and is 72 for a display at unit
/// scale, because AppKit's user-space unit is 1/72 inch by definition. It is
/// scaled by the backing factor, which is what makes a Retina screen report 144
/// and is the only place the scale enters the arithmetic.
unsafe extern "C" fn device_description(receiver: Id, _sel: Sel) -> Id {
    use foundation::geometry::NSSize;

    let display = display_of(receiver);
    // SAFETY: a screen of this crate's, which is what this method is sent to.
    let frame = unsafe { frame(receiver, objc::sel::get("frame")) };
    let scale = sdl3::video::display_content_scale(display) as f64;
    // A scale of zero would make the resolution zero, which is the value this
    // whole entry exists to keep a caller from dividing by.
    let scale = match scale > 0.0 {
        true => scale,
        false => 1.0,
    };
    // Every value is autoreleased, so this function owns none of them and the
    // dictionary's retain is the only one that matters. Mixing an owned value
    // in with autoreleased ones is how a leak or a double release gets written.
    let number = |value: i64| {
        // SAFETY: a fresh number this closure owns a reference to.
        unsafe {
            corefoundation::api::CFAutorelease(corefoundation::collections::new_number(
                corefoundation::collections::Number::Integer(value),
            ))
        }
    };
    let entries: [(&str, corefoundation::base::CFTypeRef); 4] = [
        ("NSScreenNumber", number(display as i64)),
        (
            // Pixels, which on a scaled display is the frame times the scale.
            "NSDeviceSize",
            foundation::geometry::boxed_size(NSSize {
                width: frame.size.width * scale,
                height: frame.size.height * scale,
            }) as corefoundation::base::CFTypeRef,
        ),
        (
            "NSDeviceResolution",
            foundation::geometry::boxed_size(NSSize {
                width: POINTS_PER_INCH * scale,
                height: POINTS_PER_INCH * scale,
            }) as corefoundation::base::CFTypeRef,
        ),
        ("NSDeviceBitsPerSample", number(8)),
    ];
    let pairs: Vec<(corefoundation::base::CFTypeRef, corefoundation::base::CFTypeRef)> = entries
        .iter()
        .map(|(key, value)| {
            (
                foundation::string::from_str(key) as corefoundation::base::CFTypeRef,
                *value,
            )
        })
        .collect();
    let dictionary = corefoundation::collections::new_dictionary(
        &pairs,
        corefoundation::callbacks::Policy::from_key_callbacks(),
        corefoundation::callbacks::Policy::from_type_callbacks(),
        false,
    );
    // The dictionary retained each key; the boxed values are autoreleased
    // already, so only this function's own references to the keys are spent.
    for (key, _) in &pairs {
        // SAFETY: a string this function made.
        unsafe { corefoundation::api::CFRelease(*key) };
    }
    // SAFETY: a fresh dictionary.
    unsafe { corefoundation::api::CFAutorelease(dictionary) as Id }
}

/// AppKit's user-space unit is 1/72 inch, so an unscaled display is 72 dpi by
/// definition rather than by measurement.
const POINTS_PER_INCH: f64 = 72.0;

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

/// `CGRect CGDisplayBounds(CGDirectDisplayID)`.
///
/// CoreGraphics measures the global display space from the **top left** with y
/// increasing downwards, which is SDL's convention and not `-[NSScreen frame]`'s
/// - so this is not [`frame`] with a different receiver, and converting it as
/// though it were puts a second display on the wrong side of the first.
#[allow(non_snake_case)]
pub unsafe extern "C" fn CGDisplayBounds(display: u32) -> NSRect {
    match sdl3::video::display_bounds(display) {
        Some(b) => NSRect {
            origin: foundation::geometry::NSPoint {
                x: b.x as f64,
                y: b.y as f64,
            },
            size: foundation::geometry::NSSize {
                width: b.w as f64,
                height: b.h as f64,
            },
        },
        // What CoreGraphics answers for a display id it does not know.
        None => NSRect::default(),
    }
}

/// `kCGErrorSuccess`.
const CG_SUCCESS: i32 = 0;
/// `kCGErrorIllegalArgument`.
const CG_ILLEGAL_ARGUMENT: i32 = 1001;

/// `CGError CGGetOnlineDisplayList(uint32_t, CGDirectDisplayID *, uint32_t *)`
///
/// Also `CGGetActiveDisplayList`. The two differ on a Mac only in whether a
/// display mirroring another is listed, and nothing here mirrors.
///
/// A null list with a count is the enquiry form, which is how a caller sizes
/// the array it is about to pass - so it succeeds rather than failing.
///
/// # Safety
///
/// `list` is null or writable for `max` display ids, and `count` is null or a
/// writable `uint32_t`.
#[allow(non_snake_case)]
pub unsafe extern "C" fn CGGetOnlineDisplayList(
    max: u32,
    list: *mut u32,
    count: *mut u32,
) -> i32 {
    let displays = sdl3::video::displays();
    if list.is_null() {
        if count.is_null() {
            return CG_ILLEGAL_ARGUMENT;
        }
        // SAFETY: the caller states it is writable.
        unsafe { count.write(displays.len() as u32) };
        return CG_SUCCESS;
    }
    let room = (max as usize).min(displays.len());
    // SAFETY: the caller states `list` holds `max` ids.
    unsafe { std::ptr::copy_nonoverlapping(displays.as_ptr(), list, room) };
    if !count.is_null() {
        // The number *written*, which is what a caller iterates over. The total
        // would walk past the end of its own array.
        // SAFETY: the caller states it is writable.
        unsafe { count.write(room as u32) };
    }
    CG_SUCCESS
}

/// Points per inch assumed for a display whose physical size nothing here can
/// measure.
///
/// SDL reports a display's size in points and its scale factor, and neither
/// says how large the panel is. Zero is what CoreGraphics answers for a display
/// id it does not know, and it cannot be the answer here: a caller works its
/// display density out as pixels over inches, and a zero denominator is an
/// infinite density rather than a missing one. So a conventional desktop
/// density is assumed and said so.
const ASSUMED_DPI: f64 = 96.0;

/// `CGSize CGDisplayScreenSize(CGDirectDisplayID)` - the panel's size in
/// millimetres.
#[allow(non_snake_case)]
pub unsafe extern "C" fn CGDisplayScreenSize(display: u32) -> foundation::geometry::NSSize {
    const MM_PER_INCH: f64 = 25.4;
    match sdl3::video::display_bounds(display) {
        Some(b) => foundation::geometry::NSSize {
            width: b.w as f64 / ASSUMED_DPI * MM_PER_INCH,
            height: b.h as f64 / ASSUMED_DPI * MM_PER_INCH,
        },
        None => foundation::geometry::NSSize::default(),
    }
}

/// `CGOpenGLDisplayMask CGDisplayIDToOpenGLDisplayMask(CGDirectDisplayID)`.
///
/// One bit per display, which is what a pixel-format attribute list carries.
/// The mask is only ever handed back to a CGL call this tree answers, so the
/// numbering is this tree's own; a display past the thirty-second gets no bit,
/// as it does on a Mac.
#[allow(non_snake_case)]
pub unsafe extern "C" fn CGDisplayIDToOpenGLDisplayMask(display: u32) -> u32 {
    sdl3::video::displays()
        .iter()
        .position(|d| *d == display)
        .filter(|index| *index < 32)
        .map_or(0, |index| 1u32 << index)
}

/// `io_service_t CGDisplayIOServicePort(CGDirectDisplayID)`.
///
/// `MACH_PORT_NULL`, which is what a Mac answers for a display with no IOKit
/// service behind it. There is no IORegistry here, and a fabricated port number
/// would be one a caller went on to send a message to.
#[allow(non_snake_case)]
pub unsafe extern "C" fn CGDisplayIOServicePort(_display: u32) -> u32 {
    0
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
    let is_screen = unsafe { objc::object::class_of(screen) }
        .is_some_and(|c| objc::class::lookup(c"NSScreen").is_some_and(|k| std::ptr::eq(c, k)));
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
        "maximumFramesPerSecond" => maximum_frames_per_second, c"q16@0:8";
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

/// `CGError CGWarpMouseCursorPosition(CGPoint)` - move the pointer, in global
/// screen coordinates.
///
/// SDL's own warp takes the same frame, so this is the one CoreGraphics mouse
/// call that carries across exactly. `kCGErrorSuccess` is `0`.
#[allow(non_snake_case)]
pub unsafe extern "C" fn CGWarpMouseCursorPosition(point: foundation::geometry::NSPoint) -> i32 {
    sdl3::video::warp_mouse_global(point.x as f32, point.y as f32);
    0
}

/// `CGError CGAssociateMouseAndMouseCursorPosition(boolean_t connected)`.
///
/// Disassociating is the second half of mouse look: a game hides the cursor and
/// then breaks the link between pointer and cursor so the cursor stops moving
/// while deltas keep arriving. SDL's relative mouse mode is the same thing, so
/// that is what this turns on.
///
/// The mode is per-window in SDL and process-wide in CoreGraphics, hence
/// [`apply_mouse_association`] over every window rather than the key one.
/// `kCGErrorSuccess` is `0`, and stays the answer on a host whose SDL is too
/// old for the mode: a program told this failed treats mouse look as
/// unavailable rather than running without capture.
#[allow(non_snake_case)]
pub unsafe extern "C" fn CGAssociateMouseAndMouseCursorPosition(connected: u32) -> i32 {
    ASSOCIATED.store(connected != 0, std::sync::atomic::Ordering::Release);
    apply_mouse_association();
    0
}

/// Whether the pointer and the cursor are meant to be moving together.
///
/// Read back rather than merely written: a caller that disassociates and then
/// asks is entitled to be told what it set.
static ASSOCIATED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

/// Whether the last [`CGAssociateMouseAndMouseCursorPosition`] asked for the
/// cursor to follow the pointer.
pub fn mouse_is_associated() -> bool {
    ASSOCIATED.load(std::sync::atomic::Ordering::Acquire)
}

/// Put every window into relative mouse mode, or take it out, to match the last
/// [`CGAssociateMouseAndMouseCursorPosition`].
///
/// Called again when a window takes focus, because SDL leaves the mode when
/// focus goes and a window made after the guest disassociated never had it.
pub fn apply_mouse_association() {
    let relative = !mouse_is_associated();
    for window in crate::window::sdl_windows() {
        if sdl3::video::relative_mouse_mode(window) != relative {
            sdl3::video::set_relative_mouse_mode(window, relative);
        }
    }
}

/// `bool CGEventSourceKeyState(CGEventSourceStateID, CGKeyCode)`.
///
/// False for every key. This asks the *window server* whether a key is down
/// right now, independently of the event queue, and there is no window server
/// here to ask - SDL's keyboard state is this process's own view and answers a
/// different question, one that is wrong in exactly the case the call exists
/// for (a key pressed while the program was not focused).
///
/// False rather than refused because the return type has no error value: a
/// caller gets "not pressed", which is the answer for all but one key at any
/// moment anyway.
#[allow(non_snake_case)]
pub unsafe extern "C" fn CGEventSourceKeyState(_source: i32, _key: u16) -> u8 {
    0
}
