//! `NSNotification` and `NSNotificationCenter`.
//!
//! Here rather than in AppKit because a notification is Foundation's, but the
//! reason it exists now is AppKit's: `applicationWillFinishLaunching:` takes
//! one, and a delegate that reads `-object` from a fabricated argument would
//! read whatever was in the register.
//!
//! The observer table is a vector scanned linearly, for the same reason
//! `CFDictionary` is: what a program registers is a handful of entries, and a
//! hash index beside it is the fix when one shows up in a profile rather than
//! now.

use std::ffi::c_void;

use objc::class::Class;
use objc::object::Id;
use objc::sel::Sel;

use crate::define::{class_methods, methods};

/// An `NSNotification`, laid out by this crate.
#[repr(C)]
struct Notification {
    isa: *const c_void,
    name: Id,
    object: Id,
    user_info: Id,
}

/// One `-addObserver:selector:name:object:`.
///
/// The observer is *not* retained, which is Apple's rule and the reason
/// `-removeObserver:` exists at all. A center that retained its observers would
/// keep every one of them alive forever.
struct Observer {
    observer: Id,
    selector: Sel,
    /// `None` matches any name.
    name: Option<String>,
    /// Null matches any sender.
    object: Id,
}

// SAFETY: the ids are only ever used on the thread that posts, and the guard
// below is what serializes access to the table itself.
unsafe impl Send for Observer {}

static OBSERVERS: std::sync::Mutex<Vec<Observer>> = std::sync::Mutex::new(Vec::new());

/// The one center, which `+defaultCenter` must return every time.
static DEFAULT: std::sync::OnceLock<usize> = std::sync::OnceLock::new();

fn notification_class() -> &'static Class {
    corefoundation::base::register(c"NSNotification", None, size_of::<Notification>(), false)
}

/// Build a notification, autoreleased, as every method that hands one out does.
///
/// `name` is text rather than an `NSString` because every caller here has text:
/// the constants are `&'static str` in this crate and in AppKit.
pub fn make(name: &str, object: Id, user_info: Id) -> Id {
    crate::bootstrap();
    // SAFETY: `Notification` is the layout `notification_class` was registered
    // with, and its first field is the `isa`.
    let n = unsafe { corefoundation::base::allocate::<Notification>(notification_class()) };
    // SAFETY: freshly allocated, and every field is written before it is read.
    unsafe {
        (*n).name = objc::arc::retain(crate::string::from_str(name));
        (*n).object = objc::arc::retain(object);
        (*n).user_info = objc::arc::retain(user_info);
    }
    objc::arc::autorelease(n as Id)
}

/// Post `name` to every observer that asked for it.
///
/// Callable from this tree as well as from a guest, which is what AppKit's
/// launch notifications use.
pub fn post(name: &str, object: Id, user_info: Id) {
    let notification = make(name, object, user_info);
    // Copied out before sending: an observer is entitled to add or remove one
    // from inside its own handler, and holding the lock across the send would
    // deadlock on the first that did.
    let targets: Vec<(Id, Sel)> = OBSERVERS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .filter(|o| o.name.as_deref().is_none_or(|n| n == name))
        .filter(|o| o.object.is_null() || o.object == object)
        .map(|o| (o.observer, o.selector))
        .collect();
    for (observer, selector) in targets {
        // SAFETY: an observer method is `void (id, SEL, NSNotification *)` by
        // the contract of `-addObserver:selector:name:object:`.
        unsafe { send_notification(observer, selector, notification) };
    }
}

/// # Safety
///
/// `observer` must be live and `selector` must name a method taking one object.
unsafe fn send_notification(observer: Id, selector: Sel, notification: Id) {
    // SAFETY: the caller's obligation.
    let Some(method) =
        (unsafe { objc::object::class_of(observer) }).and_then(|c| c.method(selector))
    else {
        // Apple raises here. A named abort instead, because a handler that is
        // not there is a program whose notification never arrives, and silence
        // makes that look like a notification that was never posted.
        objc::fail!(
            "-[NSNotificationCenter postNotification:]: the observer does not respond to the \
             selector it registered"
        );
    };
    // SAFETY: the caller's obligation.
    let f: unsafe extern "C" fn(Id, Sel, Id) = unsafe {
        std::mem::transmute::<*const c_void, unsafe extern "C" fn(Id, Sel, Id)>(
            method.implementation(),
        )
    };
    // SAFETY: as above.
    unsafe { f(observer, selector, notification) };
}

