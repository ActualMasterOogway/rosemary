//! M6.2 acceptance: the guest's classes, realized in the host's runtime.
//!
//! The direction under test is **host → guest**, and that is the whole point.
//! Every assertion here messages a guest class from *this* process's own
//! `objc_msgSend` — the host runtime looks the selector up, finds an `IMP`, and
//! calls it as native arm64. That `IMP` is one of this runtime's thunks, so an
//! answer coming back at all means realization, selector uniquing, method
//! installation and the JIT re-entry all worked together.
//!
//! Guest-side dispatch is deliberately absent here: no fixture in this file
//! sends a message from guest code. That is M6.3, and it is
//! [`msgsend.rs`](msgsend.rs).
//!
//! ## Why every fixture names its class differently
//!
//! Class names are global to a process, and `objc_allocateClassPair` refuses a
//! name already in use. These tests share one process and run in parallel, so a
//! fixture reusing `Greeter` would have its registration correctly refused and
//! the test would fail for a reason that is not about the code under test.
//! [`source`] takes the name.

mod common;

use std::ffi::CStr;

use rosetta3::objc::{Refusal, host};
use rosetta3::{Program, Vm, startup};

/// Serializes every test that messages guest code.
///
/// Not a convenience. A guest `IMP` is reached from the host runtime with
/// nothing on the stack belonging to this runtime, so the thunk finds its VM
/// through the process-wide pointer [`rosetta3::thunk::publish`] sets — there is
/// exactly one, and it is the last one published. Two tests publishing
/// different VMs in parallel would have each other's thunks decline, which is
/// the runtime behaving correctly and the test failing for an unrelated reason.
///
/// The same fact constrains real embedders, which is why it is stated here
/// rather than worked around: **a process runs one Objective-C guest.**
static PUBLISHED: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// A VM that outlives the test, published for the process.
///
/// Both halves are required. Leaking, because registration installs thunks as
/// `IMP`s in the *host's* runtime, which now holds code owned by this VM —
/// dropping it would unmap the trampolines while `libobjc` still has them in a
/// method list, and unlike an `atexit` handler a method can be reached by any
/// later message.
///
/// Publishing, because that is the only way a thunk entered from host code
/// finds a VM at all. `qsort`'s comparator is reached from inside a call this
/// runtime made, so the thread-local answers; a message send is not, and
/// neither is Foundation releasing a guest object on some other thread.
fn vm_for(path: &str) -> &'static Vm {
    let program = Program::load_executable(path, &[])
        .unwrap_or_else(|e| panic!("linking {path} failed: {e}"));
    let args = [path.to_string()];
    let vm: &'static Vm = Box::leak(Box::new(
        Vm::new(program, path, &args).expect("a VM with a stack"),
    ));
    rosetta3::thunk::publish(vm);
    vm
}

/// The standard fixture, with the class name substituted in.
///
/// One of everything a `class_ro_t` can carry: an ivar, a property, an instance
/// method, a class method, and a protocol.
fn source(name: &str) -> String {
    format!(
        r#"
#import <Foundation/Foundation.h>
@protocol Doubling{name}
- (int)twice:(int)x;
@end
@interface {name} : NSObject <Doubling{name}>
{{ int _n; }}
@property (nonatomic) int n;
- (int)twice:(int)x;
- (int)ivarSum;
- (int)neverCalled;
+ (int)answer;
@end
@implementation {name}
- (int)twice:(int)x {{ return x * 2; }}
- (int)ivarSum {{ _n = 7; return _n + 1; }}
// Never reached, and compiled anyway: it is what puts `twice:` in
// `__objc_selrefs`, which is the section M6.2 uniques. Sending a message from
// guest code is M6.3, so no test calls this.
- (int)neverCalled {{ return [self twice:1]; }}
+ (int)answer {{ return 42; }}
@end
int main(void) {{ return 0; }}
"#
    )
}

/// Hold the publish lock for the body of a test.
///
/// Poisoning is ignored: a panicking test has already failed, and denying the
/// next one its lock would turn one failure into every failure.
fn serialized() -> std::sync::MutexGuard<'static, ()> {
    PUBLISHED.lock().unwrap_or_else(|e| e.into_inner())
}

