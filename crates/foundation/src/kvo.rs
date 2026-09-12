//! Key-value observing: `-addObserver:forKeyPath:options:context:`.
//!
//! The half of key-value observing that can be built here. A registration is
//! recorded, and a notification is delivered when something **announces** a change
//! with `-willChangeValueForKey:`/`-didChangeValueForKey:`. What does not exist is
//! Apple's *automatic* notification, where the runtime rewrites a class's setters
//! so that `-setFoo:` notifies without the class asking - that needs a subclass
//! synthesised per observed class and a setter replaced on it, and no program in
//! this tree has needed it.
//!
//! So the line is: a property changed through `will`/`did` notifies its observers,
//! and a property changed by writing an ivar does not. That is the same line
//! Apple's own documentation draws for a class that declares
//! `+automaticallyNotifiesObserversForKey:` false, and a caller on the wrong side
//! of it sees a value that is correct when read and an observer that did not fire.
//!
//! Registration is still worth accepting on its own: `-addObserver:...` with no
//! implementation is an abort on the first observer a program installs, which is
//! during startup - AppKit's appearance is observed that way - where a
//! registration that never fires is a program that runs and does not follow a
//! change nothing here can make anyway.

use objc::object::Id;
use objc::sel::Sel;

use crate::define::methods;

/// `NSKeyValueObservingOptions`.
mod options {
    /// `NSKeyValueObservingOptionNew`: put the new value in the change dictionary.
    pub const NEW: usize = 0x01;
    /// `NSKeyValueObservingOptionOld`.
    pub const OLD: usize = 0x02;
    /// `NSKeyValueObservingOptionInitial`: notify once at registration, before any
    /// change. Honoured, because a caller that passes it reads its *current*
    /// state in the callback and does nothing until then.
    pub const INITIAL: usize = 0x04;
}

/// `NSKeyValueChangeKindKey`'s value for a plain assignment.
const CHANGE_SETTING: i64 = 1;

/// One registration. Addresses rather than `Id`s so the table can be a `static`,
/// as `corefoundation::preferences` does it.
struct Observation {
    observed: usize,
    observer: usize,
    key_path: Vec<u8>,
    context: usize,
    options: usize,
}

fn observations() -> &'static std::sync::Mutex<Vec<Observation>> {
    static OBSERVATIONS: std::sync::Mutex<Vec<Observation>> = std::sync::Mutex::new(Vec::new());
    &OBSERVATIONS
}

/// The change dictionary handed to an observer, autoreleased.
///
/// `NSKeyValueChangeNewKey` carries the value read back out of the object, which
/// is what "new" means at the moment `did` is called. The old value is not
/// carried: it would have to have been captured during `-willChangeValueForKey:`,
/// and a caller may call `did` without `will` - so an entry that was sometimes the
/// new value would be worse than an absent one the observer already handles.
fn change_dictionary(new: Option<Id>) -> Id {
    let kind = crate::number(CHANGE_SETTING);
    let mut pairs: Vec<(
        corefoundation::base::CFTypeRef,
        corefoundation::base::CFTypeRef,
    )> = vec![(
        crate::string::from_str("kind") as corefoundation::base::CFTypeRef,
        kind as corefoundation::base::CFTypeRef,
    )];
    if let Some(new) = new.filter(|n| !n.is_null()) {
        pairs.push((
            crate::string::from_str("new") as corefoundation::base::CFTypeRef,
            new as corefoundation::base::CFTypeRef,
        ));
    }
    let dictionary = corefoundation::collections::new_dictionary(
        &pairs,
        corefoundation::callbacks::Policy::from_key_callbacks(),
        corefoundation::callbacks::Policy::from_type_callbacks(),
        false,
    );
    for (key, _) in &pairs {
        // The dictionary took its own reference to each key.
        // SAFETY: a string this function made.
        unsafe { corefoundation::api::CFRelease(*key) };
    }
    // SAFETY: a fresh dictionary.
    unsafe { corefoundation::api::CFAutorelease(dictionary) as Id }
}

/// Send `-observeValueForKeyPath:ofObject:change:context:` to one observer.
///
/// # Safety
///
/// Every argument is null or a live object.
unsafe fn notify(observer: Id, key_path: &[u8], observed: Id, change: Id, context: usize) {
    let sel = objc::sel::get("observeValueForKeyPath:ofObject:change:context:");
    // SAFETY: the caller's obligation.
    let Some(method) = (unsafe { objc::object::class_of(observer) }).and_then(|c| c.method(sel))
    else {
        // An observer with no callback is a program that registered and does not
        // want to hear, which is its own business rather than a failure here.
        return;
    };
    let path = crate::string::from_str(&String::from_utf8_lossy(key_path));
    // SAFETY: the method's signature is
    // `void (id, SEL, NSString *, id, NSDictionary *, void *)`.
    unsafe {
        let f: unsafe extern "C" fn(Id, Sel, Id, Id, Id, usize) =
            std::mem::transmute(method.implementation());
        f(observer, sel, path, observed, change, context);
        corefoundation::api::CFRelease(path as corefoundation::base::CFTypeRef);
    }
}

