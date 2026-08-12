//! `.cxx_construct`: the C++ members of a guest Objective-C class.
//!
//! An Objective-C class may have C++ ivars — a `std::mutex`, a `std::string`, a
//! `shared_ptr`. The compiler emits two methods for it, `.cxx_construct` and
//! `.cxx_destruct`, and sets a bit in the class's `class_ro_t` saying they
//! exist. The runtime reads that bit at realization and calls the constructor
//! from `class_createInstance`, superclass-first, on every instance.
//!
//! **A class this runtime builds gets neither.** [`super::register_class`]
//! creates it with `objc_allocateClassPair` and fills it with
//! `class_addMethod`, and neither of those sets the bit — `objc_allocateClassPair`
//! builds a `class_ro_t` with empty flags, and `class_addMethod` does not go back
//! and look at what it just added. The methods are installed, findable, and
//! never called. Verified rather than reasoned about: a native probe that builds
//! a class pair, adds both selectors and allocates an instance sees neither run.
//!
//! ## What that costs, and why it is not a leak
//!
//! The instance is `calloc`ed, so every C++ member is zero. For most of them
//! that is a bomb rather than a fault — a zeroed `std::string` reads as empty,
//! a zeroed `shared_ptr` as null — and the program runs until something
//! notices. Roblox notices with a `pthread_mutex_t`: a mutex whose signature
//! word is zero is not one libpthread recognises, `pthread_mutex_lock` answers
//! `EINVAL`, libc++ turns that into
//! `std::system_error: mutex lock failed: Invalid argument`, and the guest's
//! terminate handler ends the process four frames from anything that mentions a
//! class. `RobloxPlayerAppDelegate` is the class; twenty-odd others in the same
//! program have C++ ivars too.
//!
//! ## The repair, and why it is `+allocWithZone:`
//!
//! The bit cannot be set through public API, so the constructors are called from
//! somewhere this runtime *does* control: a native `+allocWithZone:` installed
//! on the class, which allocates through whatever would have allocated anyway
//! and then walks the chain.
//!
//! Allocation rather than `-init` is the load-bearing choice. **The allocation is
//! very often not the guest's**: AppKit instantiates the guest's view classes
//! out of a nib, `NSApplication` creates its delegate, and a category or a
//! framework may `[[cls alloc] init]` without the guest being on the stack at
//! all. Anything hung off `init` would miss every one of those. A method on the
//! class is found by the host's own dispatch no matter who sends it.
//!
//! The chain is walked **root-first**, which is the order `object_cxxConstructFromClass`
//! uses, and each class's `.cxx_construct` is looked up **non-inherited** — a
//! subclass that declares none must not run its superclass's twice. That is what
//! [`CONSTRUCTORS`] is: the lookup is by exact class, populated as classes
//! register, so no `class_copyMethodList` allocation happens on an allocation
//! path.
//!
//! ## What this does not do
//!
//! **`.cxx_destruct` is still never called.** The runtime calls it from
//! `object_dispose`, below `-[NSObject dealloc]`, which is the end of a chain
//! this runtime cannot get underneath without replacing `dealloc` on the guest's
//! own classes and taking over an ordering that ARC and the guest both have
//! opinions about. The consequence is a **leak** — a `std::string` ivar's buffer
//! is never freed — and a leak is what a constructed-but-not-destructed object
//! looks like, where the previous behaviour looked like a crash. It is the
//! right next piece of work and it is not this one.
//!
//! A guest class that declares `+allocWithZone:` *itself* is
//! [refused](Refusal::AlreadyDefined) rather than having its override replaced.

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::RwLock;

use super::host::{self, Class, Imp, Sel};

/// The selector the compiler emits for a class with C++ ivars.
pub const CONSTRUCT: &str = ".cxx_construct";
/// Its counterpart, named here so the gap has a name — see the module note on
/// why it is not called.
#[allow(dead_code)]
pub const DESTRUCT: &str = ".cxx_destruct";

