//! An Objective-C runtime.
//!
//! The guest's classes are Apple's objc4 ABI whatever host it runs on, and
//! `crates/rosetta3/src/objc` already knows how to replay them into a runtime
//! through libobjc's *public C API*. So this crate reimplements that API rather
//! than inventing one: everything above it then works unchanged against either
//! this runtime or Apple's, and can be tested against both on a Mac.
//!
//! Nothing here is `#[no_mangle]`. The C spellings exist only as strings in
//! [`symbol`], so this runtime can be linked into a process that already has
//! Apple's - which is how it gets tested at all.
//!
//! Two invariants the rest of the runtime depends on, neither of which the C API
//! spells out:
//!
//! * `class_getMethodImplementation` returns the resolved `IMP` itself, and its
//!   value changes exactly when the resolution does. `rosetta3`'s dispatch
//!   re-probes it on every cached send to notice swizzling; a stable
//!   per-`(class, sel)` forwarding trampoline would freeze the probe and keep
//!   stale plans alive.
//! * `method_getImplementation` returns the *exact* pointer `class_addMethod`
//!   was given. `rosetta3`'s dispatch compares it against its own thunks to tell
//!   a guest method from a host one; any wrapping turns every guest method into
//!   a host target and routes it through the encoding decoder, which is
//!   precisely what the guest path exists to avoid.
//!
//! Deliberate divergences from Apple's runtime, both of which simplify the
//! above: a selector is a pointer to its interned name, and there are no tagged
//! pointers, so `object_getClass` is always a load of the first word.

pub mod api;
pub mod arc;
pub mod class;
pub mod msg;
mod nsobject;
pub mod object;
pub mod protocol;
pub mod sel;
pub mod shorthand;

use std::sync::Once;

/// Install the classes this runtime owns. Idempotent, and cheap after the first.
///
/// `NSObject` is libobjc's on the system too, so a guest that subclasses it must
/// find it before any framework has loaded.
pub fn bootstrap() {
    static ONCE: Once = Once::new();
    ONCE.call_once(nsobject::install);
}

/// One entry point, as an address.
macro_rules! at {
    ($f:expr) => {
        $f as *const () as usize as u64
    };
}

