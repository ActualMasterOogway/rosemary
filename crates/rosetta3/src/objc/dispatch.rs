//! Messages sent *by* the guest: `objc_msgSend`, and the runtime it calls.
//!
//! [M6.2](../../../docs/ROADMAP.md) made a guest class real in the host's
//! runtime, so the host can message a guest object. This is the other direction,
//! and it is the one an Objective-C program spends its life in: until it exists,
//! a Cocoa guest links cleanly and stops at its first message.
//!
//! ## Three cases, and only one of them is marshalling
//!
//! | Message | Mechanism |
//! |---|---|
//! | guest → guest | resolve the `IMP`, set `rip` to it |
//! | guest → host | look up the method's type encoding, marshal by it |
//! | host → guest | the host runtime calls the registered thunk (M6.2) |
//!
//! The first row is the pleasant one and it is also the common one: a message to
//! a method the guest itself implements needs **no ABI knowledge whatsoever**.
//! `objc_msgSend` is a tail-call trampoline — it does not build a frame, it jumps
//! to the `IMP` with the registers exactly as its caller left them — so
//! reproducing it means setting `rip` and nothing else. The guest's arguments are
//! already the callee's arguments, in the guest's own convention, and neither
//! side has to describe them.
//!
//! The second row is where the ABI comes due, and M6.1 found the thing that makes
//! it answerable: a method carries its own type encoding. `objc_msgSend` has no
//! signature — it has *whichever* signature the method has — so the alternative
//! is a guess, and a guessed signature here does not fault, it returns plausible
//! garbage. See [`super::encoding`].
//!
//! ## Which pointer means what
//!
//! A guest names its own class with `leaq _OBJC_CLASS_$_Greeter(%rip), %rdi` — a
//! direct reference to the dead `objc_class` in its own `__objc_data`, with no
//! slot anywhere to redirect at load time. The class the host runtime realized is
//! a *different object*. So the translation happens here, where the pointer
//! crosses: a class going out becomes the host's, a class coming back becomes the
//! guest's. Both directions are needed and the second is the one that is easy to
//! forget — a guest comparing `[obj class]` against `Greeter` must find them
//! equal, and it can only do that if what it gets back is the address it already
//! has.
//!
//! Nothing else needs translating. Objects are allocated by the host runtime and
//! are host pointers on both sides; selectors are uniqued into the host's table
//! by M6.2's rewrite of `__objc_selrefs`, so a guest `SEL` *is* a host `SEL`.
//!
//! ## Where the return address comes from
//!
//! Standing in for the trampoline means standing in for the transfer, and the
//! guest's stack has to end up exactly as the real `objc_msgSend` would have left
//! it. Microlift intercepts a `call` **before** it pushes a return address, and a
//! compiler tail-jumps to `objc_msgSend` freely (`- (id)a { return [self b]; }`),
//! where the return address is already on the stack. The two need opposite
//! treatment and the difference is invisible in the register context, so the loop
//! states which happened — see [`crate::thread::ThreadState::entered_by_jump`].
//!
//! ## Scope
//!
//! Refused by name, each because of something specific rather than for want of
//! effort:
//!
//! - **`objc_msgSend_stret` / `_fpret`**, and any method whose encoding passes or
//!   returns an aggregate: M2's "aggregates by value" refusal, arriving where the
//!   roadmap said it would.
//! - **Variadic methods** (`stringWithFormat:` and the rest). The encoding does
//!   not record the ellipsis, and the two platforms disagree about where variadic
//!   arguments live — SysV puts them in registers, Apple's arm64 ABI on the
//!   stack. Passing the fixed arguments and hoping is exactly the silent wrong
//!   answer this project refuses, so the ones a program actually reaches are
//!   named. The list is necessarily incomplete: see [`VARIADIC_SELECTORS`].
//! - **Forwarding.** A selector the receiver's class does not implement would go
//!   to `forwardInvocation:`, which builds an `NSInvocation` describing a frame
//!   this runtime does not have.
//! - **`objc_exception_throw`**, which is [M5's unwinder](crate::unwind) with a
//!   different personality routine.
//! - **Anything not in [`RUNTIME`]**, which is a list of what programs import
//!   rather than of everything `objc/runtime.h` declares. A guest importing
//!   something else fails by name at the call, which is a work item that closes.

use machish::resolve::SyntheticDylib;

use microlift::arch::x86_64::context::RegisterContext;

use crate::abi::aggregate::{GuestClass, Shape};
use crate::abi::marshal::{self, Convert};
use crate::abi::{AbiError, ArgType as T, GuestMemory, Result};
use crate::vm::Vm;

use super::encoding::{self, Signature};
use super::host;

/// A runtime entry point the guest may call, and what crosses at it.
///
/// Every one of these is forwarded to the host's own `libobjc`: the runtime is
/// the host's, and a guest asking it a question should get the host's answer.
/// What the entry needs from a table is the two things bits cannot say — the
/// signature, because a Mach-O records no types, and which of the pointers are
/// classes.
struct Runtime {
    /// The C name, as `dlsym` spells it: no leading underscore.
    name: &'static str,
    /// `[0]` is the return type, `[1..]` the parameters, as everywhere else.
    types: &'static [T],
    /// Zero-based parameter indices holding a `Class`.
    ///
    /// An `id` parameter that a program plausibly passes a class to is marked
    /// too — `objc_retain(Greeter)` is legal Objective-C — and marking one
    /// wrongly costs nothing: the translation is a map lookup that leaves
    /// anything but a registered guest class alone.
    classes: &'static [u8],
    /// Whether the return value is a `Class`.
    class_return: bool,
    /// Zero-based parameter indices that are **guest function pointers**, in
    /// exactly the sense [`crate::abi::FunctionAbi::callbacks`] means.
    ///
    /// One entry point needs it. `method_setImplementation` installs an `IMP`
    /// the host runtime will call as arm64 whenever anyone sends that message —
    /// including Foundation, on a thread this runtime never made — so a guest
    /// swizzling a method has to leave a thunk behind rather than its own
    /// address. It is the same substitution `pthread_create`'s start routine
    /// gets, arriving through a different door.
    callbacks: &'static [u8],
}

/// `(ret, args...)`, with no class anywhere.
macro_rules! rt {
    ($name:literal, $ret:expr $(, $arg:expr)* $(,)?) => {
        Runtime {
            name: $name,
            types: &[$ret $(, $arg)*],
            classes: &[],
            class_return: false,
            callbacks: &[],
        }
    };
}

/// As [`rt`], naming the parameters that are classes and whether the return is.
macro_rules! rtc {
    ([$($c:literal),* $(,)?] $(-> $ret_is_class:literal)?, $name:literal, $ret:expr $(, $arg:expr)* $(,)?) => {
        Runtime {
            name: $name,
            types: &[$ret $(, $arg)*],
            classes: &[$($c),*],
            class_return: false $(|| $ret_is_class)?,
            callbacks: &[],
        }
    };
}