/// Host `Class` → the `.cxx_construct` this runtime installed **on that exact
/// class**.
///
/// Not a per-`ObjcRuntime` map, and the difference matters: the entry is read
/// from a native `+alloc` that the *host* calls, on any thread, with no context
/// but the receiver. It is process-global for the same reason
/// [`crate::thunk::publish`] is.
///
/// `RwLock` rather than a published snapshot because writes stop early —
/// registration happens at load and at `dlopen` — and reads are one map lookup
/// per class in a chain, on a path that is already a `calloc`.
static CONSTRUCTORS: RwLock<Option<HashMap<usize, usize>>> = RwLock::new(None);

/// Why a class could not be given constructors.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// `class_addMethod` declined, which for a freshly built class means the
    /// guest declared `+allocWithZone:` itself. Nothing here can chain onto
    /// that without deciding what the guest's override is for.
    AlreadyDefined { class: String, selector: String },
}

/// Record that `cls` has a `.cxx_construct`, and give it somewhere to be called
/// from.
///
/// Called once per class, from registration, **after** the guest's own methods
/// are installed and before `objc_registerClassPair`. Returns the refusals so
/// they reach the same list every other registration refusal does.
///
/// # Safety
///
/// `cls` must be a class this runtime allocated and has not yet registered, and
/// `ctor` an `IMP` it installed for [`CONSTRUCT`] on that class.
pub unsafe fn install(cls: Class, ctor: Imp, name: &str) -> Vec<Refusal> {
    {
        let mut map = CONSTRUCTORS.write().unwrap_or_else(|e| e.into_inner());
        map.get_or_insert_with(HashMap::new)
            .insert(cls as usize, ctor as usize);
    }

    // **Only `+allocWithZone:`**, and `+alloc` follows from it rather than
    // needing its own. `NSObject`'s `+alloc` has a fast path that skips
    // `+allocWithZone:` entirely for a class that does not override it — and
    // `class_addMethod` clears exactly that optimisation when the method it
    // adds is this one. So adding the zone form is what makes `+alloc`, `+new`
    // and every framework's `[[cls alloc] init]` route through here; adding
    // `+alloc` as well would mean two entry points to keep agreeing about
    // whether the constructors have already run.
    let meta = {
        // SAFETY: `object_getClass` of a class is its metaclass.
        unsafe { host::object_getClass(cls) }
    };
    let Some(sel_name) = host::c_string(ALLOC_WITH_ZONE) else {
        return Vec::new();
    };
    let types = host::c_string(ALLOC_WITH_ZONE_TYPES).expect("a literal encoding has no NUL");
    // SAFETY: both strings outlive the call, and `alloc_with_zone` is a native
    // function of exactly the shape the encoding declares.
    let ok = unsafe {
        let sel = host::sel_registerName(sel_name.as_ptr());
        host::class_addMethod(meta, sel, alloc_with_zone as *const c_void as Imp, types.as_ptr())
    };
    match ok {
        true => Vec::new(),
        false => vec![Refusal::AlreadyDefined {
            class: name.to_string(),
            selector: ALLOC_WITH_ZONE.to_string(),
        }],
    }
}

/// The selector installed, and its type encoding: returns an object, takes
/// self, `_cmd` and a zone pointer.
const ALLOC_WITH_ZONE: &str = "allocWithZone:";
const ALLOC_WITH_ZONE_TYPES: &str = "@24@0:8^v16";

