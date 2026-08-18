//! Rosetta 3: the driver that joins [`machish`] to [`microlift`].
//!
//! [`Vm`] is the loop: lift the region control is about to reach, enter it, service
//! the exit, repeat. [`region`] decides which bytes a region is, [`cache`] keeps the
//! translations, [`stack`] is the guest's stack and its process arguments, [`thunk`]
//! is a native trampoline per guest function pointer handed outward, and [`startup`]
//! runs the initializers and then `main`.
//!
//! A guest thread is a host thread, so [`thread`] holds the per-thread state (a guest
//! stack, the live stack pointer, the callback depth) and everything in [`Vm`] is
//! shared: a translation does not depend on which thread runs it, and a guest pointer
//! must map to the same thunk for every thread that sees it. [`tls`] answers
//! `_Thread_local` with a block allocated per thread on first touch.
//!
//! Registering an import stops the recompiler following a call out of the guest;
//! *servicing* one needs a signature, which no Mach-O records. [`abi`] is where that
//! comes from, and an import with no signature is refused by name rather than guessed
//! at. [`shims`] holds the calls that cannot be forwarded generically.
//!
//! Passthrough is free for anything operating on memory, because memory is shared. It
//! stops being free for anything operating on a *stack*: the host would save, walk or
//! restore arm64 frames belonging to the JIT rather than the guest frames the program
//! means, without faulting. [`jmpbuf`] is that case.

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