/// As [`rt`], naming the parameters that are guest function pointers.
macro_rules! rtb {
    ([$($b:literal),* $(,)?], $name:literal, $ret:expr $(, $arg:expr)* $(,)?) => {
        Runtime {
            name: $name,
            types: &[$ret $(, $arg)*],
            classes: &[],
            class_return: false,
            callbacks: &[$($b),*],
        }
    };
}

/// As [`rtc`], also naming the parameters that are guest function pointers.
///
/// One entry needs both, and it is the one that creates a class at runtime:
/// `class_addMethod` takes the class it is adding to *and* the `IMP` it is
/// adding, which is guest code the host runtime will call as arm64.
macro_rules! rtcb {
    ([$($c:literal),* $(,)?], [$($b:literal),* $(,)?], $name:literal, $ret:expr $(, $arg:expr)* $(,)?) => {
        Runtime {
            name: $name,
            types: &[$ret $(, $arg)*],
            classes: &[$($c),*],
            class_return: false,
            callbacks: &[$($b),*],
        }
    };
}

const V: T = T::None;
const I32: T = T::SignedWord;
const I64: T = T::SignedDoubleword;
const U64: T = T::UnsignedDoubleword;
const U8: T = T::UnsignedByte;
const P: T = T::Pointer;

/// The runtime surface a guest reaches, **sorted by name**.
///
/// Not all of `objc/runtime.h`: the same posture as [`crate::abi::table`], which
/// is that a table written for what programs import is honest and a table
/// written for a header is a fiction nobody checked. `runtime_entries_are_sorted`
/// keeps the order the lookup depends on.
static RUNTIME: &[Runtime] = &[
    // ## Making a class at runtime
    //
    // The other direction of [`crate::objc::register`]. That pass replays the
    // classes a guest *declared* in its `__objc_data`; these three are a guest
    // building one while it runs, and they need no replay at all — the class is
    // the host runtime's from the moment it is allocated, so what the guest
    // holds is already a real `Class` and the translation table has nothing to
    // say about it.
    //
    // What does need saying is the `IMP`. It is guest code, and once the class
    // is registered the host runtime will call it as arm64 from anywhere —
    // Foundation, a delegate callback, a thread this runtime never made — so it
    // leaves as a thunk exactly as `method_setImplementation`'s does.
    //
    // Terraria reaches all three: Mono's Objective-C bridge builds an
    // `NSApplication` delegate class at start-up rather than declaring one.
    rtcb!([0], [2], "class_addMethod", U8, P, P, P, P),
    rtc!([0], "class_conformsToProtocol", U8, P, P),
    rtc!([0] -> true, "class_createInstance", P, P, U64),
    rtc!([0], "class_getInstanceMethod", P, P, P),
    rtc!([0], "class_getInstanceSize", U64, P),
    rtc!([0], "class_getInstanceVariable", P, P, P),
    rtc!([0], "class_getName", P, P),
    rtc!([0] -> true, "class_getSuperclass", P, P),
    rtc!([0], "class_isMetaClass", U8, P),
    rtc!([0], "class_respondsToSelector", U8, P, P),
    // ## Ivars, reflectively
    //
    // A guest that reads an ivar through `object_getIvar` rather than through a
    // compiled displacement is asking the *host* runtime where the ivar is, and
    // gets the right answer for free — M6.2 registered the class, so the layout
    // the host reports is the layout the object has. `ivar_getOffset` is what
    // makes that answer usable from guest code, and it returns a `ptrdiff_t`.
    rt!("ivar_getName", P, P),
    rt!("ivar_getOffset", I64, P),
    rtb!([1], "method_setImplementation", P, P, P),
    // ARC and the `objc_msgSend` optimisations clang emits at `-O` whether or
    // not the file uses ARC: `[X alloc]` becomes `objc_alloc`, `[[X alloc]
    // init]` becomes `objc_alloc_init`, `[X new]` becomes `objc_opt_new`. A
    // runtime without these stops at the first object a program creates.
    rtc!([0] -> true, "objc_alloc", P, P),
    rtc!([0] -> true, "objc_allocWithZone", P, P),
    rtc!([0], "objc_alloc_init", P, P),
    rtc!([0] -> true, "objc_allocateClassPair", P, P, P, U64),
    rtc!([0], "objc_autorelease", P, P),
    rt!("objc_autoreleasePoolPop", V, P),
    rt!("objc_autoreleasePoolPush", P),
    rtc!([0], "objc_autoreleaseReturnValue", P, P),
    // ## `__weak`, which is a side table and not a pointer
    //
    // These four are the whole of ARC's weak-reference support that SDL3's Cocoa
    // backend imports, and passthrough is right for a reason worth stating: a
    // weak reference is an entry in the runtime's *own* side table, keyed by the
    // address of the storage. That storage is in guest memory, which is a real
    // address in this process because the guest is identity-mapped, and the
    // object is a host allocation on both sides. So the host's table is already
    // keyed and valued by exactly the things the guest holds — there is nothing
    // to translate and nothing that could be kept coherent by a second table.
    //
    // `objc_storeWeak`'s *value* is the exception, and it is the ordinary one:
    // a guest may legally weakly reference a class, and a class is the one
    // pointer that means something different on each side.
    rt!("objc_copyWeak", V, P, P),
    rt!("objc_destroyWeak", V, P),
    // The hook the compiler emits inside every `for (x in collection)` loop, to
    // be called when the collection changed underneath it. Its argument is the
    // collection, not a class, and all it does is raise — so it needs none of
    // the translation the rest of this table exists for, and the host's own is
    // the right implementation.
    rt!("objc_enumerationMutation", V, P),
    // `object_getClass` of a class is its metaclass, so the argument is
    // translated as well as the result.
    rtc!([0] -> true, "objc_getClass", P, P),
    rtc!([] -> true, "objc_getMetaClass", P, P),
    rt!("objc_getProtocol", P, P),
    rtc!([] -> true, "objc_getRequiredClass", P, P),
    // Whatever `objc_storeWeak` put in the table comes back out, so the return
    // is translated for the same reason that call's argument is.
    rtc!([] -> true, "objc_loadWeakRetained", P, P),
    rtc!([] -> true, "objc_lookUpClass", P, P),
    rtc!([0] -> true, "objc_opt_class", P, P),
    rtc!([0, 1], "objc_opt_isKindOfClass", U8, P, P),
    rtc!([0], "objc_opt_new", P, P),
    rtc!([0], "objc_opt_respondsToSelector", U8, P, P),
    rtc!([0], "objc_opt_self", P, P),
    rtc!([0], "objc_registerClassPair", V, P),
    rtc!([0], "objc_release", V, P),
    rtc!([0], "objc_retain", P, P),
    rtc!([0], "objc_retainAutorelease", P, P),
    rtc!([0], "objc_retainAutoreleaseReturnValue", P, P),
    rtc!([0], "objc_retainAutoreleasedReturnValue", P, P),
    // `objc_storeStrong(id *location, id value)` writes through a guest pointer.
    // The value it stores is an object, and an object is a host pointer on both
    // sides — but a guest storing a *class* is legal, so it is translated.
    rtc!([1], "objc_storeStrong", V, P, P),
    // `id objc_storeWeak(id *location, id obj)` — see the weak note above.
    rtc!([1], "objc_storeWeak", P, P, P),
    // `@synchronized`. Passthrough is not a shortcut here: the host's
    // implementation is a lock table keyed by the object's address, the object
    // is a host allocation, and locking it here is locking the same thing the
    // host would lock for its own code.
    rtc!([0], "objc_sync_enter", I32, P),
    rtc!([0], "objc_sync_exit", I32, P),
    rt!("objc_terminate", V),
    // The `__unsafe_unretained` end of the autoreleased-return-value handshake:
    // it claims the value the callee left in the optimisation's thread-local
    // slot and does *not* retain it. Same shape as
    // `objc_retainAutoreleasedReturnValue` above, and it has to be the host's
    // own implementation for the same reason — the slot it reads is the one the
    // host's `objc_autoreleaseReturnValue` wrote.
    rtc!([0], "objc_unsafeClaimAutoreleasedReturnValue", P, P),
    rtc!([0] -> true, "object_getClass", P, P),
    rtc!([0], "object_getClassName", P, P),
    rt!("object_getIndexedIvars", P, P),
    rtc!([0] -> true, "object_getIvar", P, P, P),
    rtc!([0, 2], "object_setIvar", V, P, P, P),
    rt!("property_copyAttributeList", P, P, P),
    rt!("protocol_getName", P, P),
    rt!("sel_getName", P, P),
    rt!("sel_getUid", P, P),
    rt!("sel_isEqual", U8, P, P),
    rt!("sel_registerName", P, P),
];