/// `+[cls allocWithZone:]`, for every guest class with C++ ivars.
///
/// Allocates through **whatever would have allocated if this were not here**,
/// then runs the constructors. Finding that allocator at call time rather than
/// recording it at install time is what makes a guest class inheriting from
/// `NSTextView` work: `NSTextView` overrides `+allocWithZone:`, and calling
/// `class_createInstance` instead would skip whatever it does.
///
/// It also makes a guest class inheriting from *another* guest class with C++
/// ivars work, which recording the superclass's `IMP` would not: the recorded
/// one would be this same function, and the chain would be constructed once per
/// level.
extern "C" fn alloc_with_zone(cls: Class, cmd: Sel, zone: *mut c_void) -> Class {
    // SAFETY: `cls` is the receiver of a class method, so it is a class.
    let Some(real) = (unsafe { inherited_allocator(cls) }) else {
        return std::ptr::null_mut();
    };
    // SAFETY: `real` is an `IMP` the runtime resolved for `allocWithZone:` on a
    // metaclass in this chain, so it has this shape, and it is not this
    // function.
    let obj = unsafe {
        let f: extern "C" fn(Class, Sel, *mut c_void) -> Class = std::mem::transmute(real);
        f(cls, cmd, zone)
    };
    if obj.is_null() {
        return obj;
    }
    // SAFETY: `obj` is a fresh instance of `cls`.
    unsafe { construct(cls, obj) };
    obj
}

/// The first `+allocWithZone:` at or above `cls` that is not
/// [`alloc_with_zone`] itself.
///
/// # Safety
///
/// `cls` must be a class.
unsafe fn inherited_allocator(cls: Class) -> Option<Imp> {
    let sel_name = host::c_string(ALLOC_WITH_ZONE)?;
    // SAFETY: the string outlives the call.
    let sel = unsafe { host::sel_registerName(sel_name.as_ptr()) };
    let ours = alloc_with_zone as *const c_void as Imp;
    // SAFETY: walking `class_getSuperclass` from a class reaches null, and
    // `object_getClass` of each is its metaclass.
    unsafe {
        let mut at = cls;
        while !at.is_null() {
            let imp = host::class_getMethodImplementation(host::object_getClass(at), sel);
            if !imp.is_null() && imp != ours {
                return Some(imp);
            }
            at = host::class_getSuperclass(at);
        }
    }
    // Unreachable in a process with Foundation: `NSObject` defines it.
    None
}

/// Call every `.cxx_construct` in `cls`'s chain, **root-first**.
///
/// The order is the runtime's own (`object_cxxConstructFromClass` recurses into
/// the superclass before calling its own) and it is not arbitrary: a subclass's
/// constructor may read a member the superclass constructed.
///
/// # Safety
///
/// `obj` must be a live instance of `cls`.
unsafe fn construct(cls: Class, obj: Class) {
    let Ok(guard) = CONSTRUCTORS.read() else {
        return;
    };
    let Some(map) = guard.as_ref() else {
        return;
    };

    // Root-first, so collected upward and walked back down. Eight is past any
    // real Cocoa hierarchy; a deeper one simply allocates.
    let mut chain: Vec<Class> = Vec::with_capacity(8);
    // SAFETY: walking `class_getSuperclass` from a class reaches null.
    unsafe {
        let mut at = cls;
        while !at.is_null() {
            chain.push(at);
            at = host::class_getSuperclass(at);
        }
    }

    let Some(sel_name) = host::c_string(CONSTRUCT) else {
        return;
    };
    // SAFETY: the string outlives the call.
    let sel = unsafe { host::sel_registerName(sel_name.as_ptr()) };

    for &at in chain.iter().rev() {
        let Some(&imp) = map.get(&(at as usize)) else {
            continue;
        };
        // SAFETY: `imp` is the `IMP` this runtime installed for
        // `.cxx_construct` on exactly this class — a thunk that enters guest
        // code with the method's own signature, which for `.cxx_construct` is
        // `id (*)(id, SEL)`. `obj` is an instance of a subclass of `at`, which
        // is what the runtime would pass.
        unsafe {
            let f: extern "C" fn(Class, Sel) -> Class = std::mem::transmute(imp);
            f(obj, sel);
        }
    }
}