/// The value of `key_path` on `object`, through key-value coding.
///
/// # Safety
///
/// `object` is null or a live object.
unsafe fn current(object: Id, key_path: &[u8]) -> Option<Id> {
    let sel = objc::sel::get("valueForKeyPath:");
    // SAFETY: the caller's obligation.
    let method = (unsafe { objc::object::class_of(object) })?.method(sel)?;
    let path = crate::string::from_str(&String::from_utf8_lossy(key_path));
    // SAFETY: `-valueForKeyPath:` is `id (id, SEL, NSString *)`.
    let value = unsafe {
        let f: unsafe extern "C" fn(Id, Sel, Id) -> Id = std::mem::transmute(method.implementation());
        let value = f(object, sel, path);
        corefoundation::api::CFRelease(path as corefoundation::base::CFTypeRef);
        value
    };
    Some(value)
}

/// # Safety
///
/// `key_path` is an `NSString` or nil; the rest are live objects or nil.
unsafe extern "C" fn add_observer(
    receiver: Id,
    _sel: Sel,
    observer: Id,
    key_path: Id,
    options: usize,
    context: usize,
) {
    // SAFETY: the caller's obligation.
    let path = unsafe { crate::string::bytes(key_path) };
    observations()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(Observation {
            observed: receiver as usize,
            observer: observer as usize,
            key_path: path.clone(),
            context,
            options,
        });
    if options & options::INITIAL != 0 {
        // SAFETY: live objects, and the dictionary is freshly made.
        unsafe {
            let new = match options & options::NEW != 0 {
                true => current(receiver, &path),
                false => None,
            };
            notify(observer, &path, receiver, change_dictionary(new), context);
        }
    }
}

/// `-removeObserver:forKeyPath:` and the form that also names the context.
///
/// A context of zero matches any registration, because that is the two-argument
/// form: it removes the observer for that key path however it registered.
///
/// # Safety
///
/// As [`add_observer`].
unsafe extern "C" fn remove_observer(receiver: Id, _sel: Sel, observer: Id, key_path: Id) {
    // SAFETY: the caller's obligation.
    let path = unsafe { crate::string::bytes(key_path) };
    observations()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|o| {
            !(o.observed == receiver as usize
                && o.observer == observer as usize
                && o.key_path == path)
        });
}

/// # Safety
///
/// As [`add_observer`].
unsafe extern "C" fn remove_observer_with_context(
    receiver: Id,
    _sel: Sel,
    observer: Id,
    key_path: Id,
    context: usize,
) {
    // SAFETY: the caller's obligation.
    let path = unsafe { crate::string::bytes(key_path) };
    observations()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|o| {
            !(o.observed == receiver as usize
                && o.observer == observer as usize
                && o.key_path == path
                && o.context == context)
        });
}

/// `-willChangeValueForKey:`, which is recorded only so that the matching `did`
/// is not a surprise. Nothing is captured: see [`change_dictionary`].
unsafe extern "C" fn will_change(_receiver: Id, _sel: Sel, _key: Id) {}

/// `-didChangeValueForKey:`, which is what actually notifies.
///
/// # Safety
///
/// As [`add_observer`].
unsafe extern "C" fn did_change(receiver: Id, _sel: Sel, key: Id) {
    // SAFETY: the caller's obligation.
    let key = unsafe { crate::string::bytes(key) };
    // Copied out from under the lock: an observer's callback is guest code and may
    // add or remove a registration, which would deadlock a held lock and would at
    // best be iterating a list while it changed.
    let interested: Vec<(usize, usize, usize)> = observations()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .filter(|o| o.observed == receiver as usize && o.key_path == key)
        .map(|o| (o.observer, o.context, o.options))
        .collect();
    for (observer, context, options) in interested {
        // SAFETY: objects registered by `add_observer`, which is sent to live ones.
        unsafe {
            let new = match options & (options::NEW | options::OLD) != 0 {
                true => current(receiver, &key),
                false => None,
            };
            notify(
                observer as Id,
                &key,
                receiver,
                change_dictionary(new),
                context,
            );
        }
    }
}

pub(crate) fn install() {
    let nsobject = objc::class::lookup(c"NSObject").expect("the runtime's own NSObject");
    methods!(nsobject, {
        "addObserver:forKeyPath:options:context:" => add_observer, c"v48@0:8@16@24Q32^v40";
        "removeObserver:forKeyPath:" => remove_observer, c"v32@0:8@16@24";
        "removeObserver:forKeyPath:context:" => remove_observer_with_context,
        c"v40@0:8@16@24^v32";
        "willChangeValueForKey:" => will_change, c"v24@0:8@16";
        "didChangeValueForKey:" => did_change, c"v24@0:8@16";
    });
}

