//! Realizing the guest's Objective-C classes, in the host's runtime.
//!
//! [M6.1](../../../docs/ROADMAP.md) decoded what the guest declares. This runs
//! it: every class the guest defines is created in the host's `libobjc` through
//! the public runtime API, with every `IMP` replaced by an
//! [M3 thunk](crate::thunk) so that the host calls arm64 and lands in the JIT.
//!
//! ## Why replay, and not registration
//!
//! dyld hands `libobjc` a `mach_header` and the runtime walks the image's
//! `__objc_*` sections itself. Doing that here would mean private entry points
//! and a host runtime willing to read metadata in a format
//! *it never sees* — the two architectures' encodings differ, so an arm64
//! `libobjc` has no reason to have a working path for an x86-64 image, and
//! "seems to work" is not a claim this project makes about somebody else's
//! parser.
//!
//! Replaying through `objc_allocateClassPair` and friends asks the runtime to
//! do only what it already does for every other caller of those functions. What
//! it costs is that the class the runtime creates is a **different object** from
//! the `objc_class` in the guest's `__objc_data`, and the guest holds the
//! latter: `[Greeter alloc]` compiles to
//! `leaq _OBJC_CLASS_$_Greeter(%rip), %rdi`, a direct reference with no slot to
//! redirect. Hence [`ObjcRuntime::host_class`] and the substitution at the ABI
//! boundary — the same shape as [`Callbacks::substitute`], and for the same
//! reason: a pointer that means one thing on each side of the seam.
//!
//! [`Callbacks::substitute`]: crate::abi::Callbacks::substitute
//!
//! ## Ivar offsets, and the case that needs no help
//!
//! A guest method reads an ivar one of two ways, and which one is a decision
//! clang made at compile time:
//!
//! - **A hardcoded displacement** — `movl 0x8(%rax), %eax` — when the whole
//!   superclass chain's layout was known, which for a direct `NSObject`
//!   subclass it always is.
//! - **A load from `_OBJC_IVAR_$_Class._name`** — when it was not, which is
//!   what "non-fragile ivars" buys and is what a subclass of a Foundation class
//!   compiles to.
//!
//! The first needs the host runtime to lay the class out exactly as the guest's
//! compiler assumed, and it does, because both start after the superclass's
//! instance size and `NSObject` is eight bytes on both architectures. The
//! second needs the runtime's answer written back into the guest's own
//! `__objc_ivar` word, which is what [`ObjcRuntime`] does with
//! [`ObjcIvar::offset_va`] — the fixup dyld would have performed. Both are
//! checked rather than assumed: `ivar_offsets_match_what_the_guest_compiled`
//! compares the two.
//!
//! [`ObjcIvar::offset_va`]: machish::extensions::objc::ObjcIvar::offset_va
//!
//! ## Scope
//!
//! Classes, their instance and class methods, ivars, protocols and categories
//! are registered. Everything a class *cannot* be given faithfully is a
//! [`Refusal`] naming it, never a partial registration: a class missing one
//! method is a class that crashes on one message, which is strictly worse than
//! a class that was never created. Swift classes are refused outright — a Swift
//! metadata record is not an `objc_class` and this reads it as one.
//!
//! Dispatch — `objc_msgSend` and its variants, entered from *guest* code — is
//! [`dispatch`], which is the other direction and needs everything here first:
//! a message resolves an `IMP` through the class this module realized, and finds
//! either one of its thunks (a jump back into the guest) or a host method whose
//! type encoding says how to call it.

pub mod dispatch;
pub mod encoding;
pub mod host;

use std::collections::{HashMap, HashSet};

use machish::extensions::objc::{ObjcCategory, ObjcClass, ObjcImage, ObjcMethod, ObjcProtocol};
use machish::report::LinkReport;

use crate::abi::GuestMemory;
use crate::vm::{Vm, VmError};