/// Forget every class. Only for tests: the map outlives everything else.
#[cfg(test)]
fn reset() {
    *CONSTRUCTORS.write().unwrap() = None;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The claim the whole module rests on, checked against the runtime rather
    /// than believed: a class built the way this runtime builds one does **not**
    /// get its `.cxx_construct` called.
    ///
    /// If this ever starts failing, `objc4` has learned to notice a dynamically
    /// added structor and this module should be deleted rather than kept.
    #[test]
    fn the_runtime_does_not_call_a_dynamically_added_constructor() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static CALLS: AtomicUsize = AtomicUsize::new(0);
        extern "C" fn ctor(obj: Class, _cmd: Sel) -> Class {
            CALLS.fetch_add(1, Ordering::SeqCst);
            obj
        }

        let name = host::c_string("Rosetta3CxxProbe").expect("no NUL");
        let root = host::c_string("NSObject").expect("no NUL");
        // SAFETY: an ordinary class-pair construction, disposed below.
        unsafe {
            let ns = host::objc_getClass(root.as_ptr());
            assert!(!ns.is_null(), "no NSObject in this process");
            let cls = host::objc_allocateClassPair(ns, name.as_ptr(), 0);
            assert!(!cls.is_null(), "the probe class name is already taken");
            let sel_name = host::c_string(CONSTRUCT).expect("no NUL");
            let sel = host::sel_registerName(sel_name.as_ptr());
            let types = host::c_string("@16@0:8").expect("no NUL");
            assert!(host::class_addMethod(
                cls,
                sel,
                ctor as *const c_void as Imp,
                types.as_ptr()
            ));
            host::objc_registerClassPair(cls);
            let obj = host::class_createInstance(cls, 0);
            assert!(!obj.is_null());
            assert_eq!(
                CALLS.load(Ordering::SeqCst),
                0,
                "objc4 now calls a dynamically added .cxx_construct — delete this module"
            );
        }
    }

    /// And that this module's own path does call it, on an instance the *host*
    /// allocated through `+alloc` rather than one the test made — which is the
    /// case the module exists for.
    #[test]
    fn installing_makes_alloc_run_the_constructor() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static CALLS: AtomicUsize = AtomicUsize::new(0);
        extern "C" fn ctor(obj: Class, _cmd: Sel) -> Class {
            CALLS.fetch_add(1, Ordering::SeqCst);
            obj
        }

        let name = host::c_string("Rosetta3CxxProbe2").expect("no NUL");
        let root = host::c_string("NSObject").expect("no NUL");
        // SAFETY: as above.
        unsafe {
            let ns = host::objc_getClass(root.as_ptr());
            assert!(!ns.is_null(), "no NSObject in this process");
            let cls = host::objc_allocateClassPair(ns, name.as_ptr(), 0);
            assert!(!cls.is_null());
            let sel_name = host::c_string(CONSTRUCT).expect("no NUL");
            let sel = host::sel_registerName(sel_name.as_ptr());
            let types = host::c_string("@16@0:8").expect("no NUL");
            assert!(host::class_addMethod(
                cls,
                sel,
                ctor as *const c_void as Imp,
                types.as_ptr()
            ));
            let refusals = install(cls, ctor as *const c_void as Imp, "Rosetta3CxxProbe2");
            assert_eq!(refusals, Vec::new(), "the class declares no allocWithZone: of its own");
            host::objc_registerClassPair(cls);

            // Through `objc_msgSend`, which is how everyone else reaches it.
            let send: extern "C" fn(Class, Sel) -> Class =
                std::mem::transmute(host::objc_msgSend as *const c_void);
            let alloc_name = host::c_string("alloc").expect("no NUL");
            let obj = send(cls, host::sel_registerName(alloc_name.as_ptr()));
            assert!(!obj.is_null());
            assert_eq!(CALLS.load(Ordering::SeqCst), 1);
        }
        reset();
    }
}
