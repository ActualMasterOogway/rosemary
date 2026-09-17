//! `NSResponder`'s contextual menu and its default event methods, which every
//! responder inherits.
//!
//! `-menu` and `-setMenu:` are declared on `NSResponder`, so an application, a
//! window and a view all answer them - Qt sets one on `NSApp` while building its
//! menu bar, and on a view to give it a right-click menu.
//!
//! Kept beside the objects rather than in them, because `NSResponder` is declared
//! abstract here: its subclasses have layouts of their own and there is no word in
//! the base class to put a menu in. A side table is the one place a property
//! shared by three unrelated layouts can live.

use std::sync::Mutex;

use objc::object::Id;
use objc::sel::Sel;

use crate::define::methods;

/// `(responder, retained menu)`, both as addresses.
///
/// Addresses rather than `Id`s so the table can be a `static`: a raw pointer is
/// not `Send`, and what is stored here is owned by this module for the life of the
/// process rather than shared with a thread.
///
/// An entry outlives a responder that is deallocated, which is a handful of words
/// for the life of the process: a menu is set on the application and on the few
/// views that have one, and a program that sets one does not then destroy the
/// view a thousand times. The alternative is a `-dealloc` override on three
/// classes to reach one table, which is more moving parts than the leak is worth.
fn menus() -> &'static Mutex<Vec<(usize, usize)>> {
    static MENUS: Mutex<Vec<(usize, usize)>> = Mutex::new(Vec::new());
    &MENUS
}

unsafe extern "C" fn menu(receiver: Id, _sel: Sel) -> Id {
    let menus = menus().lock().unwrap_or_else(|e| e.into_inner());
    menus
        .iter()
        .find(|(object, _)| *object == receiver as usize)
        .map_or(std::ptr::null_mut(), |(_, menu)| *menu as Id)
}

unsafe extern "C" fn set_menu(receiver: Id, _sel: Sel, value: Id) {
    let mut menus = menus().lock().unwrap_or_else(|e| e.into_inner());
    let kept = objc::arc::retain(value) as usize;
    match menus
        .iter_mut()
        .find(|(object, _)| *object == receiver as usize)
    {
        Some((_, existing)) => {
            objc::arc::release(*existing as Id);
            *existing = kept;
        }
        None => menus.push((receiver as usize, kept)),
    }
}

/// Send an event method on to the next responder: a view's superview, then its
/// window. A window is the end of the chain here, as it has no window of its own.
///
/// # Safety
///
/// `receiver` is null or an object this runtime allocated, and `sel` is a
/// `void (id, SEL, NSEvent *)` method.
pub(crate) unsafe fn forward(receiver: Id, sel: Sel, event: Id) {
    // SAFETY: null or an object this runtime allocated.
    let next = match unsafe { crate::view::superview_of(receiver) } {
        // SAFETY: as above.
        next if next.is_null() => unsafe { crate::view::window_of(receiver) },
        next => next,
    };
    if next.is_null() {
        return;
    }
    // SAFETY: the prototype the caller vouches for.
    unsafe {
        let f: unsafe extern "C" fn(Id, Sel, Id) = std::mem::transmute(objc::msg::entry());
        f(next, sel, event);
    }
}

/// `-[NSResponder mouseUp:]`: a responder that does not handle the release
/// passes it up, so a container that took the press also sees it end.
unsafe extern "C" fn mouse_up(receiver: Id, sel: Sel, event: Id) {
    // SAFETY: installed as `v24@0:8@16`.
    unsafe { forward(receiver, sel, event) }
}

pub(crate) fn install() {
    let class = objc::class::lookup(c"NSResponder").expect("declared before its methods");
    methods!(class, {
        "menu" => menu, c"@16@0:8";
        "setMenu:" => set_menu, c"v24@0:8@16";
        "mouseUp:" => mouse_up, c"v24@0:8@16";
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn responder(class: &std::ffi::CStr) -> Id {
        crate::bootstrap();
        objc::object::create_instance(objc::class::lookup(class).expect("a class"), 0)
    }

    /// A menu set on one responder is not a menu set on another, and replacing one
    /// replaces it rather than adding a second entry.
    #[test]
    fn each_responder_keeps_its_own_menu() {
        let one = responder(c"NSView");
        let two = responder(c"NSView");
        let first = responder(c"NSMenu");
        let second = responder(c"NSMenu");
        // SAFETY: `-menu` and `-setMenu:` on responders of this crate's.
        unsafe {
            let get = objc::sel::get("menu");
            let set = objc::sel::get("setMenu:");
            assert!(menu(one, get).is_null(), "nothing set yet");
            set_menu(one, set, first);
            assert_eq!(menu(one, get), first);
            assert!(menu(two, get).is_null(), "a menu is not shared");
            set_menu(one, set, second);
            assert_eq!(menu(one, get), second, "replaced, not appended");
        }
    }

    /// `-mouseUp:` climbs to the first ancestor that overrides it.
    #[test]
    fn mouse_up_reaches_an_overriding_superview() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static SEEN: AtomicUsize = AtomicUsize::new(0);
        unsafe extern "C" fn record(receiver: Id, _sel: Sel, _event: Id) {
            SEEN.store(receiver as usize, Ordering::Release);
        }
        crate::bootstrap();
        let sel = objc::sel::get("mouseUp:");
        let view = objc::class::lookup(c"NSView").expect("NSView");
        let class =
            objc::class::allocate_pair(Some(view), c"MouseUpRecorder", 0).expect("a subclass");
        class.add_method(sel, record as objc::class::Imp, c"v24@0:8@16");
        objc::class::register_pair(class);
        let parent = objc::object::create_instance(class, 0);
        let child = responder(c"NSView");
        // SAFETY: `-addSubview:` is `void (id, SEL, id)` on views of this crate's.
        unsafe {
            let add: unsafe extern "C" fn(Id, Sel, Id) = std::mem::transmute(objc::msg::entry());
            add(parent, objc::sel::get("addSubview:"), child);
            mouse_up(child, sel, std::ptr::null_mut());
        }
        assert_eq!(SEEN.load(Ordering::Acquire), parent as usize);
    }

    /// Every responder subclass inherits it, which is the point of putting it on
    /// `NSResponder`: Qt sets a menu on `NSApp` and on its views, and the two are
    /// unrelated layouts.
    #[test]
    fn the_application_and_the_window_inherit_it() {
        crate::bootstrap();
        let class = objc::class::lookup(c"NSResponder").expect("NSResponder");
        let sel = objc::sel::get("setMenu:");
        for name in [c"NSApplication", c"NSWindow", c"NSView"] {
            let subclass = objc::class::lookup(name).expect("a responder subclass");
            assert!(
                subclass.chain().any(|c| std::ptr::eq(c, class)),
                "{name:?} is not a responder"
            );
            assert!(
                subclass.method(sel).is_some(),
                "{name:?} does not inherit -setMenu:"
            );
        }
    }
}