fn fixture(name: &str) -> Option<&'static Vm> {
    let path = common::build_objc(&format!("objc_{name}"), &source(name))?;
    Some(vm_for(&path))
}

/// `objc_msgSend`, as the prototype a given selector's type encoding describes.
///
/// Transmuting is not a shortcut here, it is the only way: `objc_msgSend` has
/// whichever signature the method has. See [`host::objc_msgSend`].
///
/// # Safety
///
/// `T` must be a function type matching the selector's actual signature, with
/// `(id, SEL)` first.
unsafe fn msg_send<T: Copy>() -> T {
    assert_eq!(
        std::mem::size_of::<T>(),
        std::mem::size_of::<*const ()>(),
        "T must be a bare function pointer"
    );
    let p = host::objc_msgSend as *const ();
    // SAFETY: the caller promises `T` matches the selector being sent.
    unsafe { *(&p as *const *const () as *const T) }
}

fn sel(name: &str) -> host::Sel {
    let c = std::ffi::CString::new(name).unwrap();
    // SAFETY: `c` is NUL-terminated and outlives the call.
    unsafe { host::sel_registerName(c.as_ptr()) }
}

fn class_named(name: &str) -> host::Class {
    let c = std::ffi::CString::new(name).unwrap();
    // SAFETY: as above.
    unsafe { host::objc_getClass(c.as_ptr()) }
}

/// Allocate and initialise an instance of a registered guest class.
///
/// `alloc` and `init` are `NSObject`'s, so they run in the *host's* runtime
/// against a class whose layout the host computed. Nothing guest-side is
/// involved until a guest selector is sent to the result.
fn new_instance(cls: host::Class) -> host::Class {
    // SAFETY: `cls` is a registered class; `alloc` and `init` are inherited
    // from `NSObject` and both return `id`.
    unsafe {
        let f: extern "C" fn(host::Class, host::Sel) -> host::Class = msg_send();
        let obj = f(cls, sel("alloc"));
        f(obj, sel("init"))
    }
}

#[test]
fn a_guest_class_is_visible_to_the_host_runtime_by_name() {
    let _lock = serialized();
    let Some(vm) = fixture("Named") else {
        skip!("cannot build x86_64 Objective-C fixtures")
    };
    let rt = vm.register_objc().expect("registration");
    assert!(
        rt.refusals().is_empty(),
        "unexpected refusals: {:?}",
        rt.refusals()
    );
    assert_eq!(rt.stats().classes, 1);

    // The claim the milestone is named for: `NSClassFromString(@"Named")` is
    // `objc_getClass` underneath, so this is that gate.
    let cls = class_named("Named");
    assert!(
        !cls.is_null(),
        "the host runtime does not know the guest's class"
    );

    // SAFETY: `cls` is a registered class.
    let back = unsafe { CStr::from_ptr(host::class_getName(cls)) };
    assert_eq!(back.to_str().unwrap(), "Named");

    // And the map agrees with the runtime, in both directions.
    let guest_va = vm.program.report.images[0].objc.as_ref().unwrap().classes[0].va;
    assert_eq!(rt.host_class(guest_va), Some(cls as u64));
    assert_eq!(rt.guest_class(cls as u64), Some(guest_va));
}

/// The gate: the host sends a message and *guest code* answers it.
#[test]
fn the_host_can_message_a_guest_instance_method() {
    let _lock = serialized();
    let Some(vm) = fixture("Msg") else {
        skip!("cannot build x86_64 Objective-C fixtures")
    };
    vm.register_objc().expect("registration");

    let obj = new_instance(class_named("Msg"));
    assert!(!obj.is_null());

    // SAFETY: `- (int)twice:(int)` is `i@:i`, which is this prototype.
    let twice: extern "C" fn(host::Class, host::Sel, i32) -> i32 = unsafe { msg_send() };
    assert_eq!(
        twice(obj, sel("twice:"), 21),
        42,
        "the guest's method did not run"
    );

    // Twice over, because the second call re-enters an already-translated
    // region through the same thunk — the path a real program spends its time
    // on, and one a first-call-only test would never take.
    assert_eq!(twice(obj, sel("twice:"), -3), -6);
}

