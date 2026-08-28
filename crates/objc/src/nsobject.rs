//! `NSObject`.
//!
//! Here rather than in a Foundation crate because that is where it is on the
//! system too: `libobjc.A.dylib` exports `_OBJC_CLASS_$_NSObject`, and a guest
//! whose classes descend from it must find it before any framework has loaded.
//!
//! Only the root behaviour: allocation, the reference-counting messages, and the
//! introspection every other class inherits. Anything that needs a string, a
//! description or a hash table is Foundation's.

use std::ffi::{CStr, c_void};

use crate::class::{Class, Imp};
use crate::object::Id;
use crate::sel::Sel;

/// Install `NSObject` and its metaclass. Idempotent; called from [`crate::bootstrap`].
pub(crate) fn install() {
    let name = c"NSObject";
    let Some(class) = crate::class::allocate_pair(None, name, 0) else {
        return;
    };
    let meta = class.metaclass();

    // Class methods, on the metaclass. `+alloc` and `+new` are the reason the
    // root metaclass's superclass is the root class: `[NSObject class]` has to
    // resolve from either side.
    add(meta, "alloc", alloc as Imp, c"@16@0:8");
    add(meta, "allocWithZone:", alloc_with_zone as Imp, c"@24@0:8^v16");
    add(meta, "new", new as Imp, c"@16@0:8");
    add(meta, "class", class_of_class as Imp, c"#16@0:8");
    add(meta, "superclass", superclass as Imp, c"#16@0:8");
    add(meta, "instancesRespondToSelector:", instances_respond as Imp, c"B24@0:8:16");
    add(meta, "conformsToProtocol:", conforms as Imp, c"B24@0:8@16");
    add(meta, "initialize", initialize as Imp, c"v16@0:8");
    add(meta, "load", initialize as Imp, c"v16@0:8");

    add(class, "init", init as Imp, c"@16@0:8");
    add(class, "dealloc", dealloc as Imp, c"v16@0:8");
    add(class, "retain", retain as Imp, c"@16@0:8");
    add(class, "release", release as Imp, c"v16@0:8");
    add(class, "autorelease", autorelease as Imp, c"@16@0:8");
    add(class, "retainCount", retain_count as Imp, c"Q16@0:8");
    add(class, "self", itself as Imp, c"@16@0:8");
    add(class, "class", class_of as Imp, c"#16@0:8");
    add(class, "superclass", instance_superclass as Imp, c"#16@0:8");
    add(class, "isEqual:", is_equal as Imp, c"B24@0:8@16");
    add(class, "hash", hash as Imp, c"Q16@0:8");
    add(class, "isKindOfClass:", is_kind_of as Imp, c"B24@0:8#16");
    add(class, "isMemberOfClass:", is_member_of as Imp, c"B24@0:8#16");
    add(class, "respondsToSelector:", responds as Imp, c"B24@0:8:16");
    add(class, "conformsToProtocol:", instance_conforms as Imp, c"B24@0:8@16");
    add(class, "doesNotRecognizeSelector:", does_not_recognize as Imp, c"v24@0:8:16");

    crate::class::register_pair(class);
}

fn add(class: &'static Class, sel: &str, imp: Imp, types: &CStr) {
    class.add_method(crate::sel::get(sel), imp, types);
}

/// The class a message was sent to, whichever side of the pair it arrived on.
///
/// # Safety
///
/// `receiver` must be a class pointer.
unsafe fn as_class(receiver: Id) -> &'static Class {
    // SAFETY: the caller's obligation.
    unsafe { crate::class::from_ptr(receiver as *const Class) }
        .expect("a class method's receiver is a class")
}

/// Send `receiver` a message taking one word and returning an object.
///
/// Resolved rather than called directly, which is the whole point: `+alloc` has
/// to reach a subclass's `+allocWithZone:`, and a category that replaces one of
/// these has to be reached too.
///
/// # Safety
///
/// `receiver` must be a live object, and the method must be
/// `id (*)(id, SEL, void *)`.
unsafe fn send1(receiver: Id, sel: Sel, argument: *mut c_void) -> Id {
    // SAFETY: the caller's obligation.
    let Some(method) = (unsafe { crate::object::class_of(receiver) }).and_then(|c| c.method(sel))
    else {
        return std::ptr::null_mut();
    };
    // SAFETY: the caller's obligation.
    let f: unsafe extern "C" fn(Id, Sel, *mut c_void) -> Id =
        unsafe { std::mem::transmute(method.implementation()) };
    // SAFETY: as above.
    unsafe { f(receiver, sel, argument) }
}

/// `+alloc` is `[self allocWithZone:nil]`, and not a shortcut to the allocator.
///
/// A class that overrides `+allocWithZone:` - which is how a C++ ivar
/// constructor gets to run, see `rosetta3`'s `objc::cxxivars` - must be the one
/// that allocates.
unsafe extern "C" fn alloc(receiver: Id, _sel: Sel) -> Id {
    // SAFETY: `+alloc`'s receiver is the class, and `+allocWithZone:` takes one
    // pointer and returns an object.
    unsafe {
        send1(
            receiver,
            crate::sel::get("allocWithZone:"),
            std::ptr::null_mut(),
        )
    }
}

unsafe extern "C" fn alloc_with_zone(receiver: Id, _sel: Sel, _zone: *mut c_void) -> Id {
    // SAFETY: as `alloc`; the zone has meant nothing for twenty years.
    crate::object::create_instance(unsafe { as_class(receiver) }, 0)
}