/// The `objc_msgSend` family, and what each spelling means here.
///
/// Listed rather than pattern-matched so that a spelling nobody thought about —
/// `objc_msgSendSuper2_stret`, say — is an unresolved import that names itself
/// rather than something quietly treated as an ordinary send.
static MESSAGES: &[(&str, Send)] = &[
    (
        "objc_msgSend",
        Send {
            message: Message::Direct,
            stret: false,
        },
    ),
    (
        "objc_msgSendSuper",
        Send {
            message: Message::Super { skip_one: false },
            stret: false,
        },
    ),
    (
        "objc_msgSendSuper2",
        Send {
            message: Message::Super { skip_one: true },
            stret: false,
        },
    ),
    // The struct-returning spellings. Nothing about the *search* differs: what
    // differs is that `rdi` holds a buffer the caller allocated, so every
    // argument is one register along, and the result is written there rather
    // than returned in a register. `place_shapes` derives the same shift
    // independently from the method's own encoding, and `send` refuses if the
    // two disagree — a guest calling the wrong trampoline for its method's
    // return would otherwise read its receiver out of the buffer pointer.
    (
        "objc_msgSend_stret",
        Send {
            message: Message::Direct,
            stret: true,
        },
    ),
    (
        "objc_msgSendSuper_stret",
        Send {
            message: Message::Super { skip_one: false },
            stret: true,
        },
    ),
    (
        "objc_msgSendSuper2_stret",
        Send {
            message: Message::Super { skip_one: true },
            stret: true,
        },
    ),
];

/// Spellings that exist and are refused, with the reason attached.
static REFUSED: &[(&str, &str)] = &[
    (
        "objc_msgSend_fpret",
        "a long double return lives in st0, which has no arm64 counterpart",
    ),
    (
        "objc_exception_throw",
        "an Objective-C exception needs a personality routine the guest \
                              unwinder does not implement yet (M6)",
    ),
    ("objc_exception_rethrow", "as objc_exception_throw"),
    ("objc_begin_catch", "as objc_exception_throw"),
    ("objc_end_catch", "as objc_exception_throw"),
    // Associated objects key host storage by the object's address, which would
    // work — but the *policy* argument decides whether the value is copied, and
    // copying sends `copyWithZone:` from inside the runtime with none of this
    // runtime's state on the stack. Named rather than half-supported.
    (
        "objc_setAssociatedObject",
        "associated objects may copy their value, which messages from \
                                  inside the host runtime (M6)",
    ),
    ("objc_getAssociatedObject", "as objc_setAssociatedObject"),
    (
        "objc_removeAssociatedObjects",
        "as objc_setAssociatedObject",
    ),
];

/// A nil-terminated object-list variadic, and the counted method that says the
/// same thing without an ellipsis.
///
/// These are the other half of what [`names_a_format`] covers, and the half
/// that cannot be rendered: `+[NSDictionary dictionaryWithObjectsAndKeys:]` has
/// no format string to fold its arguments into. What it has instead is a
/// **counted twin** — every collection class Foundation ships declares one,
/// because the same problem faces anyone bridging to a language without an
/// ellipsis. So the list is walked here, on the guest's own terms, and the
/// counted form is sent with an array this runtime owns.
///
/// The encoding for the variadic form is `@32@0:8@16` — three arguments and no
/// ellipsis — so a marshaller reading it passes `firstObject` and leaves the
/// rest of the list to whatever the host's `va_list` finds. That is not a
/// truncated answer, it is an unterminated walk: Foundation reads registers and
/// stack slots nobody wrote until it happens on a zero, and raises out through
/// the `extern "C"` frame this shim was entered across.
struct ObjectList {
    /// The selector the guest sends.
    variadic: &'static str,
    /// The counted selector to send instead.
    counted: &'static str,
    /// Whether the list reads `object, key, object, key, …` rather than plain
    /// objects — and so becomes *two* arrays rather than one.
    pairs: bool,
}

/// The counted twin of each nil-terminated list a program reaches.
///
/// **Incomplete, and cannot be otherwise**, for the reason [`is_variadic`]
/// gives: nothing in an image records an ellipsis. A list-taking selector
/// outside this table is not refused, it is *not recognised as variadic at all*
/// — which is the known hole in M6.3, recorded in the roadmap rather than
/// papered over.
static OBJECT_LISTS: &[ObjectList] = &[
    ObjectList {
        variadic: "arrayWithObjects:",
        counted: "arrayWithObjects:count:",
        pairs: false,
    },
    ObjectList {
        variadic: "dictionaryWithObjectsAndKeys:",
        counted: "dictionaryWithObjects:forKeys:count:",
        pairs: true,
    },
    ObjectList {
        variadic: "initWithObjects:",
        counted: "initWithObjects:count:",
        pairs: false,
    },
    ObjectList {
        variadic: "initWithObjectsAndKeys:",
        counted: "initWithObjects:forKeys:count:",
        pairs: true,
    },
    ObjectList {
        variadic: "orderedSetWithObjects:",
        counted: "orderedSetWithObjects:count:",
        pairs: false,
    },
    ObjectList {
        variadic: "setWithObjects:",
        counted: "setWithObjects:count:",
        pairs: false,
    },
];