unsafe extern "C" fn name(receiver: Id, _sel: Sel) -> Id {
    // SAFETY: a notification.
    unsafe { (*(receiver as *const Notification)).name }
}

unsafe extern "C" fn object(receiver: Id, _sel: Sel) -> Id {
    // SAFETY: a notification.
    unsafe { (*(receiver as *const Notification)).object }
}

unsafe extern "C" fn user_info(receiver: Id, _sel: Sel) -> Id {
    // SAFETY: a notification.
    unsafe { (*(receiver as *const Notification)).user_info }
}

unsafe extern "C" fn notification_with_name(class: Id, _sel: Sel, name: Id, sender: Id) -> Id {
    let _ = class;
    // SAFETY: `name` is null or a string.
    let text = unsafe { text_of(name) };
    make(&text, sender, std::ptr::null_mut())
}

unsafe extern "C" fn default_center(class: Id, _sel: Sel) -> Id {
    *DEFAULT.get_or_init(|| {
        // SAFETY: `class` is `NSNotificationCenter`, which this crate registered.
        let center = unsafe {
            objc::object::create_instance(
                objc::class::from_ptr(class as *const Class).expect("a class"),
                0,
            )
        };
        // Retained forever: a singleton a guest can release is one it can make
        // dangle by releasing what it never owned.
        objc::arc::retain(center);
        center as usize
    }) as Id
}

unsafe extern "C" fn add_observer(
    _receiver: Id,
    _sel: Sel,
    observer: Id,
    selector: Sel,
    name: Id,
    object: Id,
) {
    // SAFETY: null or a string.
    let name = (!name.is_null()).then(|| unsafe { text_of(name) });
    OBSERVERS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(Observer {
            observer,
            selector,
            name,
            object,
        });
}

unsafe extern "C" fn remove_observer(_receiver: Id, _sel: Sel, observer: Id) {
    OBSERVERS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|o| o.observer != observer);
}

unsafe extern "C" fn remove_observer_name(
    _receiver: Id,
    _sel: Sel,
    observer: Id,
    name: Id,
    object: Id,
) {
    // SAFETY: null or a string.
    let name = (!name.is_null()).then(|| unsafe { text_of(name) });
    OBSERVERS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|o| {
            o.observer != observer
                || (name.is_some() && o.name != name)
                || (!object.is_null() && o.object != object)
        });
}

unsafe extern "C" fn post_notification(_receiver: Id, _sel: Sel, notification: Id) {
    if notification.is_null() {
        return;
    }
    // SAFETY: a notification this crate made.
    let n = unsafe { &*(notification as *const Notification) };
    // SAFETY: its name field is a string.
    let text = unsafe { text_of(n.name) };
    post(&text, n.object, n.user_info);
}

unsafe extern "C" fn post_name(_receiver: Id, _sel: Sel, name: Id, object: Id) {
    // SAFETY: null or a string.
    let text = unsafe { text_of(name) };
    post(&text, object, std::ptr::null_mut());
}

unsafe extern "C" fn post_name_user_info(
    _receiver: Id,
    _sel: Sel,
    name: Id,
    object: Id,
    user_info: Id,
) {
    // SAFETY: null or a string.
    let text = unsafe { text_of(name) };
    post(&text, object, user_info);
}

/// A notification name as text.
///
/// # Safety
///
/// `string` must be null or a string object.
unsafe fn text_of(string: Id) -> String {
    if string.is_null() {
        return String::new();
    }
    // SAFETY: the caller's obligation.
    String::from_utf8_lossy(&unsafe { crate::string::bytes(string) }).into_owned()
}