/// Something the guest declared that could not be reproduced faithfully.
///
/// Each is a named stop with the guest's own name in it, in the register the
/// rest of this project uses for refusals: a work item that closes, rather than
/// a silent omission that becomes a crash in somebody's application.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Refusal {
    /// The host runtime already has a class of this name. Registering over it
    /// is not possible and shadowing it would make which one a name resolves to
    /// depend on link order.
    NameTaken { name: String },
    /// The superclass is neither a host class nor a guest class this pass
    /// registered — an unsatisfied weak import, or a class whose own
    /// registration was refused.
    SuperclassMissing { name: String, superclass: u64 },
    /// A superclass chain that comes back to itself. Not producible by a
    /// compiler; guarded because this walks a graph read out of guest memory.
    SuperclassCycle { name: String },
    /// `objc_allocateClassPair` declined, which it does for a name already in
    /// use and little else.
    AllocationFailed { name: String },
    /// An ivar the runtime would not add. Refusing the class rather than the
    /// ivar is deliberate: a class one ivar short lays every later ivar out at
    /// the wrong offset.
    IvarRejected { name: String, ivar: String },
    /// A method that is installed and works for a *guest* sender, but which the
    /// **host** cannot call faithfully.
    ///
    /// The distinction is the whole point. A guest→guest send never touches a
    /// thunk — [`dispatch`] jumps straight to the `IMP` with the guest's own
    /// registers and stack — so `-[Dog weight]` returning a `double` is
    /// perfectly callable from the guest and refusing to install it would break
    /// a working program. What cannot happen is the *host* calling it: a thunk
    /// carries six integer and eight floating-point registers and an integer
    /// return, so an aggregate argument, a seventh integer, or a
    /// floating-point return is read from somewhere nobody wrote.
    ///
    /// Listed rather than refused, because the method is genuinely usable on the
    /// path most of them are used on, and because the list is the work item:
    /// each entry closes when the thunk is built from the encoding.
    MethodNotHostCallable {
        name: String,
        method: String,
        reason: &'static str,
    },
    /// A method whose `IMP` could not be made callable from the host.
    MethodRejected {
        name: String,
        method: String,
        reason: &'static str,
    },
    /// A Swift class, identified by the low bits of its `bits` word. Its
    /// metadata is not an `objc_class`.
    SwiftClass { name: String },
    /// A category on a class that is neither the host's nor one registered
    /// here.
    CategoryTargetMissing { name: String, class: u64 },
    /// A name with an interior NUL, which cannot round-trip through the C API.
    UnrepresentableName { name: String },
}

/// What one registration pass did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub classes: usize,
    pub methods: usize,
    pub ivars: usize,
    pub protocols: usize,
    pub categories: usize,
    pub selectors: usize,
    /// Ivar offset words rewritten with the runtime's answer.
    pub offsets_patched: usize,
}

/// The guest's classes, as the host runtime now knows them.
///
/// Held by the [`Vm`] because the maps outlive registration: every message the
/// guest sends names a class by the address in its own `__objc_data`, and
/// answering that is a lookup here.
#[derive(Debug, Default)]
pub struct ObjcRuntime {
    /// Guest `objc_class` VA → the host `Class` standing for it.
    to_host: HashMap<u64, u64>,
    /// The reverse, so a `Class` coming *back* from the host runtime can be
    /// handed to the guest as the address the guest already has. Without it a
    /// guest comparing `[obj class]` against `Greeter` finds them unequal.
    to_guest: HashMap<u64, u64>,
    /// Guest `protocol_t` VA → host `Protocol *`.
    protocols: HashMap<u64, u64>,
    stats: Stats,
    refusals: Vec<Refusal>,
}

impl ObjcRuntime {
    /// The host `Class` for a guest `objc_class` address, if it was registered.
    pub fn host_class(&self, guest_va: u64) -> Option<u64> {
        self.to_host.get(&guest_va).copied()
    }

    /// The guest `objc_class` address a host `Class` stands for, if any.
    pub fn guest_class(&self, host: u64) -> Option<u64> {
        self.to_guest.get(&host).copied()
    }

    /// [`ObjcRuntime::host_class`], leaving anything that is not a guest class
    /// alone.
    ///
    /// The form the seam actually wants: a pointer crossing out is a class or it
    /// is not, and an object — which is a host allocation on both sides — must
    /// pass through untouched. See [`dispatch`].
    pub fn host_or(&self, va: u64) -> u64 {
        self.host_class(va).unwrap_or(va)
    }

    /// [`ObjcRuntime::guest_class`], with the same rule for the other direction.
    pub fn guest_or(&self, host: u64) -> u64 {
        self.guest_class(host).unwrap_or(host)
    }

    pub fn stats(&self) -> &Stats {
        &self.stats
    }