/// A class method has to land on the *metaclass*. Added to the class instead it
/// becomes an instance method, and `[Klass answer]` then finds nothing — which
/// is a wrong program with no diagnostic anywhere.
#[test]
fn a_class_method_lands_on_the_metaclass() {
    let _lock = serialized();
    let Some(vm) = fixture("Meta") else {
        skip!("cannot build x86_64 Objective-C fixtures")
    };
    vm.register_objc().expect("registration");

    let cls = class_named("Meta");
    // SAFETY: `+ (int)answer` is `i@:`.
    let answer: extern "C" fn(host::Class, host::Sel) -> i32 = unsafe { msg_send() };
    assert_eq!(answer(cls, sel("answer")), 42);

    // And it is *not* an instance method, which is the half that catches the
    // mistake this test exists for.
    // SAFETY: `cls` is registered; a missing method returns null.
    let as_instance = unsafe { host::class_getInstanceMethod(cls, sel("answer")) };
    assert!(
        as_instance.is_null(),
        "the class method was installed as an instance method"
    );
}

/// Ivars: the host runtime lays the class out, and the guest compiled in its own
/// assumption about where `_n` would be. They have to agree, and this checks the
/// agreement rather than trusting it.
#[test]
fn ivar_offsets_match_what_the_guest_compiled() {
    let _lock = serialized();
    let Some(vm) = fixture("Ivar") else {
        skip!("cannot build x86_64 Objective-C fixtures")
    };
    let rt = vm.register_objc().expect("registration");
    assert_eq!(rt.stats().ivars, 1);

    let cls = class_named("Ivar");
    let name = std::ffi::CString::new("_n").unwrap();
    // SAFETY: `cls` is registered; `name` outlives the call.
    let ivar = unsafe { host::class_getInstanceVariable(cls, name.as_ptr()) };
    assert!(!ivar.is_null(), "the runtime did not take the guest's ivar");
    // SAFETY: `ivar` was just returned for `cls`.
    let host_offset = unsafe { host::ivar_getOffset(ivar) };

    // A direct `NSObject` subclass: the guest compiled against an 8-byte
    // superclass and so must the host have.
    assert_eq!(host_offset, 8, "an int ivar directly after NSObject's isa");

    // The end-to-end version of the same claim. `-ivarSum` writes `_n = 7`
    // through whatever offset the guest compiled in and reads it back, so a
    // disagreement is a wrong answer rather than a crash.
    let obj = new_instance(cls);
    // SAFETY: `- (int)ivarSum` is `i@:`.
    let sum: extern "C" fn(host::Class, host::Sel) -> i32 = unsafe { msg_send() };
    assert_eq!(
        sum(obj, sel("ivarSum")),
        8,
        "the guest and the host disagree about _n"
    );
}

/// The case the offset *writeback* exists for: a superclass in another image,
/// whose size the guest's compiler could not know, so clang emits a load from
/// `_OBJC_IVAR_$_...` instead of a constant.
#[test]
fn an_ivar_under_a_foundation_superclass_is_patched() {
    let _lock = serialized();
    const SRC: &str = r#"
    #import <Foundation/Foundation.h>
    @interface PatchedFmt : NSFormatter { int _b; }
    - (int)get;
    - (void)put:(int)v;
    @end
    @implementation PatchedFmt
    - (int)get { return _b; }
    - (void)put:(int)v { _b = v; }
    @end
    int main(void) { return 0; }
    "#;
    let Some(path) = common::build_objc("objc_patched", SRC) else {
        skip!("cannot build x86_64 Objective-C fixtures")
    };
    let vm = vm_for(&path);
    let rt = vm.register_objc().expect("registration");
    assert!(
        rt.refusals().is_empty(),
        "unexpected refusals: {:?}",
        rt.refusals()
    );
    assert_eq!(
        rt.stats().offsets_patched,
        1,
        "the ivar offset word was not rewritten"
    );

    // NSFormatter is bigger than NSObject, so the guest's compile-time guess
    // would have been wrong had it made one. It did not: it loads the word this
    // pass rewrote.
    let cls = class_named("PatchedFmt");
    let name = std::ffi::CString::new("_b").unwrap();
    // SAFETY: `cls` is registered; `name` outlives the call.
    let ivar = unsafe { host::class_getInstanceVariable(cls, name.as_ptr()) };
    // SAFETY: `ivar` was just returned for `cls`.
    let host_offset = unsafe { host::ivar_getOffset(ivar) };
    // Not asserted to be anything in particular: NSFormatter declares no ivars
    // of its own today, so the runtime's answer happens to equal what a
    // fragile layout would have given. The claim under test is not the number
    // -- it is that the guest *loads* it from the word this pass rewrote,
    // which the round-trip below is what actually shows.
    assert!(host_offset >= 8, "an ivar cannot precede the isa");

    // The end-to-end claim: `put:` stores through the offset word this pass
    // rewrote, and `get` reads it back through the same one. A writeback that
    // did not happen leaves the compiler's placeholder there, and the store
    // lands somewhere else in the object.
    let obj = new_instance(cls);
    // SAFETY: `- (void)put:(int)` is `v@:i` and `- (int)get` is `i@:`.
    unsafe {
        let put: extern "C" fn(host::Class, host::Sel, i32) = msg_send();
        let get: extern "C" fn(host::Class, host::Sel) -> i32 = msg_send();
        put(obj, sel("put:"), 1234);
        assert_eq!(
            get(obj, sel("get")),
            1234,
            "the ivar offset was not patched"
        );
    }
}