/// Which of the three sends a spelling asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Message {
    /// `objc_msgSend(receiver, sel, ...)`.
    Direct,
    /// `objc_msgSendSuper[2](struct objc_super *, sel, ...)`.
    ///
    /// `skip_one` distinguishes the two: `objc_msgSendSuper2` names the class the
    /// method is *in* and the search starts at its superclass, which is what
    /// `[super foo]` compiles to today. `objc_msgSendSuper` names the class to
    /// start at directly. One field, two meanings, and taking the wrong one sends
    /// the message back to the method that is running — an infinite recursion
    /// rather than a wrong answer.
    Super { skip_one: bool },
}

/// One message send's shape: which trampoline, and whether the caller left a
/// hidden return buffer in the first integer register.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Send {
    message: Message,
    /// `objc_msgSend_stret`. The compiler emits it exactly when the method's
    /// return is SysV MEMORY class, so the receiver is in `rsi` and the
    /// selector in `rdx` — every argument shifted one register along by the
    /// buffer pointer in `rdi`.
    stret: bool,
}

impl Send {
    /// Index of the integer register holding the receiver: `rdi` ordinarily,
    /// `rsi` when a hidden buffer came first.
    fn receiver_reg(self) -> usize {
        usize::from(self.stret)
    }
}

/// The SysV AMD64 integer argument registers, in order.
fn arg_reg(ctx: &RegisterContext, i: usize) -> u64 {
    [ctx.rdi, ctx.rsi, ctx.rdx, ctx.rcx, ctx.r8, ctx.r9][i]
}

fn set_arg_reg(ctx: &mut RegisterContext, i: usize, v: u64) {
    *[
        &mut ctx.rdi,
        &mut ctx.rsi,
        &mut ctx.rdx,
        &mut ctx.rcx,
        &mut ctx.r8,
        &mut ctx.r9,
    ][i] = v;
}

/// Whether `name` (underscore already stripped) is one of this module's.
pub fn shim_for(name: &str) -> Option<crate::jmpbuf::Shim> {
    if MESSAGES.iter().any(|(n, _)| *n == name)
        || REFUSED.iter().any(|(n, _)| *n == name)
        || RUNTIME.binary_search_by_key(&name, |r| r.name).is_ok()
    {
        return Some(serve);
    }
    None
}

/// Symbols this runtime answers for, resolved to the host's own implementations.
///
/// Consulted before [`machish::resolve::NativeDylibProvider`], which denies the
/// `_objc_`, `_class_`, `_sel_` and `_protocol_` families precisely so that a
/// shim can claim them: passing them through unexamined would hand the host
/// runtime guest class structures, which is the fastest route to a crash inside
/// somebody else's `objc_msgSend`.
///
/// Every address here is the host's real function. That is true even of
/// `objc_msgSend`, which is intercepted and never called through — an import
/// slot holding the honest value is one fewer thing to explain when a crash dump
/// is read, and a symbol the host does not export is left undefined so it stays
/// an unresolved import that names itself.
pub fn provider() -> SyntheticDylib {
    let mut lib = SyntheticDylib::new("<rosetta3 objc runtime>");
    // Flat, for the same reason the rest of the runtime's shims are: which
    // library an image thinks `objc_msgSend` comes from is not the question.
    lib.flat(true);
    let names = RUNTIME
        .iter()
        .map(|r| r.name)
        .chain(MESSAGES.iter().map(|(n, _)| *n))
        .chain(REFUSED.iter().map(|(n, _)| *n));
    for name in names {
        if let Some(addr) = host_symbol(name).or_else(|| absent(name)) {
            lib.define_fn(&format!("_{name}"), addr as usize as *const ());
        }
    }
    lib
}

/// An address for a trampoline the *host* does not have.
///
/// The three `_stret` spellings exist only on x86-64: AAPCS64 returns every
/// struct through `x8`, so arm64's `libobjc` needs no separate entry point and
/// `dlsym` answers null for all three. This runtime intercepts them by name and
/// never calls the address — but an import has to resolve to *something*, and
/// null is how this project spells "unsatisfied", which would refuse the call
/// before the shim ever saw it.
///
/// Each gets a function of its own rather than sharing one, because
/// `import_by_target` maps an address back to a symbol and a shared address
/// would make a tail call to one of them arrive as another. Each panics if it
/// is ever really entered, which is the honest thing for an address that is
/// only ever meant to be a name.
fn absent(name: &str) -> Option<u64> {
    unsafe extern "C" fn stret() -> u64 {
        unreachable!("objc_msgSend_stret was called rather than intercepted")
    }
    unsafe extern "C" fn super_stret() -> u64 {
        unreachable!("objc_msgSendSuper_stret was called rather than intercepted")
    }
    unsafe extern "C" fn super2_stret() -> u64 {
        unreachable!("objc_msgSendSuper2_stret was called rather than intercepted")
    }
    let f: unsafe extern "C" fn() -> u64 = match name {
        "objc_msgSend_stret" => stret,
        "objc_msgSendSuper_stret" => super_stret,
        "objc_msgSendSuper2_stret" => super2_stret,
        _ => return None,
    };
    Some(f as *const () as usize as u64)
}

/// The host's own implementation of `name`, if this process has one.
fn host_symbol(name: &str) -> Option<u64> {
    let c = std::ffi::CString::new(name).ok()?;
    // SAFETY: `RTLD_DEFAULT` is `(void *)-2` on macOS — search every loaded
    // image — and `c` is a NUL-terminated string that outlives the call. A
    // missing symbol is null, which is checked rather than dereferenced.
    let ptr = unsafe { libc::dlsym(-2isize as *mut core::ffi::c_void, c.as_ptr()) };
    (!ptr.is_null()).then_some(ptr as usize as u64)
}

/// Service one call into the Objective-C runtime.
fn serve(symbol: &str, ctx: &mut RegisterContext, mem: &mut dyn GuestMemory) -> Result<()> {
    if let Some((_, reason)) = REFUSED.iter().find(|(n, _)| *n == symbol) {
        return Err(AbiError::Refused {
            symbol: symbol.to_string(),
            reason,
        });
    }
    let vm = current_vm(symbol)?;
    if let Some((_, message)) = MESSAGES.iter().find(|(n, _)| *n == symbol) {
        return send(symbol, *message, vm, ctx, mem);
    }
    let Ok(i) = RUNTIME.binary_search_by_key(&symbol, |r| r.name) else {
        return Err(AbiError::NoSignature {
            symbol: symbol.to_string(),
        });
    };
    call_runtime(symbol, &RUNTIME[i], vm, ctx, mem)
}

/// Forward one runtime entry point, translating the classes that cross it.
fn call_runtime(
    symbol: &str,
    entry: &Runtime,
    vm: &Vm,
    ctx: &mut RegisterContext,
    mem: &dyn GuestMemory,
) -> Result<()> {
    let Some(host) = host_symbol(entry.name) else {
        return Err(AbiError::Unsatisfied {
            symbol: symbol.to_string(),
        });
    };
    let convert = Classes {
        vm,
        classes: entry.classes,
        class_return: entry.class_return,
        callbacks: entry.callbacks,
        blocks: &[],
    };
    marshal::call_types(symbol, entry.types, host, ctx, mem, &convert)
}

