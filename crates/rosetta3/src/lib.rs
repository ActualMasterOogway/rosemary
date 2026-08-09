//! Rosetta 3: the driver that joins [`machish`] to [`microlift`].
//!
//! `machish` produces a **transcript** of a link — where each image landed, and
//! for every import the pointer slot, the `__stubs` entry, and the host address
//! it resolved to. `microlift` recompiles x86-64 to native code and wants to
//! know, before it lifts anything, which addresses are calls out of the guest.
//! This crate is the only place the two meet, and it is deliberately thin: all
//! it does is turn the first into the second.
//!
//! ## Why the seam is here and not inside `machish`
//!
//! `machish` is a standalone, publishable Mach-O loader with zero non-optional
//! dependencies, and must stay that way. Depending on `microlift` — or on
//! `function_traits`, for the calling-convention vocabulary a fast import needs
//! — would drag a JIT's dependency graph into a `no_std` file parser. The
//! coupling is instead an opaque `u64`:
//! [`machish::report::BindingRecord::tag`], chosen by whichever
//! [`SymbolProvider`](machish::resolve::SymbolProvider) answered the import, and
//! echoed back verbatim. An embedder that wants fast imports uses the tag to
//! index its *own* shim table, where the ABI metadata already lives.
//!
//! ## Purely JIT
//!
//! Rosetta 3 translates **on demand**: a region is lifted when control is about
//! to reach it, kept in a code cache, and re-entered on the next visit.
//! Microlift can be driven as a static recompiler, and this crate deliberately
//! does not — ahead-of-time translation of a whole `__text` is intensely
//! program-specific work (which bytes are code, where functions end, which
//! indirect targets exist) that buys nothing a cache does not, and it cannot
//! cope with code the guest generates at runtime or `dlopen`s later.
//!
//! `LifterConfig::segments` is populated with the guest's mapped images all the
//! same, but as a **hint**: it lets microlift's value-set analysis resolve
//! RIP-relative loads from read-only data at lift time instead of emitting a
//! runtime memory access. That is an optimisation input, not a translation
//! plan.
//!
//! Two places look at code that is not about to run, and both are deliberate.
//! [`audit_call_targets`] is a *diagnostic* — see its module note. And
//! [`Vm::thunk_for`] translates a callback when its thunk is built, because
//! handing a pointer to host code *is* the demand signal: the host may call it
//! at a moment when compiling is impossible, and process exit is exactly that
//! moment.
//!
//! ## What "registering an import" has to cover
//!
//! Two addresses per binding, not one. A classic x86-64 Mach-O compiles a call
//! to an imported function as `call rel32` into a six-byte `__stubs` entry which
//! then does `jmp qword [rip+disp]` through the `__la_symbol_ptr` slot, and
//! microlift matches a registered import against *either* a `call rel32` target
//! or a `call [rip+disp]` displacement
//! (`microlift/src/arch/x86_64/semantics/control_flow.rs:139-142`, `:361-366`).
//!
//! Under a JIT the consequence of missing the stub is worse than it first looks,
//! and worth stating precisely. An unregistered call target is not fatal in
//! general: it exits with `ExitReason::UnknownJump`/`TransferKind::DirectCall`,
//! and the runtime simply lifts the target and carries on — that is how control
//! leaves one translation unit for the next. But if the target is an
//! unregistered `__stubs` entry, "lift the target and carry on" means lifting
//! the six-byte stub *as guest code*, following its `jmp qword [rip+disp]` into
//! the `__la_symbol_ptr` slot, and finding there a **native arm64 host
//! address** — which the JIT would then attempt to decode as x86-64. Every
//! import call in a classic image ends that way.
//!
//! So `stub_va` is not an optimisation that saves a VM exit. It is what keeps
//! the recompiler from walking off the end of the guest and into the host.

//! ## Running it
//!
//! [`Vm`] is the loop: lift the region control is about to reach, enter it,
//! service the exit, repeat. [`region`] decides which bytes a region is,
//! [`cache`] keeps the translations, [`stack`] is the guest's stack and its
//! process arguments, [`thunk`] is the mirror of import interception — a native
//! trampoline per guest function pointer handed outward — and [`startup`] runs
//! the initializers and then `main`.
//!
//! ## One guest thread per host thread
//!
//! A guest thread is a host thread: `pthread_create`'s start routine leaves as
//! an ordinary [`thunk`] and libpthread calls it on a thread of its own. What
//! that costs is a division of the runtime's state — [`thread`] holds the part
//! that is per-thread (a guest stack, the live stack pointer, the callback
//! depth), and everything in [`Vm`] is shared, because a translation does not
//! depend on which thread runs it and a guest pointer must map to the *same*
//! thunk for every thread that sees it.
//!
//! [`tls`] is the other half: a `_Thread_local` compiles to an indirect call
//! through a descriptor, and the runtime answers it with a block allocated per
//! thread on first touch. `errno` needs neither, and is worth knowing about as
//! the case where passthrough is simply *correct* — it is `__error()` returning
//! a thread-local pointer, and a guest "syscall" is the host's libSystem call on
//! the same host thread, so the two agree with no work at all.
//!
//! Every exit the loop cannot service is a **stop that names itself**: an
//! address, a symbol, or an instruction. There is no arm that guesses, for the
//! same reason [`abi`] refuses an undescribed import.
//!
//! ## Knowing an address is not knowing a call
//!
//! Registering an import stops the recompiler following a call out of the
//! guest. *Servicing* one needs its signature — how many integer arguments, how
//! many floating-point, what comes back — and no Mach-O records that. [`abi`]
//! is where that comes from: a table the embedder writes, indexed by the
//! provider's tag or by name, with a marshaller that reads the guest's register
//! context and calls the host function. Its governing decision is that an
//! import with no signature is **refused by name** rather than guessed at, and
//! [`shims`] is where the calls that cannot be forwarded generically — the
//! variadics — are written out by hand.
//!
//! ## Where passthrough stops being free
//!
//! Almost everything a guest imports operates on *memory*, and memory is shared,
//! which is why high-level emulation works at all. The exceptions are the
//! functions that operate on a **stack**: the host's implementation would save,
//! walk, or restore arm64 frames belonging to the JIT rather than the guest
//! frames the program means, and it would do so without faulting. [`jmpbuf`] is
//! the first of them — `setjmp` and `longjmp`, implemented against the guest's
//! own register context. Unwinding and signal delivery are the rest of the same
//! milestone and are not built yet; both are refused by name until they are.

pub mod abi;
pub mod audit;
pub mod bridge;
pub mod bundle;
pub mod cache;
pub mod cxxlib;
pub mod dyld;
pub mod fault;
pub mod jit;
pub mod jmpbuf;
pub mod objc;
pub mod program;
pub mod region;
pub mod shims;
pub mod signal;
pub mod stack;
pub mod startup;
pub mod stats;
pub mod thread;
pub mod thunk;
pub mod tls;
pub mod trace;
pub mod unwind;
pub mod vm;

pub use abi::{AbiError, Classification, call_import};
pub use audit::{CallTargetAudit, UnresolvedCall, audit_call_targets};
pub use program::{ImportPolicy, Program, ProgramError};
pub use vm::{Vm, VmError};