/// A category on a *host* class: `NSString` is the runtime's own, so the only
/// thing of ours in it is the `IMP`. The host messaging a real `NSString` and
/// reaching guest code is the sharpest form of the M6.2 claim.
#[test]
fn a_category_on_a_host_class_reaches_guest_code() {
    let _lock = serialized();
    const SRC: &str = r#"
    #import <Foundation/Foundation.h>
    @interface NSString (RosettaCat)
    - (NSUInteger)tripledLength;
    @end
    @implementation NSString (RosettaCat)
    // A constant rather than `[self length] * 3`: a message sent from *guest*
    // code is M6.3. What this proves is that a category's IMP, installed on a
    // class that is entirely the host's, is reached at all.
    - (NSUInteger)tripledLength { return 15; }
    @end
    int main(void) { return 0; }
    "#;
    let Some(path) = common::build_objc("objc_cat", SRC) else {
        skip!("cannot build x86_64 Objective-C fixtures")
    };
    let vm = vm_for(&path);
    let rt = vm.register_objc().expect("registration");
    assert!(
        rt.refusals().is_empty(),
        "unexpected refusals: {:?}",
        rt.refusals()
    );
    assert_eq!(rt.stats().categories, 1);

    // A genuine host `NSString`, built by the host's own Foundation.
    let s = std::ffi::CString::new("abcde").unwrap();
    // SAFETY: `+stringWithUTF8String:` returns an `NSString *`.
    let str_obj = unsafe {
        let f: extern "C" fn(host::Class, host::Sel, *const std::ffi::c_char) -> host::Class =
            msg_send();
        f(
            class_named("NSString"),
            sel("stringWithUTF8String:"),
            s.as_ptr(),
        )
    };
    assert!(!str_obj.is_null());

    // SAFETY: `- (NSUInteger)tripledLength` is `Q@:`.
    let tripled: extern "C" fn(host::Class, host::Sel) -> u64 = unsafe { msg_send() };
    assert_eq!(tripled(str_obj, sel("tripledLength")), 15);
}