    pub fn refusals(&self) -> &[Refusal] {
        &self.refusals
    }

    /// Whether every class the guest declared was registered.
    pub fn is_complete(&self) -> bool {
        self.refusals.is_empty()
    }
}

/// Register every image's Objective-C metadata with the host runtime.
///
/// Returns `Ok` with refusals inside rather than failing: one class the runtime
/// would not take should not stop a program whose other forty are fine, and the
/// refusal names it. A hard `Err` is reserved for a guest whose memory could
/// not be read, which is not a fact about Objective-C.
pub fn register(vm: &Vm) -> Result<ObjcRuntime, VmError> {
    let mut rt = ObjcRuntime::default();

    // Dependencies before dependents, as `startup` runs initializers: an image
    // may subclass a class defined by one it links against.
    let images: Vec<&ObjcImage> = vm
        .program
        .report
        .images
        .iter()
        .rev()
        .filter_map(|i| i.objc.as_ref())
        .collect();

    // Protocols first — a class adopts them as it is registered.
    for image in &images {
        for p in &image.protocols {
            register_protocol(&mut rt, p);
        }
    }

    // Then classes, superclass-first. The index is built across every image so
    // a subclass can find a superclass declared in another one.
    let by_va: HashMap<u64, &ObjcClass> = images
        .iter()
        .flat_map(|i| i.classes.iter())
        .map(|c| (c.va, c))
        .collect();
    for image in &images {
        for c in &image.classes {
            let mut chain = HashSet::new();
            register_class(vm, &mut rt, &by_va, c, &mut chain);
        }
    }

    // Ivar offsets, once every class is realized and the runtime has laid them
    // out. Separate from registration because a class's own offsets are not
    // final until `objc_registerClassPair`.
    for image in &images {
        for c in &image.classes {
            patch_ivar_offsets(vm, &mut rt, c)?;
        }
    }

    // Categories last: one may extend a class registered above.
    for image in &images {
        for c in &image.categories {
            register_category(vm, &mut rt, c);
        }
    }

    // Selector uniquing, which is independent of all of the above: it rewrites
    // the guest's own `__objc_selrefs` slots from string pointers to `SEL`s.
    for image in &images {
        rt.stats.selectors += unique_selectors(vm, image)?;
    }

    Ok(rt)
}

/// Call every `+load` the guest declares, superclass first.
///
/// The one Objective-C method that runs *because the image was loaded* rather
/// than because somebody sent a message, and programs use it for exactly that:
/// registering a class with a factory, installing a swizzle, setting a flag a
/// later `main` reads. Nothing else calls it, so a runtime that skips it runs a
/// program whose setup silently did not happen.
///
/// Called from [`crate::startup`] between registration and the initializers,
/// which is dyld's order: `+load` runs as soon as its image's classes are
/// realized, before any C constructor in the same image.
///
/// ## Order, as far as it is defined
///
/// A class's `+load` runs after its superclass's, which is the only ordering
/// guarantee the language makes and the only one worth reproducing — a subclass
/// whose superclass has not been prepared is the case programs actually depend
/// on. Categories come last, as they do on a real launch. Beyond that the order
/// is this pass's, and a program relying on more than that would be relying on
/// dyld's link order.
pub fn call_load_methods(vm: &Vm, rt: &ObjcRuntime) -> Result<usize, VmError> {
    let images: Vec<&ObjcImage> = vm
        .program
        .report
        .images
        .iter()
        .rev()
        .filter_map(|i| i.objc.as_ref())
        .collect();
    let by_va: HashMap<u64, &ObjcClass> = images
        .iter()
        .flat_map(|i| i.classes.iter())
        .map(|c| (c.va, c))
        .collect();

    let mut done = HashSet::new();
    let mut called = 0;
    for image in &images {
        for class in &image.classes {
            called += load_class(vm, rt, &by_va, class, &mut done)?;
        }
    }
    // A category's `+load` is a method on the target class's metaclass and is
    // called even when the class itself has one — both run, which is the one
    // place Objective-C does not collapse a category over a class.
    for image in &images {
        for cat in &image.categories {
            for m in &cat.class_methods {
                if m.name == "load" && vm.program.is_guest_address(m.imp) {
                    let target = rt.host_or(cat.class);
                    vm.call(m.imp, &[target, load_selector()])?;
                    called += 1;
                }
            }
        }
    }
    Ok(called)
}