/// Class pointers, translated by parameter index.
struct Classes<'a> {
    vm: &'a Vm,
    classes: &'a [u8],
    class_return: bool,
    /// See [`Runtime::callbacks`]. Empty for a message send, where the `IMP` on
    /// the far side is the host's own and nothing of the guest's is installed.
    callbacks: &'a [u8],
    /// Parameter indices holding a block, from the method's own encoding.
    ///
    /// Empty for the runtime entry points, none of which takes one.
    blocks: &'a [u8],
}

impl Convert for Classes<'_> {
    fn argument(&self, symbol: &str, index: usize, raw: u64) -> Result<u64> {
        if self.callbacks.contains(&(index as u8)) {
            return crate::abi::Callbacks::substitute(self.vm, symbol, raw);
        }
        if self.blocks.contains(&(index as u8)) {
            // A block is edited where it lies and travels unchanged, which is
            // why this returns `raw` rather than a substitute — see
            // [`crate::shims::block`] for why the thunk has to be written into
            // the literal instead of kept beside it.
            //
            // A fresh view rather than the caller's `mem`: guest memory is
            // identity-mapped and shared, so a second handle addresses the same
            // bytes, and taking one here is what lets this hook edit memory at
            // all without threading a mutable borrow through the marshaller.
            //
            // A null block is a guest passing `nil`, which is legal for every
            // block parameter in Cocoa and means "no handler".
            if raw != 0 {
                let mut view = self.vm.view();
                crate::shims::block::substitute_block(symbol, raw, &mut view, self.vm)?;
            }
            return Ok(raw);
        }
        match self.classes.contains(&(index as u8)) {
            true => Ok(self.vm.objc_host_class(raw)),
            false => Ok(raw),
        }
    }

    fn result(&self, raw: u64) -> u64 {
        match self.class_return {
            true => self.vm.objc_guest_class(raw),
            false => raw,
        }
    }
}

/// Send one message.
///
/// On success either `rip` is the guest `IMP` the message resolved to — the
/// transfer performed, not a call made — or the host method has run and its
/// result is in the guest's return register.
fn send(
    symbol: &str,
    send: Send,
    vm: &Vm,
    ctx: &mut RegisterContext,
    mem: &mut dyn GuestMemory,
) -> Result<()> {
    let base = send.receiver_reg();
    let sel = arg_reg(ctx, base + 1);

    // Where the receiver is, and where the method search starts, which for a
    // `super` send are two different classes.
    let (receiver, search) = match send.message {
        Message::Direct => (arg_reg(ctx, base), None),
        Message::Super { skip_one } => {
            let sup = arg_reg(ctx, base);
            let receiver = (mem as &dyn GuestMemory).read_u64(sup, symbol)?;
            let named = (mem as &dyn GuestMemory).read_u64(sup.wrapping_add(8), symbol)?;
            let named = vm.objc_host_class(named) as host::Class;
            // SAFETY: `named` is a `Class` — either the host's own, arrived
            // through an ordinary bind, or one this runtime registered. A
            // superclass query on one is what the runtime is for.
            let start = match skip_one {
                true => unsafe { host::class_getSuperclass(named) },
                false => named,
            };
            (receiver, Some(start))
        }
    };

    // Messaging nil is not an error, it is the language: the result is zero,
    // in whichever register the caller reads. Both are cleared because the
    // method's return type is exactly what is not known at this point — and it
    // is not worth looking up, since nothing about a nil send depends on it.
    if receiver == 0 {
        ctx.rax = 0;
        ctx.xmm[0] = [0, 0];
        return Ok(());
    }

    // The receiver as the host runtime knows it. A guest *class* is the only
    // thing that changes here; an object is a host allocation and is already
    // itself.
    let host_receiver = vm.objc_host_class(receiver);
    // A receiver in guest memory is not automatically broken. A constant
    // `@"..."` string is an object in the guest's own `__cfstring`, and the host
    // runtime understands it perfectly: its `isa` is an ordinary bind of
    // `___CFConstantStringClassReference`, so it points at a *host* class. What
    // is broken is a guest **class** the runtime did not register, whose `isa` is
    // the dead metaclass sitting beside it in `__objc_data` — messaging that
    // means the host runtime walking a structure it never realized.
    if vm.program.is_guest_address(host_receiver) {
        let isa = vm.objc_host_class((mem as &dyn GuestMemory).read_u64(host_receiver, symbol)?);
        if isa == 0 || vm.program.is_guest_address(isa) {
            return Err(AbiError::Refused {
                symbol: symbol.to_string(),
                reason: "the receiver is a guest address whose class the runtime never realized",
            });
        }
    }

    // SAFETY: `host_receiver` is a pointer the host runtime owns — an object it
    // allocated, or a class it realized — which is what `object_getClass` takes.
    let class = match search {
        Some(start) => start,
        None => unsafe { host::object_getClass(host_receiver as host::Class) },
    };
    if class.is_null() {
        return Err(AbiError::Refused {
            symbol: symbol.to_string(),
            reason: "the receiver's class could not be determined",
        });
    }

    // One lookup for both halves of what a send needs. A class method is an
    // instance method of the metaclass, and `class` is already the metaclass
    // when the receiver is a class, so this answers for both.
    //
    // SAFETY: `class` is a `Class` and `sel` a uniqued `SEL` — M6.2 rewrote the
    // guest's `__objc_selrefs` slots from string pointers into the host
    // runtime's own selectors, so the value the guest loaded *is* one.
    let method = unsafe { host::class_getInstanceMethod(class, sel as host::Sel) };
    if method.is_null() {
        return Err(unrecognised(symbol, class, sel));
    }
    // SAFETY: `method` is a `Method` the runtime just returned for `class`.
    let imp = unsafe { host::method_getImplementation(method) } as u64;

    // Guest → guest: the trampoline's whole job is the jump, and the guest's
    // registers are already the callee's.
    if let Some(guest_imp) = vm.guest_behind_thunk(imp) {
        // A `super` send arrived with the `objc_super` structure in `rdi`, and
        // the method is called with the receiver.
        set_arg_reg(ctx, base, receiver);
        return transfer(symbol, ctx, mem, guest_imp);
    }

    // Guest → host: the method's own type encoding is the signature.
    let signature = signature_of(symbol, method)?;
    // A variadic method's arguments are in registers the encoding does not
    // describe. The ones a program actually reaches all have the same shape —
    // a trailing `NSString` format — so they are *rendered here* and the method
    // is then sent with the result standing in for its format, which is a call
    // the encoding does describe exactly. See [`render_variadic`].
    if is_variadic(sel) {
        // A nil-terminated object list has no format to fold its arguments
        // into, so it is answered by sending the counted twin instead — which
        // returns rather than falling through to the send below, because the
        // selector that reaches the host is not the one the guest sent.
        if let Some(list) = object_list(sel) {
            return send_object_list(
                vm,
                symbol,
                list,
                &signature,
                send,
                class,
                host_receiver,
                base,
                ctx,
                mem,
            );
        }
        render_variadic(symbol, sel, &signature, base, ctx, mem)?;
    }
    // Two independent statements about the same fact, held to each other. The
    // *guest* said "struct return" by calling the `_stret` trampoline; the
    // *method* says so through its encoding. If they disagree, every argument
    // is one register out — the receiver would be read from the buffer pointer
    // — and nothing about the result would fault.
    let by_encoding = matches!(
        signature.shapes.first(),
        Some(Shape::Struct(agg)) if agg.guest_class() == GuestClass::Memory
    );
    if by_encoding != send.stret {
        return Err(AbiError::Refused {
            symbol: symbol.to_string(),
            reason: "the trampoline the guest called disagrees with the method's own \
                     return class",
        });
    }
    // Everything fallible happens before `rdi` moves, so a refusal leaves the
    // guest context exactly as it was found — the guarantee every other refusal
    // in this project makes.
    set_arg_reg(ctx, base, host_receiver);
    let convert = Classes {
        vm,
        classes: &signature.classes,
        class_return: signature.class_return,
        callbacks: &[],
        blocks: &signature.blocks,
    };
    marshal::call_shapes(symbol, &signature.shapes, imp, ctx, mem, &convert)
}