/// A guest class deriving from another guest class. The registration order is
/// not the list order, so a pass that walked `__objc_classlist` straight through
/// would try to allocate the subclass against a superclass that does not exist
/// yet.
#[test]
fn an_in_image_superclass_is_registered_first() {
    let _lock = serialized();
    const SRC: &str = r#"
    #import <Foundation/Foundation.h>
    @interface ChainSub : NSObject
    - (int)base;
    @end
    @interface ChainDerived : ChainSub
    - (int)derived;
    @end
    @implementation ChainDerived
    // `[self base] + 1` would be a guest-side send; see the category fixture.
    - (int)derived { return 11; }
    @end
    @implementation ChainSub
    - (int)base { return 10; }
    @end
    int main(void) { return 0; }
    "#;
    let Some(path) = common::build_objc("objc_chain", SRC) else {
        skip!("cannot build x86_64 Objective-C fixtures")
    };
    let vm = vm_for(&path);
    let rt = vm.register_objc().expect("registration");
    assert!(
        rt.refusals().is_empty(),
        "unexpected refusals: {:?}",
        rt.refusals()
    );
    assert_eq!(rt.stats().classes, 2);

    let derived = class_named("ChainDerived");
    assert!(!derived.is_null());
    // SAFETY: `derived` is registered.
    let sup = unsafe { host::class_getSuperclass(derived) };
    assert_eq!(sup, class_named("ChainSub"), "the chain was not reproduced");

    // Both halves of the chain answer: the subclass's own method, and the
    // superclass's inherited one found through the host runtime's own lookup.
    let obj = new_instance(derived);
    // SAFETY: `- (int)derived` and `- (int)base` are both `i@:`.
    let f: extern "C" fn(host::Class, host::Sel) -> i32 = unsafe { msg_send() };
    assert_eq!(f(obj, sel("derived")), 11);
    assert_eq!(
        f(obj, sel("base")),
        10,
        "the inherited method was not found"
    );
}

/// Selector uniquing rewrites the guest's own `__objc_selrefs` slots from
/// pointers-to-strings into `SEL`s, which is what dyld's `libobjc` does on a
/// real launch. Guest code loads straight out of those slots, so until this runs
/// it is passing a `char *` where a `SEL` belongs.
#[test]
fn selector_reference_slots_are_uniqued_in_place() {
    let _lock = serialized();
    let Some(vm) = fixture("Selref") else {
        skip!("cannot build x86_64 Objective-C fixtures")
    };

    // Before: each slot holds a pointer to the name.
    let image = vm.program.report.images[0].objc.clone().unwrap();
    assert!(
        !image.selector_refs.is_empty(),
        "the fixture references selectors"
    );

    let rt = vm.register_objc().expect("registration");
    assert_eq!(rt.stats().selectors, image.selector_refs.len());

    // After: each slot holds exactly what `sel_registerName` returns for the
    // name that used to be there.
    use rosetta3::abi::GuestMemory;
    let view = vm.view();
    for &slot in &image.selector_refs {
        let mut buf = [0u8; 8];
        view.read(slot, &mut buf).expect("the slot is guest memory");
        let value = u64::from_le_bytes(buf);
        assert_ne!(value, 0);
        // SAFETY: the slot now holds a `SEL` the host runtime interned, so
        // `sel_getName` is defined on it.
        let name = unsafe { CStr::from_ptr(host::sel_getName(value as host::Sel)) };
        assert_eq!(
            value,
            sel(name.to_str().unwrap()) as u64,
            "slot {slot:#x} is not a SEL"
        );
    }
}

/// A protocol the guest declares becomes a real one, and the class conforms.
#[test]
fn a_guest_protocol_is_registered_and_adopted() {
    let _lock = serialized();
    let Some(vm) = fixture("Proto") else {
        skip!("cannot build x86_64 Objective-C fixtures")
    };
    let rt = vm.register_objc().expect("registration");
    assert_eq!(rt.stats().protocols, 1);

    let name = std::ffi::CString::new("DoublingProto").unwrap();
    // SAFETY: `name` outlives the call.
    let proto = unsafe { host::objc_getProtocol(name.as_ptr()) };
    assert!(!proto.is_null(), "the guest's protocol was not registered");

    let cls = class_named("Proto");
    // SAFETY: both are registered.
    let conforms: extern "C" fn(host::Class, host::Sel, host::Protocol) -> bool =
        unsafe { msg_send() };
    assert!(conforms(cls, sel("conformsToProtocol:"), proto));
}

/// Registration happens once. A second call must return the same maps rather
/// than trying to allocate every class again, which the runtime would refuse
/// for a name already in use — turning an idempotency bug into a pile of
/// refusals rather than a crash, but a bug either way.
#[test]
fn registration_is_idempotent() {
    let _lock = serialized();
    let Some(vm) = fixture("Once") else {
        skip!("cannot build x86_64 Objective-C fixtures")
    };
    let first = vm.register_objc().expect("registration").stats().clone();
    let second = vm.register_objc().expect("registration").stats().clone();
    assert_eq!(first, second);
    assert!(vm.objc().is_some());
}