/// One class's `+load`, its superclass's first.
fn load_class(
    vm: &Vm,
    rt: &ObjcRuntime,
    by_va: &HashMap<u64, &ObjcClass>,
    class: &ObjcClass,
    done: &mut HashSet<u64>,
) -> Result<usize, VmError> {
    if !done.insert(class.va) {
        return Ok(0);
    }
    let mut called = 0;
    if let Some(parent) = by_va.get(&class.superclass).copied() {
        called += load_class(vm, rt, by_va, parent, done)?;
    }
    // Only a class this pass registered: a refused class has no realized
    // metaclass, and running its `+load` would be running a method of an object
    // that does not exist.
    if rt.host_class(class.va).is_none() {
        return Ok(called);
    }
    let Some(meta) = &class.meta else {
        return Ok(called);
    };
    for m in &meta.ro.methods {
        if m.name == "load" && vm.program.is_guest_address(m.imp) {
            // `self` is the class as the *guest* names it, which is the rule
            // everywhere: guest code sees guest addresses, and a `+load` that
            // messages `self` gets it translated at the seam like any other.
            vm.call(m.imp, &[class.va, load_selector()])?;
            called += 1;
        }
    }
    Ok(called)
}

/// The uniqued `load` selector, as the second argument every method takes.
fn load_selector() -> u64 {
    // SAFETY: a NUL-terminated literal; the returned `SEL` is interned by the
    // runtime for the process lifetime.
    unsafe { host::sel_registerName(c"load".as_ptr()) as u64 }
}

/// A failed write into the guest's own metadata, named with the address.
///
/// Distinct from every other error here: the refusals above are facts about
/// Objective-C, and this is a fact about memory.
fn memory_error(addr: u64) -> VmError {
    VmError::Abi(crate::abi::AbiError::Memory {
        symbol: "objc metadata".to_string(),
        addr,
    })
}

