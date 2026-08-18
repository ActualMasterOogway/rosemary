//! `.cxx_construct`: the C++ members of a guest Objective-C class.

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

/// Host `Class` → the `.cxx_construct` this runtime installed **on that exact class**.
static CONSTRUCTORS: RwLock<Option<HashMap<usize, usize>>> = RwLock::new(None);

/// Why a class could not be given constructors.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// `class_addMethod` declined, which for a freshly built class means the guest
    /// declared `+allocWithZone:` itself.
    AlreadyDefined { class: String, selector: String },
}

/// Record that `cls` has a `.cxx_construct`, and give it somewhere to be called from.
///
/// # Safety
///
/// `cls` must be a class this runtime allocated and has not yet registered, and `ctor`
/// an `IMP` it installed for [`CONSTRUCT`] on that class.
pub unsafe fn install(cls: Class, ctor: Imp, name: &str) -> Vec<Refusal> {
    {
        let mut map = CONSTRUCTORS.write().unwrap_or_else(|e| e.into_inner());
        map.get_or_insert_with(HashMap::new)
            .insert(cls as usize, ctor as usize);
    }

    // **Only `+allocWithZone:`**, and `+alloc` follows from it rather than
    // needing its own.
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
        host::class_addMethod(
            meta,
            sel,
            alloc_with_zone as *const c_void as Imp,
            types.as_ptr(),
        )
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

    /// The claim the whole module rests on, checked against the runtime rather than
    /// believed:
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
            assert_eq!(
                refusals,
                Vec::new(),
                "the class declares no allocWithZone: of its own"
            );
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