/// Continue the guest at `target`, as the trampoline would have.
///
/// The guest's stack has to look exactly as it would after a real
/// `objc_msgSend`: the `IMP` returns to whoever called the message, not to
/// anything of this runtime's. Where that return address comes from depends on
/// how control reached the shim; see the module note.
fn transfer(
    symbol: &str,
    ctx: &mut RegisterContext,
    mem: &mut dyn GuestMemory,
    target: u64,
) -> Result<()> {
    let Some(state) = crate::thread::current() else {
        return Err(AbiError::Refused {
            symbol: symbol.to_string(),
            reason: "a message sent outside a running guest thread",
        });
    };
    if !state.entered_by_jump() {
        // An intercepted `call`: microlift exits before the push, so the return
        // address is this runtime's to write. `rip` is already the instruction
        // after the call, which is precisely where the method must return to.
        let rsp = ctx.rsp.wrapping_sub(8);
        mem.write(rsp, &ctx.rip.to_le_bytes())
            .map_err(|_| AbiError::Memory {
                symbol: symbol.to_string(),
                addr: rsp,
            })?;
        ctx.rsp = rsp;
    }
    ctx.rip = target;
    // The live stack pointer moved without an exit reporting it, and `rip` is
    // now a destination rather than a resume point: whoever serviced this call
    // must not synthesize a return over it.
    state.rsp.set(ctx.rsp);
    state.redirect();
    Ok(())
}

/// The method's type encoding, decoded, or a refusal naming what stopped it.
fn signature_of(symbol: &str, method: host::Method) -> Result<Signature> {
    // SAFETY: `method` is a `Method` the runtime returned; its encoding is a
    // NUL-terminated string the runtime owns, read and copied before anything
    // else happens.
    let types = unsafe { host::method_getTypeEncoding(method) };
    if types.is_null() {
        return Err(AbiError::Refused {
            symbol: symbol.to_string(),
            reason: "the method carries no type encoding, so its signature is unknown",
        });
    }
    // SAFETY: as above.
    let types = unsafe { std::ffi::CStr::from_ptr(types) };
    let Ok(types) = types.to_str() else {
        return Err(AbiError::Refused {
            symbol: symbol.to_string(),
            reason: "the method's type encoding is not valid UTF-8",
        });
    };

    encoding::signature(types).map_err(|reason| AbiError::Refused {
        symbol: symbol.to_string(),
        reason,
    })
}

/// Whether the selector names a method that takes an ellipsis.
///
/// By name, because nothing else says so: the two conventions Cocoa keeps to
/// are a trailing format string ([`names_a_format`]) and a nil-terminated
/// object list ([`OBJECT_LISTS`]).
fn is_variadic(sel: u64) -> bool {
    let Some(name) = selector_name(sel) else {
        return false;
    };
    names_a_format(&name) || OBJECT_LISTS.iter().any(|l| l.variadic == name)
}

/// The counted twin of a nil-terminated list selector, if this is one.
fn object_list(sel: u64) -> Option<&'static ObjectList> {
    let name = selector_name(sel)?;
    OBJECT_LISTS.iter().find(|l| l.variadic == name)
}

/// Whether the selector's last keyword names a *format string* parameter.
///
/// "Ends in `Format:`" is the obvious reading of Cocoa's convention and it is
/// too broad by one word: `-[NSOpenGLView initWithFrame:pixelFormat:]` ends in
/// `Format:` and takes an `NSOpenGLPixelFormat *`, not a format. Treating it as
/// variadic is not a near miss — its first parameter is an `NSRect`, so the
/// register the format would be in cannot be computed and the send is refused,
/// which is where Geometry Dash's window creation stopped.
///
/// So the rule reads the last keyword as camelCase words and asks what
/// qualifies "Format": nothing (`raise:format:`), `with`
/// (`stringWithFormat:`), or `append`/`appending` (`appendFormat:`,
/// `stringByAppendingFormat:`). A `pixelFormat` is qualified by `pixel` and is
/// a thing rather than a format. This is still a convention and still cannot be
/// complete — nothing in an image says a method is variadic — but it is wrong
/// in the direction of doing nothing rather than of refusing a call that was
/// never variadic.
fn names_a_format(selector: &str) -> bool {
    let Some(last) = selector
        .strip_suffix(':')
        .and_then(|s| s.rsplit(':').next())
    else {
        return false;
    };
    // Split camelCase: `stringByAppendingFormat` -> [string, By, Appending, Format].
    let mut words = Vec::new();
    let mut start = 0;
    for (i, c) in last.char_indices() {
        if c.is_ascii_uppercase() && i != 0 {
            words.push(&last[start..i]);
            start = i;
        }
    }
    words.push(&last[start..]);
    let mut words = words.iter().rev().map(|w| w.to_ascii_lowercase());
    if words.next().as_deref() != Some("format") {
        return false;
    }
    matches!(
        words.next().as_deref(),
        None | Some("with") | Some("append") | Some("appending")
    )
}