/// Create one class, registering its superclass first if that is also a guest's.
///
/// `chain` is the set of classes already being registered further up this
/// recursion, which is what makes a cycle a named refusal rather than a stack
/// overflow. The graph is read out of guest memory, so it is not the compiler's
/// word that it is acyclic.
fn register_class(
    vm: &Vm,
    rt: &mut ObjcRuntime,
    by_va: &HashMap<u64, &ObjcClass>,
    class: &ObjcClass,
    chain: &mut HashSet<u64>,
) -> Option<u64> {
    if let Some(&already) = rt.to_host.get(&class.va) {
        return Some(already);
    }
    if !chain.insert(class.va) {
        rt.refusals.push(Refusal::SuperclassCycle {
            name: class.ro.name.clone(),
        });
        return None;
    }

    let name = class.ro.name.clone();

    // A Swift class's `bits` carries one of the two Swift markers, and what it
    // points at is Swift metadata rather than a `class_ro_t`. M6.1 decodes it
    // as one regardless, because a decoder that guessed would be worse;
    // refusing here is the honest end of that.
    if class.is_swift() {
        rt.refusals.push(Refusal::SwiftClass { name });
        return None;
    }

    let Some(c_name) = host::c_string(&name) else {
        rt.refusals.push(Refusal::UnrepresentableName { name });
        return None;
    };

    // The superclass is a bind, so it is either a host `Class` already or the
    // address of another guest class this pass has to have created first.
    let superclass = if class.ro.is_root() {
        std::ptr::null_mut()
    } else if let Some(&host_super) = rt.to_host.get(&class.superclass) {
        host_super as host::Class
    } else if let Some(guest_super) = by_va.get(&class.superclass).copied() {
        match register_class(vm, rt, by_va, guest_super, chain) {
            Some(c) => c as host::Class,
            None => {
                rt.refusals.push(Refusal::SuperclassMissing {
                    name,
                    superclass: class.superclass,
                });
                return None;
            }
        }
    } else if class.superclass != 0 && !vm.program.is_guest_address(class.superclass) {
        // A host class, resolved by the ordinary native passthrough. This is
        // the common case: `Greeter : NSObject` binds `_OBJC_CLASS_$_NSObject`
        // to the host's own, which is exactly the class we want to derive from.
        class.superclass as host::Class
    } else {
        rt.refusals.push(Refusal::SuperclassMissing {
            name,
            superclass: class.superclass,
        });
        return None;
    };

    // SAFETY: `c_name` is NUL-terminated and outlives the call; `superclass` is
    // either null, a `Class` the host runtime gave us, or one it created above.
    let cls = unsafe { host::objc_allocateClassPair(superclass, c_name.as_ptr(), 0) };
    if cls.is_null() {
        // The runtime declines exactly one thing here, and it is worth telling
        // apart: a name already in use. A guest defining `NSString` is a
        // different problem from a guest defining something unrepresentable.
        // SAFETY: `c_name` as above.
        let taken = unsafe { !host::objc_lookUpClass(c_name.as_ptr()).is_null() };
        rt.refusals.push(if taken {
            Refusal::NameTaken { name }
        } else {
            Refusal::AllocationFailed { name }
        });
        return None;
    }

    // Ivars, before registration: the runtime lays the class out at
    // `objc_registerClassPair` and will not take one afterwards.
    let mut ivars = 0;
    for ivar in &class.ro.ivars {
        let (Some(n), Some(t)) = (host::c_string(&ivar.name), host::c_string(&ivar.types)) else {
            rt.refusals.push(Refusal::UnrepresentableName {
                name: format!("{name}.{}", ivar.name),
            });
            // SAFETY: `cls` was allocated above and not yet registered, which is
            // exactly when disposing is allowed.
            unsafe { host::objc_disposeClassPair(cls) };
            return None;
        };
        // `alignment` is already a log2 in the guest's `ivar_t`, which is what
        // `class_addIvar` wants.
        // SAFETY: strings outlive the call; `cls` is unregistered.
        let ok = unsafe {
            host::class_addIvar(
                cls,
                n.as_ptr(),
                ivar.size as usize,
                ivar.alignment.min(15) as u8,
                t.as_ptr(),
            )
        };
        if !ok {
            rt.refusals.push(Refusal::IvarRejected {
                name,
                ivar: ivar.name.clone(),
            });
            // SAFETY: as above.
            unsafe { host::objc_disposeClassPair(cls) };
            return None;
        }
        ivars += 1;
    }

    // Protocols the class adopts.
    for &p in &class.ro.protocols {
        if let Some(&proto) = rt.protocols.get(&p) {
            // SAFETY: `proto` is a `Protocol *` this pass registered.
            unsafe { host::class_addProtocol(cls, proto as host::Protocol) };
        }
    }

    // Instance methods on the class, class methods on its metaclass. A class
    // method added to the class itself is silently an instance method, which is
    // a wrong program with no diagnostic.
    // SAFETY: `cls` is a `Class`; `object_getClass` of one is its metaclass.
    let meta = unsafe { host::object_getClass(cls) };
    let no_class_methods: Vec<ObjcMethod> = Vec::new();
    let class_methods = class
        .meta
        .as_ref()
        .map(|m| &m.ro.methods)
        .unwrap_or(&no_class_methods);
    let mut methods = 0;
    for (target, list) in [(cls, &class.ro.methods), (meta, class_methods)] {
        for m in list {
            match add_method(vm, target, m) {
                Ok(carried) => {
                    methods += 1;
                    if let Some(reason) = carried {
                        rt.refusals.push(Refusal::MethodNotHostCallable {
                            name: name.to_string(),
                            method: m.name.clone(),
                            reason,
                        });
                    }
                }
                Err(reason) => {
                    rt.refusals.push(Refusal::MethodRejected {
                        name,
                        method: m.name.clone(),
                        reason,
                    });
                    // SAFETY: as above — still unregistered.
                    unsafe { host::objc_disposeClassPair(cls) };
                    return None;
                }
            }
        }
    }

    // SAFETY: `cls` was allocated by `objc_allocateClassPair` and is registered
    // exactly once; every later use is through the maps below.
    unsafe { host::objc_registerClassPair(cls) };

    let host_va = cls as u64;
    rt.to_host.insert(class.va, host_va);
    rt.to_guest.insert(host_va, class.va);
    // The metaclass is a class the guest names too. `[super description]` inside
    // a `+` method puts the guest's *metaclass* address in the `objc_super`
    // struct, and a message send that could not translate it would look a method
    // up on a structure the host runtime has never realized. It costs one map
    // entry to make the pair total over both halves of a class.
    if let Some(declared) = &class.meta {
        rt.to_host.insert(declared.va, meta as u64);
        rt.to_guest.insert(meta as u64, declared.va);
    }
    rt.stats.classes += 1;
    rt.stats.methods += methods;
    rt.stats.ivars += ivars;
    chain.remove(&class.va);
    Some(host_va)
}

