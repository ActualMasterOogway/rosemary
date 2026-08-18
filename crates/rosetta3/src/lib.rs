//! Rosetta 3: the driver that joins [`machish`] to [`microlift`].

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
//! roadmap and are not built yet; both are refused by name until they are.

pub mod abi;
pub mod atomic128;
pub mod audit;
pub mod bridge;
pub mod bundle;
pub mod cache;
pub mod compat;
pub mod cxxlib;
pub mod dyld;
pub mod fault;
pub mod gui;
pub mod interpose;
pub mod jit;
pub mod jmpbuf;
pub mod lldb;
pub mod objc;
pub mod profile;
pub mod program;
pub mod region;
pub mod retstub;
pub mod shims;
pub mod signal;
pub mod stack;
pub mod startup;
pub mod stats;
pub mod thread;
pub mod thunk;
pub mod tls;
pub mod trace;
pub mod trampoline;
pub mod unwind;
pub mod vm;

pub use abi::{AbiError, Classification, call_import};
pub use audit::{CallTargetAudit, UnresolvedCall, audit_call_targets};
pub use program::{ImportPolicy, Program, ProgramError};
pub use vm::{Vm, VmError};