/// Turn a variadic format-style send into the non-variadic send of the rendered
/// string, in place.
///
/// The trick is that every variadic method Cocoa has of this shape takes its
/// format as the **last named parameter** — `stringWithFormat:`,
/// `appendFormat:`, `initWithFormat:`, `raise:format:` — so rendering the
/// conversions here and doubling the `%`s in the result produces a format with
/// no conversions left. Re-sending the same selector with that string is the
/// same call with the ellipsis empty, which the method's own encoding already
/// describes. Nothing about where the *variadic* arguments live has to be told
/// to the host at all, which is the whole difficulty going away rather than
/// being solved.
///
/// The shapes this cannot do are still refused by name: a nil-terminated object
/// list (`arrayWithObjects:`) has no format to render, and a format that is not
/// the last argument, or fixed arguments that are not all integer-class, would
/// put the variadic tail somewhere this does not look.
fn render_variadic(
    symbol: &str,
    sel: u64,
    signature: &Signature,
    base: usize,
    ctx: &mut RegisterContext,
    mem: &mut dyn GuestMemory,
) -> Result<()> {
    // Named, because "a variadic method this cannot render" is a work item and
    // the selector is the whole of it — the reader otherwise learns only that
    // *some* message was refused, on a list that cannot be enumerated from the
    // image (see [`VARIADIC_SELECTORS`]).
    let refuse = |reason: &str| {
        let selector = selector_name(sel).unwrap_or_else(|| format!("<sel {sel:#x}>"));
        Err(AbiError::Nested {
            symbol: symbol.to_string(),
            reason: format!("{selector}: {reason}"),
        })
    };
    // Every named parameter must be integer-class, or the format is not in the
    // register this computes and the variadic tail does not start after it.
    let params = &signature.shapes[1..];
    if !params.iter().all(|s| s.scalar() == Some(T::Pointer)) {
        return refuse(
            "a variadic method whose named parameters are not all objects; the format's \
             register cannot be computed",
        );
    }
    // `self`, `_cmd`, then the arguments — the format is the last of them, and
    // `base` accounts for a hidden struct-return buffer ahead of all of it.
    let Some(fmt_reg) = params.len().checked_sub(1).map(|i| i + base) else {
        return refuse("a variadic method with no named format argument");
    };
    if fmt_reg >= SYSV_INT_REGS {
        return refuse("a variadic method whose format is not in an argument register");
    }
    let Some(text) =
        crate::shims::nslog::render_format(symbol, ctx, &*mem, arg_reg(ctx, fmt_reg), fmt_reg + 1)?
    else {
        // Not a readable string. Left alone rather than refused: the method
        // will do to it whatever it does to any other argument.
        return Ok(());
    };
    let Some(replacement) = crate::shims::nslog::escaped_string(&text) else {
        return refuse("the rendered format could not be made into a string");
    };
    set_arg_reg(ctx, fmt_reg, replacement);
    Ok(())
}

/// The SysV AMD64 integer argument registers there are.
const SYSV_INT_REGS: usize = 6;

/// How many objects a nil-terminated list may hold before this stops walking.
///
/// A list with no terminator is indistinguishable from a long one until the
/// read faults, and the read need not fault: past the arguments lies the
/// guest's own stack, which is mapped and full of non-zero words. So the walk
/// is bounded and says so, rather than climbing the caller's frames until it
/// leaves the mapping. Well past anything a program writes by hand.
const OBJECT_LIST_CAP: usize = 4096;

/// Send the counted twin of a nil-terminated object-list message.
///
/// The three questions this has to answer are all about *where the list is*,
/// and SysV AMD64 answers each of them: an integer-class variadic argument
/// continues in the argument registers after the named ones, and spills to the
/// caller's stack at `rsp + 0` — which, because microlift exits before the
/// `CALL`, is the same stack path [`crate::abi::marshal`] already reads
/// MEMORY-class arguments from. `firstObject` is named rather than variadic and
/// is the list's first element, so the walk starts at it.
///
/// The result is arrays this runtime owns, handed to a method with no ellipsis
/// at all. Nothing about arm64's variadic convention has to be reproduced,
/// which is the same move [`render_variadic`] makes for a format string and for
/// the same reason: the difficulty goes away rather than being solved.
///
/// On any refusal `ctx` is untouched, as everywhere else — every fallible step
/// happens before `rax` is written.
#[allow(clippy::too_many_arguments)]
fn send_object_list(
    vm: &Vm,
    symbol: &str,
    list: &ObjectList,
    signature: &Signature,
    send: Send,
    class: host::Class,
    host_receiver: u64,
    base: usize,
    ctx: &mut RegisterContext,
    mem: &mut dyn GuestMemory,
) -> Result<()> {
    let refuse = |reason: String| {
        Err(AbiError::Nested {
            symbol: symbol.to_string(),
            reason: format!("{}: {reason}", list.variadic),
        })
    };
    // The guest called `objc_msgSend_stret` for a method returning an object.
    // Every argument would be one register out, and the counted send would be
    // built from the wrong words.
    if send.stret {
        return refuse("sent through the struct-return trampoline, which returns an object".into());
    }
    // `self`, `_cmd`, `firstObject`, each integer-class — which is what makes
    // the register the list starts in computable at all.
    let params = &signature.shapes[1..];
    if params.len() != 3 || !params.iter().all(|s| s.scalar() == Some(T::Pointer)) {
        return refuse(format!(
            "expected a receiver, a selector and one named object, and the method's \
             encoding describes {} named parameter(s)",
            params.len().saturating_sub(2)
        ));
    }

    // Walk it. `firstObject` is at the last named register; the ellipsis
    // continues from the next one and spills to the stack after `r9`.
    let mut items = Vec::new();
    let mut reg = base + 2;
    let mut spill = 0u64;
    loop {
        let raw = match reg < SYSV_INT_REGS {
            true => arg_reg(ctx, reg),
            false => {
                let at = ctx.rsp.wrapping_add(spill);
                spill += 8;
                (mem as &dyn GuestMemory).read_u64(at, symbol)?
            }
        };
        reg += 1;
        if raw == 0 {
            break;
        }
        if items.len() == OBJECT_LIST_CAP {
            return refuse(format!(
                "a list of more than {OBJECT_LIST_CAP} objects with no nil terminator found"
            ));
        }
        // A guest class is legal in any of these — `@[SomeClass]` is an array
        // of one object — and is the one pointer that has to be translated.
        items.push(vm.objc_host_class(raw) as *mut std::ffi::c_void);
    }

    // The counted twin, looked up on the same class the variadic one was found
    // on, so a receiver that does not have it is named rather than assumed.
    let Some(name) = host::c_string(list.counted) else {
        return refuse("the counted selector is not a C string".into());
    };
    // SAFETY: `name` is a NUL-terminated string that outlives the call, and
    // `class` is a `Class` the runtime returned.
    let counted = unsafe { host::sel_registerName(name.as_ptr()) };
    // SAFETY: as above; `counted` is a uniqued `SEL`.
    let method = unsafe { host::class_getInstanceMethod(class, counted) };
    if method.is_null() {
        return refuse(format!(
            "the receiver's class does not implement {}",
            list.counted
        ));
    }
    // SAFETY: `method` is a `Method` the runtime just returned for `class`.
    let imp = unsafe { host::method_getImplementation(method) };
    // A guest class overriding the counted form would be re-entered as arm64.
    // Nothing plausible does, and "nothing plausible does" is not a reason to
    // find out at the call.
    if vm.guest_behind_thunk(imp as u64).is_some() {
        return refuse(format!("{} is implemented by the guest", list.counted));
    }

    let receiver = host_receiver as *mut std::ffi::c_void;
    let result = match list.pairs {
        false => {
            // SAFETY: `imp` implements `…WithObjects:count:`, whose prototype
            // is exactly this — an object array and its length. `items` is
            // alive across the call and holds `items.len()` objects.
            let f: extern "C" fn(
                *mut std::ffi::c_void,
                host::Sel,
                *const *mut std::ffi::c_void,
                usize,
            ) -> u64 = unsafe { std::mem::transmute(imp) };
            f(receiver, counted, items.as_ptr(), items.len())
        }
        true => {
            // The list is `object, key, object, key, …`; an odd one is a guest
            // bug, and Foundation's answer to it is a raise this cannot catch.
            if items.len() % 2 != 0 {
                return refuse(format!(
                    "{} objects and keys, which is not a whole number of pairs",
                    items.len()
                ));
            }
            let (objects, keys): (Vec<_>, Vec<_>) =
                items.chunks_exact(2).map(|p| (p[0], p[1])).unzip();
            // SAFETY: as above, for `…Objects:forKeys:count:`. Both vectors
            // hold `objects.len()` objects and are alive across the call.
            let f: extern "C" fn(
                *mut std::ffi::c_void,
                host::Sel,
                *const *mut std::ffi::c_void,
                *const *mut std::ffi::c_void,
                usize,
            ) -> u64 = unsafe { std::mem::transmute(imp) };
            f(
                receiver,
                counted,
                objects.as_ptr(),
                keys.as_ptr(),
                objects.len(),
            )
        }
    };
    ctx.rax = result;
    Ok(())
}

