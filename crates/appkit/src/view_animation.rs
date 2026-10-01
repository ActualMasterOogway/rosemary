//! `NSAnimation` and `NSViewAnimation`, run to completion at once.
//!
//! An `NSViewAnimation` moves, resizes, fades in or fades out views and
//! windows. What a program depends on is where each one ends up and that the
//! delegate hears the animation end, so `-startAnimation` applies every end
//! state immediately and then tells the delegate.
//!
//! FIXME (ACCURACY): no frame between the start and the end is ever drawn.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use foundation::geometry::NSRect;
use objc::object::Id;
use objc::sel::Sel;

use crate::define::methods;

#[derive(Clone, Copy, Default)]
struct Animation {
    /// Retained `NSArray` of dictionaries.
    animations: usize,
    /// Unretained, as AppKit's delegate is.
    delegate: usize,
    duration: f64,
    animating: bool,
}

fn animations() -> &'static Mutex<HashMap<usize, Animation>> {
    static ANIMATIONS: OnceLock<Mutex<HashMap<usize, Animation>>> = OnceLock::new();
    ANIMATIONS.get_or_init(Default::default)
}

fn read(receiver: Id) -> Animation {
    animations()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&(receiver as usize))
        .copied()
        .unwrap_or_default()
}

fn update(receiver: Id, change: impl FnOnce(&mut Animation)) {
    change(
        animations()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(receiver as usize)
            .or_default(),
    );
}

fn send_get(receiver: Id, selector: &str, argument: Id) -> Id {
    // SAFETY: each selector here is `id (id, SEL, id)`.
    unsafe {
        let f: unsafe extern "C" fn(Id, Sel, Id) -> Id = std::mem::transmute(objc::msg::entry());
        f(receiver, objc::sel::get(selector), argument)
    }
}

fn value_for(dictionary: Id, key: &str) -> Id {
    let key = foundation::string::from_str(key);
    let value = send_get(dictionary, "objectForKey:", key);
    objc::arc::release(key);
    value
}

fn text_of(string: Id) -> String {
    // SAFETY: a string, or nil.
    String::from_utf8_lossy(&unsafe { foundation::string::bytes(string) }).into_owned()
}

/// One dictionary's end state: the end frame, and the effect.
fn finish(dictionary: Id) {
    let target = value_for(dictionary, "NSViewAnimationTargetKey");
    if target.is_null() {
        return;
    }
    let is_window = crate::window::is_window(target);
    let end = value_for(dictionary, "NSViewAnimationEndFrameKey");
    if !end.is_null() {
        // SAFETY: an `NSValue` of a rect answers `-rectValue`; a window and a
        // view both take `-setFrame:`, the window's with a display flag.
        unsafe {
            // A rect comes back through a buffer, so through the `_stret` send.
            let rect: unsafe extern "C" fn(Id, Sel) -> NSRect =
                std::mem::transmute(objc::msg::entry_stret());
            let frame = rect(end, objc::sel::get("rectValue"));
            match is_window {
                true => {
                    let f: unsafe extern "C" fn(Id, Sel, NSRect, u8) =
                        std::mem::transmute(objc::msg::entry());
                    f(target, objc::sel::get("setFrame:display:"), frame, 1);
                }
                false => {
                    let f: unsafe extern "C" fn(Id, Sel, NSRect) =
                        std::mem::transmute(objc::msg::entry());
                    f(target, objc::sel::get("setFrame:"), frame);
                }
            }
        }
    }
    let effect = text_of(value_for(dictionary, "NSViewAnimationEffectKey"));
    let shown = match effect.as_str() {
        "NSViewAnimationFadeInEffect" => true,
        "NSViewAnimationFadeOutEffect" => false,
        _ => return,
    };
    // A view fades by `hidden`, a window by being ordered in or out.
    let (selector, argument) = match (is_window, shown) {
        (true, true) => ("orderFront:", std::ptr::null_mut()),
        (true, false) => ("orderOut:", std::ptr::null_mut()),
        (false, shown) => ("setHidden:", usize::from(!shown) as Id),
    };
    // SAFETY: `-orderFront:`/`-orderOut:` take an object and `-setHidden:` a
    // `BOOL`, both in the first argument register.
    unsafe {
        let f: unsafe extern "C" fn(Id, Sel, Id) = std::mem::transmute(objc::msg::entry());
        f(target, objc::sel::get(selector), argument);
    }
}

unsafe extern "C" fn init_with_view_animations(receiver: Id, _sel: Sel, list: Id) -> Id {
    // SAFETY: `-[NSAnimation initWithDuration:animationCurve:]` would set a
    // duration; AppKit's default for a view animation is half a second.
    update(receiver, |a| {
        a.animations = objc::arc::retain(list) as usize;
        a.duration = 0.5;
    });
    receiver
}

