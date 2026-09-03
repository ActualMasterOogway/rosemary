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
pub mod cache;
pub mod cxxlib;
pub mod dispatch;
pub mod dyld;
pub mod exec;
pub mod fault;
pub mod heapcheck;
pub mod host;
pub mod hostheap;
pub mod interp;
pub mod interpose;
pub mod jit;
pub mod jmpbuf;
pub mod lldb;
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

pub fn darwin_name(symbol: &str) -> &str {
    symbol.strip_prefix('_').unwrap_or(symbol)
}

/// The frameworks this tree implements, and whether they are the ones in use.
pub mod frameworks;

/// Realizing the guest's Objective-C classes, and servicing the messages it sends.
///
/// Not gated, because the runtime it talks to need not be the host's: everything
/// here goes through libobjc's public C API, and [`objc::host`] chooses between
/// the host's implementation of that API and this tree's own.
pub mod objc;
pub mod objcexc;

/// `OpenGL.framework`, bound to the GL library SDL is using.
pub mod opengl;

/// Subsystems that exist because macOS does.
///
/// [`gui`] is the debug overlay drawn through Metal and AppKit, [`profile`] a
/// sampler built on Mach thread suspension, [`bundle`] the `.app` layout the
/// host's `NSBundle` expects, and [`compat`] a `NSUserDefaults` registration.
/// None of them has a meaning on a host with none of those, and none is stubbed:
/// a guest that imports into one is refused by name at the import. See
/// [`darwin::divergent`].
#[cfg(target_os = "macos")]
pub mod bundle;
#[cfg(target_os = "macos")]
pub mod compat;
#[cfg(target_os = "macos")]
pub mod gui;
#[cfg(target_os = "macos")]
pub mod profile;

pub use abi::{AbiError, Classification, call_import};
pub use audit::{CallTargetAudit, UnresolvedCall, audit_call_targets};
pub use exec::{Engine, Execution, Native};
pub use program::{ImportPolicy, Program, ProgramError};
pub use vm::{Vm, VmError};
