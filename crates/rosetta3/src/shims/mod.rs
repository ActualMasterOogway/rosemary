//! Hand-written implementations for imports that cannot be passed through.
//!
//! ## Why variadics need code rather than a mechanism
//!
//! Everything else in [`crate::abi`] is one marshaller and a table. A variadic
//! call cannot join it, and not for want of effort: SysV AMD64 passes variadic
//! arguments in the *same registers* as fixed ones with `al` holding the count
//! of vector registers used, while Apple's arm64 ABI passes every variadic
//! argument on the **stack**. Forwarding one therefore means knowing how many
//! arguments there are and of what type — which for `printf` means parsing the
//! format string, and for `open`/`fcntl`/`ioctl` means interpreting the flag
//! argument. There is no general answer, so each family is written out.
//!
//! Microlift already demotes a variadic import to a slow VM exit, which is
//! exactly the interception point these need.
//!
//! ## Rendering rather than relaying
//!
//! The `printf` family formats *here*, in Rust, and hands the host only the
//! finished bytes. Relaying to the host's `vsnprintf` would mean synthesizing
//! an arm64 `va_list`, whose layout is an implementation detail we would be
//! guessing at from the outside; rendering makes the conversion specification
//! the C standard rather than a struct layout. The cost is that the supported
//! conversions are the ones written in [`printf`], and anything else renders
//! literally — see that module's scope note.
//!
//! ## Scope
//!
//! Implemented: `printf`, `fprintf`, `sprintf`, `snprintf`, `ioctl`, the
//! `open`/`openat`/`fcntl` group, and `__cxa_atexit` — the last of which is not
//! variadic and is here for the third reason a shim exists: one of its
//! arguments describes an *image*, and the guest's answer is not one the host
//! can be told. See [`atexit`]. Three families live in modules of their own
//! and are reached from here: [`crate::jmpbuf`], [`crate::unwind`],
//! [`crate::signal`], and [`crate::objc::dispatch`] — each because what it
//! stands in for is a stack, a personality routine, a callback crossing back
//! in, or a message, rather than a calling convention.
//! Everything else variadic is *classified* by [`crate::abi::table`] and refused by name with
//! [`AbiError::NoShim`] until someone needs it — a refusal that names the
//! symbol is a work item; a guess is a bug report from a user.

use microlift::arch::x86_64::context::RegisterContext;

use crate::abi::{AbiError, GuestMemory, Result};

pub mod atexit;
pub mod audiounit;
pub mod ioctl;
pub mod math;
pub mod nslog;
pub mod openat;
pub mod printf;
pub mod runtime;
pub mod zlib;