/// A notification name a guest reads as a global, as `(Mach-O name, object)`.
///
/// Interned the same way IOKit's keys are, and for the same reason: a guest
/// compares the constant against the name it was handed, and two objects with
/// equal text are not the same object.
pub fn name_object(text: &'static str) -> u64 {
    static NAMES: std::sync::Mutex<Vec<(&'static str, u64)>> = std::sync::Mutex::new(Vec::new());
    crate::bootstrap();
    let mut names = NAMES.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((_, addr)) = names.iter().find(|(t, _)| *t == text) {
        return *addr;
    }
    let object = crate::string::from_str(text);
    // Retained forever: a global a guest may release is one it can make dangle.
    objc::arc::retain(object);
    let addr = object as usize as u64;
    names.push((text, addr));
    addr
}

pub(crate) fn install() {
    let notification = notification_class();
    methods!(notification, {
        "name" => name, c"@16@0:8";
        "object" => object, c"@16@0:8";
        "userInfo" => user_info, c"@16@0:8";
    });
    class_methods!(notification, {
        "notificationWithName:object:" => notification_with_name, c"@32@0:8@16@24";
    });

    let center =
        corefoundation::base::register(c"NSNotificationCenter", None, size_of::<usize>(), false);
    class_methods!(center, {
        "defaultCenter" => default_center, c"@16@0:8";
    });
    methods!(center, {
        "addObserver:selector:name:object:" => add_observer, c"v48@0:8@16:24@32@40";
        "removeObserver:" => remove_observer, c"v24@0:8@16";
        "removeObserver:name:object:" => remove_observer_name, c"v40@0:8@16@24@32";
        "postNotification:" => post_notification, c"v24@0:8@16";
        "postNotificationName:object:" => post_name, c"v32@0:8@16@24";
        "postNotificationName:object:userInfo:" => post_name_user_info, c"v40@0:8@16@24@32";
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    static SEEN: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

    unsafe extern "C" fn record(_receiver: Id, _sel: Sel, notification: Id) {
        // SAFETY: the poster built it.
        let n = unsafe { &*(notification as *const Notification) };
        // SAFETY: its name is a string.
        let text = unsafe { text_of(n.name) };
        SEEN.lock().unwrap_or_else(|e| e.into_inner()).push(text);
    }

    /// An observer built for one test, so the tests do not share a table entry.
    fn observer(class_name: &std::ffi::CStr) -> Id {
        crate::bootstrap();
        let class = corefoundation::base::register(class_name, None, size_of::<usize>(), false);
        methods!(class, {
            "heard:" => record, c"v24@0:8@16";
        });
        objc::object::create_instance(class, 0)
    }

    #[test]
    fn an_observer_hears_the_name_it_registered_and_no_other() {
        let who = observer(c"R3TestNotificationObserver");
        OBSERVERS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(Observer {
                observer: who,
                selector: objc::sel::get("heard:"),
                name: Some("R3Wanted".into()),
                object: std::ptr::null_mut(),
            });
        SEEN.lock().unwrap_or_else(|e| e.into_inner()).clear();
        post("R3Unwanted", std::ptr::null_mut(), std::ptr::null_mut());
        post("R3Wanted", std::ptr::null_mut(), std::ptr::null_mut());
        let seen = SEEN.lock().unwrap_or_else(|e| e.into_inner()).clone();
        assert_eq!(seen, ["R3Wanted"]);
    }

    /// The notification the observer is handed carries the sender it was posted
    /// with, which is the whole reason the argument is a real object.
    #[test]
    fn a_notification_carries_its_sender() {
        crate::bootstrap();
        let sender = crate::string::from_str("sender");
        let n = make("R3Named", sender, std::ptr::null_mut());
        // SAFETY: `-object` is `id (id, SEL)`.
        let got = unsafe { object(n, objc::sel::get("object")) };
        assert_eq!(got, sender);
        // SAFETY: `-name` is `id (id, SEL)`, and its result is a string.
        let text = unsafe { text_of(name(n, objc::sel::get("name"))) };
        assert_eq!(text, "R3Named");
    }

    /// The same name is the same object every time, because a guest compares
    /// the global it was given against the one it stored.
    #[test]
    fn a_name_global_is_one_object() {
        assert_eq!(
            name_object("NSApplicationDidFinishLaunchingNotification"),
            name_object("NSApplicationDidFinishLaunchingNotification")
        );
    }
}