/// `+new` is `[[self alloc] init]`, both of them resolved.
unsafe extern "C" fn new(receiver: Id, _sel: Sel) -> Id {
    // SAFETY: the receiver is a class.
    let object = unsafe { send1(receiver, crate::sel::get("alloc"), std::ptr::null_mut()) };
    if object.is_null() {
        return object;
    }
    // SAFETY: a freshly allocated object, and `-init` is `id (*)(id, SEL)`.
    unsafe { send1(object, crate::sel::get("init"), std::ptr::null_mut()) }
}

unsafe extern "C" fn init(receiver: Id, _sel: Sel) -> Id {
    receiver
}

unsafe extern "C" fn dealloc(receiver: Id, _sel: Sel) {
    crate::arc::finalize(receiver);
}

unsafe extern "C" fn retain(receiver: Id, _sel: Sel) -> Id {
    crate::arc::retain(receiver)
}

unsafe extern "C" fn release(receiver: Id, _sel: Sel) {
    crate::arc::release(receiver);
}

unsafe extern "C" fn autorelease(receiver: Id, _sel: Sel) -> Id {
    crate::arc::autorelease(receiver)
}

unsafe extern "C" fn retain_count(receiver: Id, _sel: Sel) -> usize {
    crate::arc::retain_count(receiver)
}

unsafe extern "C" fn itself(receiver: Id, _sel: Sel) -> Id {
    receiver
}

/// `-class` is the class; `+class` is the class itself, not the metaclass.
unsafe extern "C" fn class_of(receiver: Id, _sel: Sel) -> Id {
    // SAFETY: an instance.
    unsafe { crate::object::class_of(receiver) }
        .map_or(std::ptr::null_mut(), |c| crate::class::ptr(c) as Id)
}

unsafe extern "C" fn class_of_class(receiver: Id, _sel: Sel) -> Id {
    receiver
}

unsafe extern "C" fn superclass(receiver: Id, _sel: Sel) -> Id {
    // SAFETY: a class.
    unsafe { as_class(receiver) }
        .superclass()
        .map_or(std::ptr::null_mut(), |c| crate::class::ptr(c) as Id)
}

unsafe extern "C" fn instance_superclass(receiver: Id, _sel: Sel) -> Id {
    // SAFETY: an instance.
    unsafe { crate::object::class_of(receiver) }
        .and_then(|c| c.superclass())
        .map_or(std::ptr::null_mut(), |c| crate::class::ptr(c) as Id)
}

unsafe extern "C" fn is_equal(receiver: Id, _sel: Sel, other: Id) -> bool {
    receiver == other
}

unsafe extern "C" fn hash(receiver: Id, _sel: Sel) -> usize {
    receiver as usize
}

unsafe extern "C" fn is_kind_of(receiver: Id, _sel: Sel, class: Id) -> bool {
    // SAFETY: an instance and a class.
    let Some(mine) = (unsafe { crate::object::class_of(receiver) }) else {
        return false;
    };
    // SAFETY: a class pointer this runtime produced, or null.
    let Some(theirs) = (unsafe { crate::class::from_ptr(class as *const Class) }) else {
        return false;
    };
    mine.chain().any(|c| std::ptr::eq(c, theirs))
}

unsafe extern "C" fn is_member_of(receiver: Id, _sel: Sel, class: Id) -> bool {
    // SAFETY: an instance.
    unsafe { crate::object::class_of(receiver) }
        .is_some_and(|c| std::ptr::eq(c, class as *const Class))
}

unsafe extern "C" fn responds(receiver: Id, _sel: Sel, selector: Sel) -> bool {
    // SAFETY: an instance.
    unsafe { crate::object::class_of(receiver) }.is_some_and(|c| c.method(selector).is_some())
}

unsafe extern "C" fn instances_respond(receiver: Id, _sel: Sel, selector: Sel) -> bool {
    // SAFETY: a class.
    unsafe { as_class(receiver) }.method(selector).is_some()
}

unsafe extern "C" fn conforms(receiver: Id, _sel: Sel, protocol: Id) -> bool {
    // SAFETY: a class and a protocol.
    let class = unsafe { as_class(receiver) };
    protocol_of(protocol).is_some_and(|p| class.conforms_to(p))
}

unsafe extern "C" fn instance_conforms(receiver: Id, _sel: Sel, protocol: Id) -> bool {
    // SAFETY: an instance.
    let Some(class) = (unsafe { crate::object::class_of(receiver) }) else {
        return false;
    };
    protocol_of(protocol).is_some_and(|p| class.conforms_to(p))
}

fn protocol_of(protocol: Id) -> Option<&'static crate::protocol::Protocol> {
    // SAFETY: a `Protocol *` this runtime produced, or null.
    (!protocol.is_null()).then(|| unsafe { &*(protocol as *const crate::protocol::Protocol) })
}

unsafe extern "C" fn does_not_recognize(receiver: Id, _sel: Sel, selector: Sel) {
    // SAFETY: an instance.
    let class = unsafe { crate::object::class_of(receiver) };
    let name = class.map_or("nil", |c| c.name_str());
    let selector = match selector.is_null() {
        true => std::borrow::Cow::Borrowed("<null selector>"),
        // SAFETY: a registered selector is its own NUL-terminated name.
        false => unsafe { CStr::from_ptr(selector) }.to_string_lossy(),
    };
    panic!("unrecognized selector sent to instance of {name}: {selector}");
}

/// `+initialize` and `+load`, which `NSObject` answers by doing nothing.
unsafe extern "C" fn initialize(_receiver: Id, _sel: Sel) {}