/// Run the shim for `symbol`, if there is one.
///
/// `None` means "not shimmed" and the caller should carry on with the table;
/// `Some(Err(..))` means the shim itself refused, which is a different thing
/// and must not fall through to a passthrough call.
pub fn dispatch(
    symbol: &str,
    ctx: &mut RegisterContext,
    mem: &mut dyn GuestMemory,
    callbacks: &dyn crate::abi::Callbacks,
) -> Option<Result<()>> {
    let name = symbol.strip_prefix('_').unwrap_or(symbol);
    let f = match name {
        "printf" => printf::printf,
        "fprintf" => printf::fprintf,
        "sprintf" => printf::sprintf,
        "snprintf" => printf::snprintf,
        // The `_FORTIFY_SOURCE` variants clang emits by default on macOS. They
        // are the *same call* with a compile-time destination size threaded
        // through, and a guest built with default flags imports these rather
        // than the plain names — so leaving them out would mean the shim never
        // fires for the programs that actually exist.
        "__sprintf_chk" => printf::sprintf_chk,
        "__snprintf_chk" => printf::snprintf_chk,
        // The `v`-prefixed family. Not variadic — each is handed a `va_list`
        // its caller built — and impossible to forward for the mirror-image
        // reason: the structure is *SysV AMD64's*, and the host's would read it
        // as arm64's. See [`printf::VaList`].
        "vprintf" => printf::vprintf,
        "vfprintf" => printf::vfprintf,
        "vsprintf" => printf::vsprintf,
        "vsnprintf" => printf::vsnprintf,
        "vasprintf" => printf::vasprintf,
        "__vsnprintf_chk" => printf::vsnprintf_chk,
        // Variadic in its declaration and one argument wide in fact; see
        // [`ioctl`] for why that makes it forwardable when `printf` is not.
        "NSLog" => nslog::nslog,
        "ioctl" => ioctl::ioctl,
        // Variadic in their declarations and one argument wide in fact, decided
        // by a flag word rather than a format string; see [`openat`].
        "open" => openat::open,
        "openat" => openat::openat,
        "fcntl" => openat::fcntl,
        // Not variadic, and here for the same underlying reason: forwarding it
        // would operate on the *host's* registers and the host's stack. See
        // [`crate::jmpbuf`], which is a module of its own because a stack is not
        // a calling convention.
        _ => {
            if let Some(f) = math::shim_for(name)
                .or_else(|| crate::jmpbuf::shim_for(name))
                .or_else(|| crate::unwind::shim_for(name))
                .or_else(|| crate::objc::dispatch::shim_for(name))
                .or_else(|| crate::dyld::shim_for(name))
            {
                return Some(f(name, ctx, mem));
            }
            // And the three families that need one thing more than the rest: a
            // guest function pointer the shim itself must substitute, because
            // the marshaller is not the one placing it. See [`crate::signal`]
            // for a disposition that also crosses back *in*, [`atexit`] for a
            // destructor whose third argument is a DSO handle that must not,
            // and [`zlib`] and [`audiounit`] for pointers that are not
            // arguments at all — they are fields of a structure the guest
            // passes by address.
            return crate::signal::shim_for(name)
                .or_else(|| atexit::shim_for(name))
                .or_else(|| zlib::shim_for(name))
                .or_else(|| audiounit::shim_for(name))
                .map(|f| f(name, ctx, mem, callbacks));
        }
    };
    Some(f(name, ctx, mem))
}

/// Whether [`dispatch`] would handle `symbol`.
///
/// Consulted by [`crate::ImportPolicy`] before promoting an import: a shimmed
/// symbol must never be inlined by the lifter, because the whole reason it has a
/// shim is that reaching the host function is wrong.
pub fn is_shimmed(symbol: &str) -> bool {
    let name = symbol.strip_prefix('_').unwrap_or(symbol);
    if math::shim_for(name).is_some()
        || crate::jmpbuf::shim_for(name).is_some()
        || crate::unwind::shim_for(name).is_some()
        || crate::objc::dispatch::shim_for(name).is_some()
        || crate::dyld::shim_for(name).is_some()
        || crate::signal::shim_for(name).is_some()
        || atexit::shim_for(name).is_some()
        || zlib::shim_for(name).is_some()
        || audiounit::shim_for(name).is_some()
    {
        return true;
    }
    matches!(
        name,
        "NSLog"
            | "printf"
            | "fprintf"
            | "sprintf"
            | "snprintf"
            | "__sprintf_chk"
            | "__snprintf_chk"
            | "vprintf"
            | "vfprintf"
            | "vsprintf"
            | "vsnprintf"
            | "vasprintf"
            | "__vsnprintf_chk"
            | "ioctl"
            | "open"
            | "openat"
            | "fcntl"
    )
}

/// Write the guest's `int` return value.
pub(crate) fn set_int_return(ctx: &mut RegisterContext, value: i32) {
    ctx.rax = value as i64 as u64;
}

/// Report a guest pointer that could not be followed.
pub(crate) fn bad_pointer(symbol: &str, addr: u64) -> AbiError {
    AbiError::Memory {
        symbol: symbol.to_string(),
        addr,
    }
}