/// The address `name` has in this runtime, or `None` if it defines no such
/// symbol.
///
/// `name` is the C identifier, without the leading underscore a Mach-O export
/// carries. The shape mirrors [`darwin::symbol`], and for the same reason: it is
/// what a `dlsym` against this runtime has to answer.
pub fn symbol(name: &str) -> Option<u64> {
    bootstrap();
    Some(match name {
        // selectors
        "sel_registerName" | "sel_getUid" => at!(api::sel_registerName),
        "sel_getName" => at!(api::sel_getName),
        "sel_isEqual" => at!(api::sel_isEqual),

        // classes
        "objc_getClass" => at!(api::objc_getClass),
        "objc_lookUpClass" => at!(api::objc_lookUpClass),
        "objc_getRequiredClass" => at!(api::objc_getRequiredClass),
        "objc_getMetaClass" => at!(api::objc_getMetaClass),
        "objc_allocateClassPair" => at!(api::objc_allocateClassPair),
        "objc_registerClassPair" => at!(api::objc_registerClassPair),
        "objc_disposeClassPair" => at!(api::objc_disposeClassPair),
        "class_getName" => at!(api::class_getName),
        "class_getSuperclass" => at!(api::class_getSuperclass),
        "class_isMetaClass" => at!(api::class_isMetaClass),
        "class_getInstanceSize" => at!(api::class_getInstanceSize),
        "class_addMethod" => at!(api::class_addMethod),
        "class_replaceMethod" => at!(api::class_replaceMethod),
        "class_addIvar" => at!(api::class_addIvar),
        "class_getInstanceVariable" => at!(api::class_getInstanceVariable),
        "class_getInstanceMethod" => at!(api::class_getInstanceMethod),
        "class_getClassMethod" => at!(api::class_getClassMethod),
        "class_getMethodImplementation" => at!(api::class_getMethodImplementation),
        "class_respondsToSelector" => at!(api::class_respondsToSelector),
        "class_addProtocol" => at!(api::class_addProtocol),
        "class_conformsToProtocol" => at!(api::class_conformsToProtocol),
        "class_createInstance" => at!(api::class_createInstance),

        // methods and ivars
        "method_getImplementation" => at!(api::method_getImplementation),
        "method_getTypeEncoding" => at!(api::method_getTypeEncoding),
        "method_getName" => at!(api::method_getName),
        "method_setImplementation" => at!(api::method_setImplementation),
        "ivar_getOffset" => at!(api::ivar_getOffset),
        "ivar_getName" => at!(api::ivar_getName),

        // objects
        "object_getClass" => at!(api::object_getClass),
        "object_setClass" => at!(api::object_setClass),
        "object_getClassName" => at!(api::object_getClassName),
        "object_getIndexedIvars" => at!(api::object_getIndexedIvars),
        "object_getIvar" => at!(api::object_getIvar),
        "object_setIvar" => at!(api::object_setIvar),
        "objc_constructInstance" => at!(api::objc_constructInstance),
        "objc_destructInstance" => at!(api::objc_destructInstance),

        // protocols
        "objc_getProtocol" => at!(api::objc_getProtocol),
        "objc_allocateProtocol" => at!(api::objc_allocateProtocol),
        "objc_registerProtocol" => at!(api::objc_registerProtocol),
        "protocol_getName" => at!(api::protocol_getName),
        "protocol_addProtocol" => at!(api::protocol_addProtocol),
        "protocol_addMethodDescription" => at!(api::protocol_addMethodDescription),
        "protocol_getMethodDescription" => at!(api::protocol_getMethodDescription),

        // reference counting
        "objc_retain" => at!(api::objc_retain),
        "objc_release" => at!(api::objc_release),
        "objc_autorelease" => at!(api::objc_autorelease),
        "objc_autoreleasePoolPush" => at!(api::objc_autoreleasePoolPush),
        "objc_autoreleasePoolPop" => at!(api::objc_autoreleasePoolPop),
        "objc_autoreleaseReturnValue" => at!(api::objc_autoreleaseReturnValue),
        "objc_retainAutorelease" => at!(api::objc_retainAutorelease),
        "objc_retainAutoreleaseReturnValue" => at!(api::objc_retainAutoreleaseReturnValue),
        "objc_retainAutoreleasedReturnValue" => at!(api::objc_retainAutoreleasedReturnValue),
        "objc_unsafeClaimAutoreleasedReturnValue" => {
            at!(api::objc_unsafeClaimAutoreleasedReturnValue)
        }
        "objc_storeStrong" => at!(api::objc_storeStrong),
        "objc_initWeak" => at!(api::objc_initWeak),
        "objc_storeWeak" => at!(api::objc_storeWeak),
        "objc_loadWeak" => at!(api::objc_loadWeak),
        "objc_loadWeakRetained" => at!(api::objc_loadWeakRetained),
        "objc_destroyWeak" => at!(api::objc_destroyWeak),
        "objc_copyWeak" => at!(api::objc_copyWeak),
        "objc_moveWeak" => at!(api::objc_moveWeak),
        "objc_sync_enter" => at!(api::objc_sync_enter),
        "objc_sync_exit" => at!(api::objc_sync_exit),
        "objc_setAssociatedObject" => at!(api::objc_setAssociatedObject),
        "objc_getAssociatedObject" => at!(api::objc_getAssociatedObject),

        // the folded messages
        "objc_alloc" => at!(shorthand::objc_alloc),
        "objc_allocWithZone" => at!(shorthand::objc_allocWithZone),
        "objc_alloc_init" => at!(shorthand::objc_alloc_init),
        "objc_opt_new" => at!(shorthand::objc_opt_new),
        "objc_opt_self" => at!(shorthand::objc_opt_self),
        "objc_opt_class" => at!(shorthand::objc_opt_class),
        "objc_opt_isKindOfClass" => at!(shorthand::objc_opt_isKindOfClass),
        "objc_opt_respondsToSelector" => at!(shorthand::objc_opt_respondsToSelector),
        "objc_getProperty" => at!(shorthand::objc_getProperty),
        "objc_setProperty" => at!(shorthand::objc_setProperty),
        "objc_setProperty_atomic" => at!(shorthand::objc_setProperty_atomic),
        "objc_setProperty_nonatomic" => at!(shorthand::objc_setProperty_nonatomic),
        "objc_setProperty_atomic_copy" => at!(shorthand::objc_setProperty_atomic_copy),
        "objc_setProperty_nonatomic_copy" => at!(shorthand::objc_setProperty_nonatomic_copy),
        "objc_retainBlock" => at!(shorthand::objc_retainBlock),
        "objc_enumerationMutation" => at!(shorthand::objc_enumerationMutation),
        "objc_terminate" => at!(shorthand::objc_terminate),

        // the send itself
        "objc_msgSend" => msg::entry() as usize as u64,

        _ => return None,
    })
}

/// The class objects this runtime exports as *data*.
///
/// A guest's `Greeter : NSObject` binds `_OBJC_CLASS_$_NSObject` and stores the
/// address it gets as its own superclass pointer, so the symbol has to be the
/// class itself and not a function that returns it.
///
/// Names as a Mach-O spells them, leading underscore and all.
pub fn class_data() -> Vec<(String, u64)> {
    bootstrap();
    let mut data = Vec::new();
    for name in ROOT_CLASSES {
        let Some(class) = std::ffi::CString::new(*name).ok().and_then(|n| class::lookup(&n)) else {
            continue;
        };
        data.push((
            format!("_OBJC_CLASS_$_{name}"),
            class::ptr(class) as usize as u64,
        ));
        data.push((
            format!("_OBJC_METACLASS_$_{name}"),
            class::ptr(class.metaclass()) as usize as u64,
        ));
    }
    data
}