/// Install one guest method on `target`, with a thunk in place of its `IMP`.
///
/// The thunk is the whole reason host→guest dispatch needs no interception: the
/// host's own `objc_msgSend` finds this and calls it as native arm64, exactly
/// as the host's `qsort` calls a guest comparator.
fn add_method(
    vm: &Vm,
    target: host::Class,
    m: &ObjcMethod,
) -> Result<Option<&'static str>, &'static str> {
    let (Some(name), Some(types)) = (host::c_string(&m.name), host::c_string(&m.types)) else {
        return Err("the selector or type encoding has an interior NUL");
    };
    if !vm.program.is_guest_address(m.imp) {
        return Err("the implementation is not a guest address");
    }
    // The encoding is right here, and it is what makes this callback different
    // from every other one: a bare function pointer says nothing about its own
    // shape, but a method carries its types in the class metadata. Used, the
    // trampoline places an `NSRect` argument on the guest's stack where SysV
    // puts it. Unused — as it was — the guest reads a stack slot nobody wrote,
    // which is not a fault and not a diagnostic, just a garbage rectangle that
    // becomes a garbage pointer somewhere further on.
    //
    // A shape the typed trampoline still cannot express falls back to the
    // generic one and is *listed*, because the guest calls it perfectly well
    // even when the host cannot. See [`Refusal::MethodNotHostCallable`].
    let (imp, carried) = match typed_thunk(vm, m) {
        Ok(imp) => (imp, Ok(())),
        Err(reason) => (
            vm.thunk_for(m.imp)
                .map_err(|_| "a host->guest thunk could not be built")?,
            Err(reason),
        ),
    };

    // SAFETY: both strings outlive the call; `imp` is an executable arm64
    // trampoline this runtime built and never frees.
    let sel = unsafe { host::sel_registerName(name.as_ptr()) };
    // SAFETY: as above.
    let ok = unsafe { host::class_addMethod(target, sel, imp as host::Imp, types.as_ptr()) };
    // `false` means the class already has the selector, which for a freshly
    // allocated class means the guest declared it twice.
    if ok {
        Ok(carried.err())
    } else {
        Err("the class already responds to that selector")
    }
}

/// Build a trampoline from the method's own type encoding.
///
/// `Err` is a `&'static str` in the register the rest of this project uses for
/// refusals — it names what about *this* method could not be carried, so the
/// listing says "returns an aggregate by value" rather than "unsupported".
///
/// What is still refused, and each is a work item rather than an unknown:
///
/// - **An aggregate return.** AAPCS64 sends a homogeneous float aggregate back
///   in `v0`–`v3` while the guest returns through a hidden buffer, and "return
///   four doubles in four vector registers" is not something Cranelift's return
///   lowering promises to do the way the ABI specifies. Emitting it and hoping
///   is precisely the plausible wrong answer this module exists to avoid.
/// - **A block argument**, which is a structure with a guest function pointer
///   inside it. Carrying one means building a second thunk for its invoke
///   function at the offset the block ABI puts it, which is its own work.
/// - **An aggregate too large for AAPCS64's registers**, which the host copies
///   to its *own* stack; the trampoline would have to find it there.
fn typed_thunk(vm: &Vm, m: &ObjcMethod) -> Result<u64, &'static str> {
    let signature = encoding::signature(&m.types)?;
    let (returns, params) = signature
        .shapes
        .split_first()
        .ok_or("the type encoding describes no return value")?;
    // `Scalar(None)` is how the decoder spells `void`; a thunk returning
    // nothing declares no return rather than an ignored zero.
    let returns = match returns {
        crate::abi::aggregate::Shape::Scalar(crate::abi::ArgType::None) => None,
        other => Some(other.clone()),
    };
    let signature = crate::thunk::ThunkSignature::new(params.to_vec(), returns)?;
    vm.typed_thunk_for(m.imp, signature)
        .map_err(|_| "a host->guest thunk could not be built")
}

