//! Hand-written implementations for imports that cannot be passed through.
//!
//! A shim exists because the call cannot be *forwarded*: it needs the guest's
//! registers or the guest's memory, which a host function reached through an import
//! slot has neither of. A call that can be forwarded but means something different on
//! this host is a different problem, and belongs in [`darwin`].
//!
//! The families gated to macOS below are gated because what they bridge is macOS: a
//! guest that imports `objc_msgSend` or `AudioUnitRender` on Linux is refused at the
//! import, by name, rather than half an hour into its run.

use microlift::arch::x86_64::context::RegisterContext;

use crate::abi::{AbiError, GuestMemory, Result};

pub mod atexit;
/// Not gated: the callback substitution is this runtime's business on either
/// host, and which implementation the call lands on is decided inside.
pub mod audiounit;
pub mod block;
/// Not gated: which registry the legacy spellings reach is decided inside, the
/// same way [`audiounit`]'s is.
pub mod component;
pub mod corefoundation;
pub mod curl;
pub(crate) mod host;
pub mod intdiv;
pub mod ioctl;
pub mod math;
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

#[cfg(target_os = "macos")]
pub mod appkit;
#[cfg(target_os = "macos")]
pub mod cfuuid;
#[cfg(target_os = "macos")]
pub mod coregraphics;
#[cfg(target_os = "macos")]
pub mod coremedia;
#[cfg(target_os = "macos")]
pub mod iokit;
#[cfg(target_os = "macos")]
pub mod machthread;

/// Not gated: `NSLog`'s two complications are a variadic tail and an `NSString`
/// format, and both are this runtime's problem on either host. What it reaches
/// for - CoreFoundation, and Foundation's own `NSLog` - it reaches by `dlsym`
/// and does without when there is none. See [`nslog::emit`].
pub mod nslog;
pub mod variadic;

/// A shim that reads and writes the guest's registers and memory.
pub type Shim = fn(&str, &mut RegisterContext, &mut dyn GuestMemory) -> Result<()>;

/// A shim that may also call *into* the guest.
pub type CallbackShim =
    fn(&str, &mut RegisterContext, &mut dyn GuestMemory, &dyn crate::abi::Callbacks) -> Result<()>;

/// The families this host has, whichever host it is.
///
/// The Objective-C runtime is here on both, because [`crate::objc::host`] chooses
/// one rather than requiring the host to be one.
fn platform_shim_for(name: &str) -> Option<Shim> {
    crate::objc::dispatch::shim_for(name)
        .or_else(|| component::shim_for(name))
        .or_else(|| macos_shim_for(name))
}

/// The macOS-only families, which do not exist on a host with no CoreFoundation
/// and no Mach.
#[cfg(target_os = "macos")]
fn macos_shim_for(name: &str) -> Option<Shim> {
    coregraphics::shim_for(name)
        .or_else(|| coremedia::shim_for(name))
        .or_else(|| cfuuid::shim_for(name))
        .or_else(|| appkit::shim_for(name))
        .or_else(|| machthread::shim_for(name))
        // The overlay's OpenGL seam. Only present when the overlay is on;
        // see [`crate::gui::hook::shim_for`].
        .or_else(|| crate::gui::hook::shim_for(name))
}

#[cfg(not(target_os = "macos"))]
fn macos_shim_for(_name: &str) -> Option<Shim> {
    None
}

/// The families that may call into the guest, on either host.
fn callback_shim_for(name: &str) -> Option<CallbackShim> {
    corefoundation::shim_for(name)
        .or_else(|| audiounit::shim_for(name))
        .or_else(|| macos_callback_shim_for(name))
}

/// The macOS-only families that may call into the guest.
#[cfg(target_os = "macos")]
fn macos_callback_shim_for(name: &str) -> Option<CallbackShim> {
    iokit::shim_for(name).or_else(|| coremedia::callback_shim_for(name))
}

#[cfg(not(target_os = "macos"))]
fn macos_callback_shim_for(_name: &str) -> Option<CallbackShim> {
    None
}