/// A uniqued selector as its name, for a diagnostic or the variadic check.
fn selector_name(sel: u64) -> Option<String> {
    if sel == 0 {
        return None;
    }
    // SAFETY: `sel` is a uniqued `SEL`, which the runtime interns for the
    // process lifetime; `sel_getName` returns a string it owns.
    let name = unsafe { host::sel_getName(sel as host::Sel) };
    if name.is_null() {
        return None;
    }
    // SAFETY: as above.
    unsafe { std::ffi::CStr::from_ptr(name) }
        .to_str()
        .ok()
        .map(str::to_string)
}

/// "This class does not implement that selector", with both names in it.
///
/// Worth the lookup: the alternative diagnostic is an address, and the whole
/// point of refusing rather than forwarding is that the reader can see which
/// message was sent.
fn unrecognised(symbol: &str, class: host::Class, sel: u64) -> AbiError {
    // SAFETY: `class` is a `Class` the runtime owns; `class_getName` returns a
    // string it owns.
    let name = unsafe { host::class_getName(class) };
    let name = match name.is_null() {
        true => "<unnamed>".to_string(),
        // SAFETY: as above.
        false => unsafe { std::ffi::CStr::from_ptr(name) }
            .to_string_lossy()
            .into_owned(),
    };
    let selector = selector_name(sel).unwrap_or_else(|| format!("<sel {sel:#x}>"));
    AbiError::Nested {
        symbol: symbol.to_string(),
        reason: format!(
            "{name} does not respond to {selector}; message forwarding builds an NSInvocation \
             describing a frame this runtime does not have (M6)"
        ),
    }
}

/// The VM running this thread, which a message needs for the class maps and for
/// entering guest code.
fn current_vm(symbol: &str) -> Result<&'static Vm> {
    crate::thunk::current().ok_or_else(|| AbiError::Refused {
        symbol: symbol.to_string(),
        reason: "messaging needs the JIT, and no VM is running on this thread",
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_entries_are_sorted_and_unique() {
        for pair in RUNTIME.windows(2) {
            assert!(
                pair[0].name < pair[1].name,
                "{} and {} are out of order; binary_search would miss one",
                pair[0].name,
                pair[1].name
            );
        }
    }

    #[test]
    fn every_name_this_module_claims_is_one_it_can_serve() {
        // The provider and the shim table are two lists that must agree: a
        // symbol resolved but not shimmed would be *called* — the host's
        // `objc_msgSend` with a guest class in `rdi` — and a symbol shimmed but
        // not resolved would be an unsatisfied import.
        for name in RUNTIME
            .iter()
            .map(|r| r.name)
            .chain(MESSAGES.iter().map(|(n, _)| *n))
            .chain(REFUSED.iter().map(|(n, _)| *n))
        {
            assert!(shim_for(name).is_some(), "{name} is claimed and not served");
            assert!(
                crate::shims::is_shimmed(&format!("_{name}")),
                "{name} would be promoted to an inlined host call"
            );
        }
    }

    #[test]
    fn the_host_really_exports_what_this_module_forwards_to() {
        // Every entry in the table is a passthrough, so a name misspelled here
        // is a program that stops with "unsatisfied import" at run time. `dlsym`
        // answers the question now instead.
        for entry in RUNTIME {
            assert!(
                host_symbol(entry.name).is_some(),
                "libobjc has no {}",
                entry.name
            );
        }
    }

    #[test]
    fn a_refused_spelling_is_never_also_served_as_a_message() {
        for (name, _) in REFUSED {
            assert!(
                !MESSAGES.iter().any(|(m, _)| m == name)
                    && RUNTIME.binary_search_by_key(name, |r| r.name).is_err(),
                "{name} is both refused and implemented"
            );
        }
    }

    #[test]
    fn the_variadic_check_is_by_selector_name() {
        // `sel_registerName` is the host's own, so this is checking the rule
        // against real selectors rather than against a string.
        // SAFETY: both are NUL-terminated literals.
        let format = unsafe { host::sel_registerName(c"stringWithFormat:".as_ptr()) } as u64;
        let objects = unsafe { host::sel_registerName(c"arrayWithObjects:".as_ptr()) } as u64;
        let plain = unsafe { host::sel_registerName(c"count".as_ptr()) } as u64;
        assert!(is_variadic(format));
        assert!(is_variadic(objects));
        assert!(!is_variadic(plain));
        assert!(!is_variadic(0));
    }

    #[test]
    fn a_format_keyword_is_a_word_not_a_suffix() {
        for yes in [
            "stringWithFormat:",
            "initWithFormat:",
            "localizedStringWithFormat:",
            "appendFormat:",
            "stringByAppendingFormat:",
            "raise:format:",
            "predicateWithFormat:",
        ] {
            assert!(names_a_format(yes), "{yes} should name a format");
        }
        // `initWithFrame:pixelFormat:` is the one that mattered: it ends in
        // `Format:`, is not variadic, and takes an `NSRect` — so believing the
        // suffix refuses the call that opens Geometry Dash's window.
        for no in [
            "initWithFrame:pixelFormat:",
            "setPixelFormat:",
            "audioFormat:",
            "count",
            "format",
            "",
        ] {
            assert!(!names_a_format(no), "{no} should not name a format");
        }
    }
}
