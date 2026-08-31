//! `NSThread`, as far as detaching one.
//!
//! A detached thread is a real host thread, so a guest method reached this way
//! runs the same way one reached through `pthread_create` does. The class stays
//! otherwise abstract: a thread *object* would have to answer for a thread this
//! module does not own - the main one, and every one a guest made itself - and
//! there is nothing here that could.

use objc::object::Id;
use objc::sel::Sel;

use crate::define::class_methods;

/// A retained object, so the sending thread cannot release what the new thread
/// is about to be handed.
struct Detached {
    target: usize,
    object: usize,
    selector: usize,
}

// SAFETY: the two objects are retained for the new thread's lifetime, and every
// runtime they belong to is shared across threads already.
unsafe impl Send for Detached {}

unsafe extern "C" fn detach(_class: Id, _sel: Sel, selector: Sel, target: Id, object: Id) {
    if target.is_null() || selector.is_null() {
        return;
    }
    let detached = Detached {
        target: objc::arc::retain(target) as usize,
        object: objc::arc::retain(object) as usize,
        selector: selector as usize,
    };
    std::thread::spawn(move || {
        let detached = detached;
        let (target, object) = (detached.target as Id, detached.object as Id);
        let selector = detached.selector as Sel;
        // Apple gives a detached thread a pool of its own, and a method written
        // for one autoreleases into it.
        let pool = objc::arc::pool_push();
        // SAFETY: a live object, retained above.
        let method = unsafe { objc::object::class_of(target) }.and_then(|c| c.method(selector));
        let Some(method) = method else {
            objc::fail!(
                "+[NSThread detachNewThreadSelector:toTarget:withObject:]: the target does not \
                 respond to the selector"
            );
        };
        // SAFETY: a detached thread's method is `void (id, SEL, id)`.
        unsafe {
            let f: unsafe extern "C" fn(Id, Sel, Id) = std::mem::transmute(method.implementation());
            f(target, selector, object);
        }
        objc::arc::pool_pop(pool);
        objc::arc::release(object);
        objc::arc::release(target);
    });
}

unsafe extern "C" fn sleep_for_interval(_class: Id, _sel: Sel, seconds: f64) {
    if seconds > 0.0 {
        std::thread::sleep(std::time::Duration::from_secs_f64(seconds));
    }
}

pub(crate) fn install() {
    let class = objc::class::lookup(c"NSThread").expect("declared abstract by this crate");
    class_methods!(class, {
        "detachNewThreadSelector:toTarget:withObject:" => detach, c"v40@0:8:16@24@32";
        "sleepForTimeInterval:" => sleep_for_interval, c"v24@0:8d16";
    });
}