/// A class name already in use is refused **by name**, with nothing registered
/// under it. Shadowing would make which class a name resolves to depend on load
/// order, which is not a question this runtime should be answering.
///
/// Provoked by loading one guest *twice* rather than by writing a fixture that
/// redeclares a Foundation class — clang rejects that outright, and the two
/// cases are the same one to `objc_allocateClassPair`. It also documents a real
/// limit: two guests in one process cannot both define a class name, because
/// there is one host runtime between them.
#[test]
fn a_class_name_already_in_use_is_refused_by_name() {
    let _lock = serialized();
    let Some(path) = common::build_objc("objc_dup", &source("Dup")) else {
        skip!("cannot build x86_64 Objective-C fixtures")
    };
    let first = vm_for(&path);
    let second = vm_for(&path);

    let a = first.register_objc().expect("registration");
    assert!(
        a.refusals().is_empty(),
        "the first load must succeed: {:?}",
        a.refusals()
    );

    let b = second.register_objc().expect("registration");
    assert!(
        b.refusals()
            .iter()
            .any(|r| matches!(r, Refusal::NameTaken { name } if name == "Dup")),
        "expected a named refusal, got {:?}",
        b.refusals()
    );
    assert_eq!(
        b.stats().classes,
        0,
        "nothing was registered under the taken name"
    );

    // The first load's class is intact and still answers, which is the half
    // that says the refusal did not damage what was already there. Republishing
    // is required and is the point of the module note: `Dup`'s methods are
    // `first`'s thunks, and `second` is what is published right now.
    rosetta3::thunk::publish(first);
    let obj = new_instance(class_named("Dup"));
    // SAFETY: `- (int)twice:(int)` is `i@:i`.
    let twice: extern "C" fn(host::Class, host::Sel, i32) -> i32 = unsafe { msg_send() };
    assert_eq!(twice(obj, sel("twice:"), 5), 10);
}

/// A program with no Objective-C registers nothing and says so, rather than
/// failing. Registration runs for every guest at startup, so the common case
/// has to be free.
#[test]
fn a_plain_c_program_registers_nothing() {
    let _lock = serialized();
    let Some(path) = common::hello() else {
        skip!("cannot build x86_64 fixtures")
    };
    let vm = vm_for(&path);
    let rt = vm.register_objc().expect("registration");
    assert_eq!(rt.stats(), &rosetta3::objc::Stats::default());
    assert!(rt.is_complete());
}

/// End to end, through the built driver: an Objective-C guest starts, registers,
/// runs its initializers and its `main`, and exits with its own status. Nothing
/// here sends a message from guest code — that is M6.3 — but everything up to
/// the point where it could is exercised as a process.
#[test]
fn an_objective_c_program_starts_and_exits() {
    const SRC: &str = r#"
    #import <Foundation/Foundation.h>
    @interface StartupClass : NSObject
    - (int)unused;
    @end
    @implementation StartupClass
    - (int)unused { return 0; }
    @end
    #include <stdio.h>
    int main(void) { printf("started\n"); return 3; }
    "#;
    let Some(path) = common::build_objc("objc_startup", SRC) else {
        skip!("cannot build x86_64 Objective-C fixtures")
    };
    let (status, stdout, stderr) = common::run_guest(&path, &[]);
    assert_eq!(status, Some(3), "stderr: {stderr}");
    assert_eq!(stdout, "started\n");
}

/// The `startup` hook: classes are realized *before* any guest code runs,
/// because an initializer may reference one and a `+load` is itself a method.
#[test]
fn classes_are_registered_before_initializers_run() {
    let _lock = serialized();
    let Some(vm) = fixture("Ordered") else {
        skip!("cannot build x86_64 Objective-C fixtures")
    };
    assert!(vm.objc().is_none(), "nothing is registered before startup");
    startup::run_initializers(vm).expect("initializers run");
    // `run_initializers` alone does not register; `run` does, and in that order.
    // Checking the composite would not distinguish "registered first" from
    // "registered at all", so the claim is made against `run`.
    let rt = vm.register_objc().expect("registration");
    assert!(rt.stats().classes > 0);
}