/// Recreate a protocol the guest declares.
///
/// A protocol the *host* already has (`NSCopying`) is reused rather than
/// recreated: `objc_allocateProtocol` returns null for a name in use, and two
/// protocols of one name would make `conformsToProtocol:` depend on which was
/// asked about.
fn register_protocol(rt: &mut ObjcRuntime, p: &ObjcProtocol) {
    let Some(name) = host::c_string(&p.name) else {
        rt.refusals.push(Refusal::UnrepresentableName {
            name: p.name.clone(),
        });
        return;
    };
    // SAFETY: `name` is NUL-terminated and outlives the call.
    let existing = unsafe { host::objc_getProtocol(name.as_ptr()) };
    if !existing.is_null() {
        rt.protocols.insert(p.va, existing as u64);
        return;
    }
    // SAFETY: as above.
    let proto = unsafe { host::objc_allocateProtocol(name.as_ptr()) };
    if proto.is_null() {
        return;
    }

    for (list, required, instance) in [
        (&p.instance_methods, true, true),
        (&p.class_methods, true, false),
        (&p.optional_instance_methods, false, true),
        (&p.optional_class_methods, false, false),
    ] {
        for m in list {
            let (Some(n), Some(t)) = (host::c_string(&m.name), host::c_string(&m.types)) else {
                continue;
            };
            // SAFETY: strings outlive the call; `proto` is unregistered, which
            // is when descriptions may be added.
            unsafe {
                let sel = host::sel_registerName(n.as_ptr());
                host::protocol_addMethodDescription(proto, sel, t.as_ptr(), required, instance);
            }
        }
    }
    for &inc in &p.protocols {
        if let Some(&other) = rt.protocols.get(&inc) {
            // SAFETY: `other` is a `Protocol *` registered by this pass.
            unsafe { host::protocol_addProtocol(proto, other as host::Protocol) };
        }
    }

    // SAFETY: `proto` came from `objc_allocateProtocol` and is registered once.
    unsafe { host::objc_registerProtocol(proto) };
    rt.protocols.insert(p.va, proto as u64);
    rt.stats.protocols += 1;
}

/// Attach a category's methods to its target class.
///
/// The target is a bind, so a category on `NSString` names the host's class and
/// needs no translation at all — `class_addMethod` on it is precisely what the
/// runtime would have done. Only the `IMP` is ours.
fn register_category(vm: &Vm, rt: &mut ObjcRuntime, cat: &ObjcCategory) {
    let target = if let Some(&registered) = rt.to_host.get(&cat.class) {
        registered as host::Class
    } else if cat.class != 0 && !vm.program.is_guest_address(cat.class) {
        cat.class as host::Class
    } else {
        rt.refusals.push(Refusal::CategoryTargetMissing {
            name: cat.name.clone(),
            class: cat.class,
        });
        return;
    };
    // SAFETY: `target` is a `Class` — either the host's own or one registered
    // above — so its metaclass is what `object_getClass` returns.
    let meta = unsafe { host::object_getClass(target) };

    let mut added = 0;
    for (on, list) in [(target, &cat.instance_methods), (meta, &cat.class_methods)] {
        for m in list {
            match add_method(vm, on, m) {
                Ok(carried) => {
                    added += 1;
                    if let Some(reason) = carried {
                        rt.refusals.push(Refusal::MethodNotHostCallable {
                            name: cat.name.clone(),
                            method: m.name.clone(),
                            reason,
                        });
                    }
                }
                Err(reason) => rt.refusals.push(Refusal::MethodRejected {
                    name: cat.name.clone(),
                    method: m.name.clone(),
                    reason,
                }),
            }
        }
    }
    rt.stats.methods += added;
    rt.stats.categories += 1;
}