/// The classes this crate owns, as opposed to a framework's.
///
/// One, and it is the one the system's libobjc owns too.
pub const ROOT_CLASSES: &[&str] = &["NSObject"];

/// Every name [`symbol`] answers, for [`provider`] and for the test below.
///
/// A second list rather than a generated one, because the match in [`symbol`] is
/// what a reader checks against the headers and a macro would hide it.
pub const NAMES: &[&str] = &[
    "sel_registerName", "sel_getUid", "sel_getName", "sel_isEqual",
    "objc_getClass", "objc_lookUpClass", "objc_getRequiredClass", "objc_getMetaClass",
    "objc_allocateClassPair", "objc_registerClassPair", "objc_disposeClassPair",
    "class_getName", "class_getSuperclass", "class_isMetaClass", "class_getInstanceSize",
    "class_addMethod", "class_replaceMethod", "class_addIvar", "class_getInstanceVariable",
    "class_getInstanceMethod", "class_getClassMethod", "class_getMethodImplementation",
    "class_respondsToSelector", "class_addProtocol", "class_conformsToProtocol",
    "class_createInstance",
    "method_getImplementation", "method_getTypeEncoding", "method_getName",
    "method_setImplementation", "ivar_getOffset", "ivar_getName",
    "object_getClass", "object_setClass", "object_getClassName", "object_getIndexedIvars",
    "object_getIvar", "object_setIvar", "objc_constructInstance", "objc_destructInstance",
    "objc_getProtocol", "objc_allocateProtocol", "objc_registerProtocol", "protocol_getName",
    "protocol_addProtocol", "protocol_addMethodDescription", "protocol_getMethodDescription",
    "objc_retain", "objc_release", "objc_autorelease", "objc_autoreleasePoolPush",
    "objc_autoreleasePoolPop", "objc_autoreleaseReturnValue", "objc_retainAutorelease",
    "objc_retainAutoreleaseReturnValue", "objc_retainAutoreleasedReturnValue",
    "objc_unsafeClaimAutoreleasedReturnValue", "objc_storeStrong", "objc_initWeak",
    "objc_storeWeak", "objc_loadWeak", "objc_loadWeakRetained", "objc_destroyWeak",
    "objc_copyWeak", "objc_moveWeak", "objc_sync_enter", "objc_sync_exit",
    "objc_setAssociatedObject", "objc_getAssociatedObject",
    "objc_alloc", "objc_allocWithZone", "objc_alloc_init", "objc_opt_new", "objc_opt_self",
    "objc_opt_class", "objc_opt_isKindOfClass", "objc_opt_respondsToSelector",
    "objc_getProperty", "objc_setProperty", "objc_setProperty_atomic",
    "objc_setProperty_nonatomic", "objc_setProperty_atomic_copy",
    "objc_setProperty_nonatomic_copy", "objc_retainBlock", "objc_enumerationMutation",
    "objc_terminate",
    "objc_msgSend",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_listed_name_resolves() {
        let missing: Vec<&str> = NAMES.iter().copied().filter(|n| symbol(n).is_none()).collect();
        assert!(missing.is_empty(), "listed but unresolved: {missing:?}");
    }

    #[test]
    fn nsobject_is_installed_and_its_metaclass_chain_terminates() {
        bootstrap();
        let nsobject = class::lookup(c"NSObject").expect("NSObject is this runtime's own");
        assert!(nsobject.superclass().is_none(), "NSObject is a root class");
        let meta = nsobject.metaclass();
        assert!(meta.is_meta());
        // The root metaclass's superclass is the root class, so `+alloc` and
        // `-class` are both findable from a class object.
        assert!(std::ptr::eq(meta.superclass().unwrap(), nsobject));
        // And its own `isa` is itself, which is what stops the walk.
        assert!(std::ptr::eq(meta.metaclass(), meta));
    }
}

/// The two invariants `rosetta3`'s dispatch depends on, as tests.
///
/// Neither is stated by the C API, and breaking either produces a wrong `IMP`
/// months later rather than a failure here. See the crate doc.
#[cfg(test)]
mod invariants {
    use std::ffi::c_void;

    use super::*;
    use crate::object::Id;
    use crate::sel::Sel;

    unsafe extern "C" fn first(_receiver: Id, _sel: Sel) -> u64 {
        1
    }
    unsafe extern "C" fn second(_receiver: Id, _sel: Sel) -> u64 {
        2
    }

