//! Hand-written implementations for imports that cannot be passed through.

use microlift::arch::x86_64::context::RegisterContext;

use crate::abi::{AbiError, GuestMemory, Result};

pub mod appkit;
pub mod atexit;
pub mod audiounit;
pub mod block;
pub mod cfuuid;
pub mod component;
pub mod coregraphics;
pub mod coremedia;
pub mod curl;
pub(crate) mod host;
pub mod intdiv;
pub mod ioctl;
pub mod iokit;
pub mod machthread;
pub mod math;
pub mod nslog;
pub mod openat;
pub mod printf;
pub mod process;
pub mod pthread;
pub mod runtime;
pub mod sscanf;
pub mod stackprobe;
pub mod sysvsem;
pub mod vmmap;
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
        // The locale-aware pair, which the **vendored libc++** uses for every
        // number it formats so that `std::to_string` does not pick up the
        // program's locale.
        "snprintf_l" => printf::snprintf_l,
        "sprintf_l" => printf::sprintf_l,
        // The `_FORTIFY_SOURCE` variants clang emits by default on macOS.
        "__sprintf_chk" => printf::sprintf_chk,
        "__snprintf_chk" => printf::snprintf_chk,
        // The `v`-prefixed family. Not variadic — each is handed a `va_list`
        // its caller built — and impossible to forward for the mirror-image
        // reason.
        "vprintf" => printf::vprintf,
        "vfprintf" => printf::vfprintf,
        "vsprintf" => printf::vsprintf,
        "vsnprintf" => printf::vsnprintf,
        // The wide form, which is how Unreal Engine 3 formats every string it
        // has. See [`printf::vswprintf`] for why its truncation return is not
        // `vsnprintf`'s.
        "vswprintf" => printf::vswprintf,
        "vasprintf" => printf::vasprintf,
        "__vsnprintf_chk" => printf::vsnprintf_chk,
        "sscanf" => sscanf::sscanf,
        // The locale-aware and `va_list` spellings, which are the same parser
        // reached with the destinations somewhere else. See [`sscanf::vsscanf`]
        // for why the list cannot be handed to the host.
        "sscanf_l" => sscanf::sscanf_l,
        "vsscanf" => sscanf::vsscanf,
        // libsystem_malloc's own printer, which the interposing mimalloc uses.
        "malloc_printf" => printf::malloc_printf,
        // Variadic in its declaration and one argument wide in fact; see
        // [`ioctl`] for why that makes it forwardable when `printf` is not.
        "NSLog" => nslog::nslog,
        // The other `NSString`-format variadic, and the one a Cocoa game
        // reports a fatal error through. See [`nslog::run_alert_panel`].
        "NSRunAlertPanel" => nslog::run_alert_panel,
        "ioctl" => ioctl::ioctl,
        // Variadic in its declaration and one argument wide in fact, decided by
        // a command number rather than a format string; see [`openat`].
        "fcntl" => openat::fcntl,
        // Not variadic, and here for the same underlying reason: forwarding it
        // would operate on the *host's* registers and the host's stack.
        _ => {
            if let Some(f) = math::shim_for(name)
                .or_else(|| intdiv::shim_for(name))
                .or_else(|| coregraphics::shim_for(name))
                .or_else(|| coremedia::shim_for(name))
                .or_else(|| cfuuid::shim_for(name))
                .or_else(|| appkit::shim_for(name))
                .or_else(|| component::shim_for(name))
                .or_else(|| process::shim_for(name))
                .or_else(|| machthread::shim_for(name))
                .or_else(|| stackprobe::shim_for(name))
                .or_else(|| sysvsem::shim_for(name))
                .or_else(|| vmmap::shim_for(name))
                .or_else(|| crate::jmpbuf::shim_for(name))
                // .or_else(|| crate::unwind::shim_for(name))
                .or_else(|| crate::objc::dispatch::shim_for(name))
                .or_else(|| crate::dyld::shim_for(name))
                // The overlay's OpenGL seam. Only present when the overlay is on;
                // see [`crate::gui::hook::shim_for`].
                .or_else(|| crate::gui::hook::shim_for(name))
            {
                return Some(f(name, ctx, mem));
            }
            // And the three families that need one thing more than the rest: a
            // guest function pointer the shim itself must substitute, because
            // the marshaller is not the one placing it.
            return crate::signal::shim_for(name)
                .or_else(|| pthread::shim_for(name))
                .or_else(|| iokit::shim_for(name))
                .or_else(|| block::shim_for(name))
                .or_else(|| coremedia::callback_shim_for(name))
                .or_else(|| crate::dyld::shim_for_callbacks(name))
                .or_else(|| atexit::shim_for(name))
                .or_else(|| zlib::shim_for(name))
                .or_else(|| audiounit::shim_for(name))
                .or_else(|| curl::shim_for(name))
                .map(|f| f(name, ctx, mem, callbacks));
        }
    };
    Some(f(name, ctx, mem))
}

/// Whether [`dispatch`] would handle `symbol`.
///
/// Consulted by [`crate::ImportPolicy`] before promoting an import.
pub fn is_shimmed(symbol: &str) -> bool {
    let name = symbol.strip_prefix('_').unwrap_or(symbol);
    if math::shim_for(name).is_some()
        || intdiv::shim_for(name).is_some()
        || coregraphics::shim_for(name).is_some()
        || coremedia::shim_for(name).is_some()
        || coremedia::callback_shim_for(name).is_some()
        || cfuuid::shim_for(name).is_some()
        || appkit::shim_for(name).is_some()
        || component::shim_for(name).is_some()
        || iokit::shim_for(name).is_some()
        || process::shim_for(name).is_some()
        || machthread::shim_for(name).is_some()
        || stackprobe::shim_for(name).is_some()
        || sysvsem::shim_for(name).is_some()
        || vmmap::shim_for(name).is_some()
        || crate::jmpbuf::shim_for(name).is_some()
        || crate::unwind::shim_for(name).is_some()
        || crate::objc::dispatch::shim_for(name).is_some()
        || crate::dyld::shim_for(name).is_some()
        || crate::signal::shim_for(name).is_some()
        || pthread::shim_for(name).is_some()
        || block::shim_for(name).is_some()
        || crate::dyld::shim_for_callbacks(name).is_some()
        || atexit::shim_for(name).is_some()
        || zlib::shim_for(name).is_some()
        || audiounit::shim_for(name).is_some()
        || curl::shim_for(name).is_some()
        || crate::gui::hook::shim_for(name).is_some()
    {
        return true;
    }
    matches!(
        name,
        "NSLog"
            | "NSRunAlertPanel"
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
            | "vswprintf"
            | "vasprintf"
            | "__vsnprintf_chk"
            | "sscanf"
            | "sscanf_l"
            | "vsscanf"
            | "malloc_printf"
            | "ioctl"
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