unsafe extern "C" fn init_with_duration(
    receiver: Id,
    _sel: Sel,
    duration: f64,
    _curve: usize,
) -> Id {
    update(receiver, |a| a.duration = duration);
    receiver
}

unsafe extern "C" fn set_view_animations(receiver: Id, _sel: Sel, list: Id) {
    let list = objc::arc::retain(list) as usize;
    let mut old = 0;
    update(receiver, |a| {
        old = std::mem::replace(&mut a.animations, list)
    });
    objc::arc::release(old as Id);
}

unsafe extern "C" fn view_animations(receiver: Id, _sel: Sel) -> Id {
    read(receiver).animations as Id
}

unsafe extern "C" fn set_delegate(receiver: Id, _sel: Sel, delegate: Id) {
    update(receiver, |a| a.delegate = delegate as usize);
}

unsafe extern "C" fn delegate(receiver: Id, _sel: Sel) -> Id {
    read(receiver).delegate as Id
}

unsafe extern "C" fn set_duration(receiver: Id, _sel: Sel, duration: f64) {
    update(receiver, |a| a.duration = duration);
}

unsafe extern "C" fn duration(receiver: Id, _sel: Sel) -> f64 {
    read(receiver).duration
}

unsafe extern "C" fn is_animating(receiver: Id, _sel: Sel) -> u8 {
    u8::from(read(receiver).animating)
}

/// Settings that shape the frames in between, of which there are none.
unsafe extern "C" fn ignored_integer(_receiver: Id, _sel: Sel, _value: usize) {}

unsafe extern "C" fn ignored_float(_receiver: Id, _sel: Sel, _value: f32) {}

unsafe extern "C" fn progress(_receiver: Id, _sel: Sel) -> f32 {
    1.0
}

fn tell_delegate(receiver: Id, selector: &str) {
    let delegate = read(receiver).delegate as Id;
    let sel = objc::sel::get(selector);
    // SAFETY: a live delegate, or nil.
    let responds =
        unsafe { objc::object::class_of(delegate) }.is_some_and(|c| c.method(sel).is_some());
    if responds {
        send_get(delegate, selector, receiver);
    }
}

/// `-startAnimation`: every end state now, then the delegate's
/// `-animationDidEnd:`.
unsafe extern "C" fn start_animation(receiver: Id, _sel: Sel) {
    // SAFETY: an array of dictionaries, or nil.
    let list = unsafe { corefoundation::collections::array(read(receiver).animations as _) }
        .map(|a| a.values.clone())
        .unwrap_or_default();
    update(receiver, |a| a.animating = true);
    for dictionary in list {
        finish(dictionary as Id);
    }
    update(receiver, |a| a.animating = false);
    tell_delegate(receiver, "animationDidEnd:");
}

unsafe extern "C" fn stop_animation(receiver: Id, _sel: Sel) {
    if read(receiver).animating {
        update(receiver, |a| a.animating = false);
        tell_delegate(receiver, "animationDidStop:");
    }
}

unsafe extern "C" fn dealloc(receiver: Id, _sel: Sel) {
    let gone = animations()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&(receiver as usize));
    if let Some(animation) = gone {
        objc::arc::release(animation.animations as Id);
    }
    objc::arc::finalize(receiver);
}

pub(crate) fn install() {
    let animation = objc::class::lookup(c"NSAnimation").expect("declared in crate::classes");
    methods!(animation, {
        "initWithDuration:animationCurve:" => init_with_duration, c"@32@0:8d16Q24";
        "setDelegate:" => set_delegate, c"v24@0:8@16";
        "delegate" => delegate, c"@16@0:8";
        "setDuration:" => set_duration, c"v24@0:8d16";
        "duration" => duration, c"d16@0:8";
        "setAnimationBlockingMode:" => ignored_integer, c"v24@0:8Q16";
        "setAnimationCurve:" => ignored_integer, c"v24@0:8Q16";
        "setFrameRate:" => ignored_float, c"v20@0:8f16";
        "currentProgress" => progress, c"f16@0:8";
        "isAnimating" => is_animating, c"c16@0:8";
        "startAnimation" => start_animation, c"v16@0:8";
        "stopAnimation" => stop_animation, c"v16@0:8";
        "dealloc" => dealloc, c"v16@0:8";
    });
    let view_animation =
        objc::class::lookup(c"NSViewAnimation").expect("declared in crate::classes");
    methods!(view_animation, {
        "initWithViewAnimations:" => init_with_view_animations, c"@24@0:8@16";
        "setViewAnimations:" => set_view_animations, c"v24@0:8@16";
        "viewAnimations" => view_animations, c"@16@0:8";
    });
}