/// Write the runtime's ivar offsets back into the guest's own `__objc_ivar`.
///
/// The fixup dyld performs, and the reason M6.1 decoded `offset_va` rather than
/// just the offset. A guest whose superclass came from another image loads its
/// ivar offsets from these words; one whose layout was statically known does
/// not, and for that one this write changes nothing — which is why the test
/// asserts the two agree rather than that the write happened.
fn patch_ivar_offsets(vm: &Vm, rt: &mut ObjcRuntime, class: &ObjcClass) -> Result<(), VmError> {
    let Some(&cls) = rt.to_host.get(&class.va) else {
        return Ok(());
    };
    let mut view = vm.view();
    for ivar in &class.ro.ivars {
        if ivar.offset_va == 0 {
            continue;
        }
        let Some(name) = host::c_string(&ivar.name) else {
            continue;
        };
        // SAFETY: `cls` is a registered `Class`; `name` outlives the call.
        let found = unsafe { host::class_getInstanceVariable(cls as host::Class, name.as_ptr()) };
        if found.is_null() {
            continue;
        }
        // SAFETY: `found` is an `Ivar` the runtime just returned for `cls`.
        let offset = unsafe { host::ivar_getOffset(found) };

        // The guest's word is 32 bits — `int32_t`, per `ivar_t` — and a negative
        // or oversized offset would mean the runtime laid out something this
        // does not understand, so it is refused rather than truncated.
        let Ok(offset) = u32::try_from(offset) else {
            continue;
        };
        view.write(ivar.offset_va, &offset.to_le_bytes())
            .map_err(|_| memory_error(ivar.offset_va))?;
        rt.stats.offsets_patched += 1;
    }
    Ok(())
}

/// Turn every `__objc_selrefs` slot from a string pointer into a `SEL`.
///
/// Guest code loads a selector straight out of one of these slots
/// (`movq selref(%rip), %rsi`) and passes it to `objc_msgSend`, so until this
/// runs the guest is passing a `char *` where the runtime expects a uniqued
/// selector. On a real launch dyld's `libobjc` does exactly this rewrite.
///
/// The slot's *current* contents are a pointer to the name — which is why this
/// reads before it writes, rather than working from the decoded method lists:
/// a selector may be referenced without any class in this image implementing it.
fn unique_selectors(vm: &Vm, image: &ObjcImage) -> Result<usize, VmError> {
    let mut uniqued = 0;
    let mut view = vm.view();
    for &slot in &image.selector_refs {
        let mut buf = [0u8; 8];
        if view.read(slot, &mut buf).is_err() {
            continue;
        }
        let name_va = u64::from_le_bytes(buf);
        if name_va == 0 {
            continue;
        }
        let Ok(bytes) = (&view as &dyn GuestMemory).read_cstr(name_va, "objc selector") else {
            continue;
        };
        let Ok(name) = std::ffi::CString::new(bytes) else {
            continue;
        };
        // SAFETY: `name` is NUL-terminated and outlives the call. The returned
        // `SEL` is interned by the runtime for the process lifetime.
        let sel = unsafe { host::sel_registerName(name.as_ptr()) };
        view.write(slot, &(sel as u64).to_le_bytes())
            .map_err(|_| memory_error(slot))?;
        uniqued += 1;
    }
    Ok(uniqued)
}

/// Unique the selector references of an image the *guest* loaded, and nothing
/// else about it.
///
/// [`register`] is the launch-time pass over the program's own images, and it
/// does five things; this does one of them, for a `dlopen`ed image, because that
/// one is separable and the other four are not.
///
/// **Why it cannot wait for the rest.** A selector reference is not metadata the
/// runtime consults — it is a slot *guest code loads from*
/// (`movq selref(%rip), %rsi`) on its way into `objc_msgSend`. Until it is
/// rewritten it holds a `char *`, so every message a loaded image sends arrives
/// with a string pointer where a `SEL` belongs, and the host runtime answers
/// "that class does not respond to that selector" — for `+[NSProcessInfo
/// processInfo]`, which of course it does. dyld's `libobjc` performs exactly
/// this rewrite when it maps an image, so doing it here is matching the platform
/// rather than working around it.
///
/// What is still not done for a loaded image is registration proper: its classes
/// are not realized, its categories are not attached, and its `+load` does not
/// run. Those are refused by name when reached ([`dispatch`]), which is a
/// different failure from a message that should have worked — and this closes
/// only the second kind. Messages *to Foundation*, which is what a library like
/// `steamclient.dylib` actually sends, work after this and did not before.
pub(crate) fn unique_loaded_selectors(vm: &Vm, report: &LinkReport) -> Result<usize, VmError> {
    let mut uniqued = 0;
    for image in report.images.iter().filter_map(|i| i.objc.as_ref()) {
        uniqued += unique_selectors(vm, image)?;
    }
    Ok(uniqued)
}