/// The two variadics whose format string is an `NSString`, and so are Foundation's.
fn foundation_variadic(name: &str) -> Option<Shim> {
    match name {
        "NSLog" => Some(nslog::nslog),
        // The other `NSString`-format variadic, and the one a Cocoa game reports a
        // fatal error through. See [`nslog::run_alert_panel`].
        "NSRunAlertPanel" => Some(nslog::run_alert_panel),
        _ => None,
    }
}

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
        // The `v`-prefixed family.
        "vprintf" => printf::vprintf,
        "vfprintf" => printf::vfprintf,
        "vsprintf" => printf::vsprintf,
        "vsnprintf" => printf::vsnprintf,
        // The wide form, which is how Unreal Engine 3 formats every string it
        // has.
        "vswprintf" => printf::vswprintf,
        "vasprintf" => printf::vasprintf,
        "__vsnprintf_chk" => printf::vsnprintf_chk,
        "sscanf" => sscanf::sscanf,
        // The locale-aware and `va_list` spellings, which are the same parser
        // reached with the destinations somewhere else.
        "sscanf_l" => sscanf::sscanf_l,
        "vsscanf" => sscanf::vsscanf,
        // libsystem_malloc's own printer, which the interposing mimalloc uses.
        "malloc_printf" => printf::malloc_printf,
        "ioctl" => ioctl::ioctl,
        // Variadic in its declaration and one argument wide in fact, decided by
        // a command number rather than a format string; see [`openat`].
        "fcntl" => openat::fcntl,
        // Not variadic, and here for the same underlying reason: forwarding it
        // would operate on the *host's* registers and the host's stack.
        _ => {
            if let Some(f) = foundation_variadic(name)
                .or_else(|| math::shim_for(name))
                .or_else(|| intdiv::shim_for(name))
                .or_else(|| process::shim_for(name))
                .or_else(|| stackprobe::shim_for(name))
                .or_else(|| sysvsem::shim_for(name))
                .or_else(|| vmmap::shim_for(name))
                .or_else(|| crate::jmpbuf::shim_for(name))
                // .or_else(|| crate::unwind::shim_for(name))
                .or_else(|| crate::dyld::shim_for(name))
                .or_else(|| platform_shim_for(name))
            {
                return Some(f(name, ctx, mem));
            }
            // And the families that need one thing more than the rest.
            return crate::signal::shim_for(name)
                .or_else(|| pthread::shim_for(name))
                .or_else(|| block::shim_for(name))
                .or_else(|| crate::dyld::shim_for_callbacks(name))
                .or_else(|| atexit::shim_for(name))
                .or_else(|| zlib::shim_for(name))
                .or_else(|| curl::shim_for(name))
                .or_else(|| callback_shim_for(name))
                .map(|f| f(name, ctx, mem, callbacks));
        }
    };
    Some(f(name, ctx, mem))
}

/// Whether [`dispatch`] would handle `symbol`.
pub fn is_shimmed(symbol: &str) -> bool {
    let name = symbol.strip_prefix('_').unwrap_or(symbol);
    if math::shim_for(name).is_some()
        || intdiv::shim_for(name).is_some()
        || process::shim_for(name).is_some()
        || stackprobe::shim_for(name).is_some()
        || sysvsem::shim_for(name).is_some()
        || vmmap::shim_for(name).is_some()
        || crate::jmpbuf::shim_for(name).is_some()
        || crate::unwind::shim_for(name).is_some()
        || crate::dyld::shim_for(name).is_some()
        || crate::signal::shim_for(name).is_some()
        || pthread::shim_for(name).is_some()
        || block::shim_for(name).is_some()
        || crate::dyld::shim_for_callbacks(name).is_some()
        || atexit::shim_for(name).is_some()
        || zlib::shim_for(name).is_some()
        || curl::shim_for(name).is_some()
        || foundation_variadic(name).is_some()
        || platform_shim_for(name).is_some()
        || callback_shim_for(name).is_some()
    {
        return true;
    }
    matches!(
        name,
        "printf"
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