    /// A class of this name, freshly allocated. Each test gets its own, because
    /// class names are global and the tests share a process.
    fn class_named(name: &std::ffi::CStr) -> &'static class::Class {
        bootstrap();
        let nsobject = class::lookup(c"NSObject").expect("NSObject");
        let class = class::allocate_pair(Some(nsobject), name, 0).expect("a fresh name");
        class::register_pair(class);
        class
    }

    #[test]
    fn method_get_implementation_returns_the_pointer_that_was_installed() {
        let class = class_named(c"Rosetta3InvariantA");
        let sel = sel::get("answer");
        let installed = first as *const c_void;
        assert!(class.add_method(sel, installed, c"Q16@0:8"));

        let method = class.own_method(sel).expect("just added");
        assert_eq!(
            method.implementation(),
            installed,
            "an IMP that comes back wrapped makes every guest method look like a host one"
        );

        // And still exactly the pointer after a replacement.
        let replaced = second as *const c_void;
        assert_eq!(method.set_implementation(replaced), installed);
        assert_eq!(method.implementation(), replaced);
    }

    #[test]
    fn the_method_implementation_probe_moves_when_the_resolution_does() {
        let class = class_named(c"Rosetta3InvariantB");
        let sel = sel::get("answer");
        assert!(class.add_method(sel, first as *const c_void, c"Q16@0:8"));

        let before = class::method_implementation(class, sel);
        assert_eq!(before, first as *const c_void);

        // Swizzled. A cached plan keyed on the old probe must now miss.
        class
            .own_method(sel)
            .unwrap()
            .set_implementation(second as *const c_void);
        let after = class::method_implementation(class, sel);
        assert_ne!(
            before, after,
            "the probe did not move, so a cached dispatch plan would stay stale"
        );
        assert_eq!(after, second as *const c_void);
    }

    /// A subclass overriding a superclass's method moves the probe too, which is
    /// the same invariant reached by the other route: resolution, not storage.
    #[test]
    fn an_override_added_after_the_fact_moves_the_probe() {
        let base = class_named(c"Rosetta3InvariantC");
        let sel = sel::get("answer");
        assert!(base.add_method(sel, first as *const c_void, c"Q16@0:8"));
        let derived = class::allocate_pair(Some(base), c"Rosetta3InvariantCSub", 0).unwrap();
        class::register_pair(derived);

        assert_eq!(
            class::method_implementation(derived, sel),
            first as *const c_void,
            "inherited before the override"
        );
        assert!(derived.add_method(sel, second as *const c_void, c"Q16@0:8"));
        assert_eq!(
            class::method_implementation(derived, sel),
            second as *const c_void
        );
    }

    /// `+alloc`, `-init` and `-isKindOfClass:` on a subclass of `NSObject`,
    /// through the real `objc_msgSend`.
    ///
    /// The one test that exercises the naked trampoline rather than calling an
    /// `IMP` directly.
    #[test]
    fn a_message_reaches_an_inherited_implementation_through_msg_send() {
        let class = class_named(c"Rosetta3InvariantD");
        // SAFETY: `+alloc` and `-init` are `id (*)(id, SEL)`, and
        // `-isKindOfClass:` is `BOOL (*)(id, SEL, Class)`.
        unsafe {
            let send: extern "C" fn(Id, Sel) -> Id = std::mem::transmute(msg::entry());
            let object = send(class::ptr(class) as Id, sel::get("alloc"));
            assert!(!object.is_null(), "+alloc came back nil");
            let object = send(object, sel::get("init"));

            let is_kind: extern "C" fn(Id, Sel, Id) -> bool = std::mem::transmute(msg::entry());
            assert!(is_kind(object, sel::get("isKindOfClass:"), class::ptr(class) as Id));
            let nsobject = class::lookup(c"NSObject").unwrap();
            assert!(is_kind(
                object,
                sel::get("isKindOfClass:"),
                class::ptr(nsobject) as Id
            ));

            let release: extern "C" fn(Id, Sel) = std::mem::transmute(msg::entry());
            release(object, sel::get("release"));
        }
    }

    /// Messaging nil is the language, not an error.
    #[test]
    fn a_message_to_nil_returns_zero() {
        bootstrap();
        // SAFETY: the nil path never reaches an IMP, so any prototype is safe.
        unsafe {
            let send: extern "C" fn(Id, Sel) -> u64 = std::mem::transmute(msg::entry());
            assert_eq!(send(std::ptr::null_mut(), sel::get("answer")), 0);
            let send_f: extern "C" fn(Id, Sel) -> f64 = std::mem::transmute(msg::entry());
            assert_eq!(send_f(std::ptr::null_mut(), sel::get("answer")), 0.0);
        }
    }
}