/// The change-dictionary keys, as `(Mach-O name, value)`.
///
/// Each is an `NSString *` **variable**, so an unresolved one binds to zero and the
/// observer passes address zero as the key it looks its new value up by. The values
/// are Apple's: `NSKeyValueChangeNewKey` is the string `"new"`.
pub const KEYS: &[(&str, &str)] = &[
    ("_NSKeyValueChangeKindKey", "kind"),
    ("_NSKeyValueChangeNewKey", "new"),
    ("_NSKeyValueChangeOldKey", "old"),
    ("_NSKeyValueChangeIndexesKey", "indexes"),
    ("_NSKeyValueChangeNotificationIsPriorKey", "notificationIsPrior"),
];

#[cfg(test)]
mod tests {
    use super::*;

    /// A class that records what it was told, so the notification can be checked
    /// rather than just the registration.
    ///
    /// Shared by every test in this module, which run in parallel - so each test
    /// uses a key path of its own and reads back only its own entries. A test that
    /// asserted on the whole list would be asserting on another test's timing.
    static SEEN: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

    /// What this key path's observer was told, in order.
    fn seen(key_path: &str) -> Vec<String> {
        SEEN.lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|entry| entry.starts_with(&format!("{key_path}:")))
            .cloned()
            .collect()
    }

    unsafe extern "C" fn observed(
        _receiver: Id,
        _sel: Sel,
        key_path: Id,
        _object: Id,
        _change: Id,
        context: usize,
    ) {
        // SAFETY: the key path is the `NSString` `notify` made.
        let path = unsafe { crate::string::bytes(key_path) };
        SEEN.lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(format!("{}:{context}", String::from_utf8_lossy(&path)));
    }

    fn observer_object() -> Id {
        crate::bootstrap();
        let class = corefoundation::base::register(c"R3KvoObserver", None, size_of::<usize>(), false);
        class.replace_method(
            objc::sel::get("observeValueForKeyPath:ofObject:change:context:"),
            observed as *const std::ffi::c_void,
            c"v48@0:8@16@24@32^v40",
        );
        objc::object::create_instance(class, 0)
    }

    fn plain_object() -> Id {
        crate::bootstrap();
        objc::object::create_instance(
            objc::class::lookup(c"NSObject").expect("NSObject"),
            0,
        )
    }

    /// The registration fires on `-didChangeValueForKey:` and stops firing once
    /// the observer is removed - which is the whole contract a caller relies on.
    #[test]
    fn an_observer_hears_an_announced_change_until_it_is_removed() {
        let subject = plain_object();
        let observer = observer_object();
        let path = crate::string::from_str("announced");
        // SAFETY: live objects this test made.
        unsafe {
            add_observer(
                subject,
                objc::sel::get("addObserver:forKeyPath:options:context:"),
                observer,
                path,
                0,
                0x99,
            );
            did_change(subject, objc::sel::get("didChangeValueForKey:"), path);
            assert_eq!(
                seen("announced"),
                ["announced:153"],
                "one notification, with the context it registered with"
            );

            remove_observer(
                subject,
                objc::sel::get("removeObserver:forKeyPath:"),
                observer,
                path,
            );
            did_change(subject, objc::sel::get("didChangeValueForKey:"), path);
            assert_eq!(
                seen("announced").len(),
                1,
                "removed, so the second change is not delivered"
            );
        }
    }

    /// A change to *another* key path is not this observer's.
    #[test]
    fn a_change_to_another_key_path_is_not_delivered() {
        let subject = plain_object();
        let observer = observer_object();
        let watched = crate::string::from_str("watched");
        let other = crate::string::from_str("other");
        // SAFETY: live objects this test made.
        unsafe {
            add_observer(
                subject,
                objc::sel::get("addObserver:forKeyPath:options:context:"),
                observer,
                watched,
                0,
                0,
            );
            did_change(subject, objc::sel::get("didChangeValueForKey:"), other);
            assert!(seen("watched").is_empty(), "a different key path");
        }
    }

    /// `NSKeyValueObservingOptionInitial` notifies at registration, before any
    /// change - a caller that passes it does nothing until that first callback.
    #[test]
    fn the_initial_option_notifies_straight_away() {
        let subject = plain_object();
        let observer = observer_object();
        let path = crate::string::from_str("initial");
        // SAFETY: live objects this test made.
        unsafe {
            add_observer(
                subject,
                objc::sel::get("addObserver:forKeyPath:options:context:"),
                observer,
                path,
                options::INITIAL,
                0,
            );
        }
        assert_eq!(
            seen("initial").len(),
            1,
            "the initial notification is sent from the registration itself"
        );
    }
}
