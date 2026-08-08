//! The Carbon Component Manager, answered by the AudioComponent registry.
//!
//! ## The finding
//!
//! `FindNextComponent`, `OpenAComponent` and `CloseComponent` are the
//! pre-10.6 way to reach an audio unit, and UNDERTALE's runner — built against
//! the 10.13 SDK but carrying much older code — still uses them. All three are
//! still **exported** by arm64 CoreServices, so native passthrough resolves
//! them and they were ordinary signatures in [`crate::abi::table`]. They are
//! also **empty**: a `FindNextComponent` for `auou/def /appl` returns null on
//! this host while an `AudioComponentFindNext` for the identical description
//! returns a component. Checked by compiling both calls into one arm64 program
//! and running it, because "the symbol resolves" and "the call works" are
//! different claims and only the first one is visible from a link.
//!
//! The guest then says so itself — "FindNextComponent unable to find a default
//! audio output device" — and carries on with no sound. That is a *correct*
//! forward of a call that does nothing, which is the worst shape a passthrough
//! can take: nothing refuses and nothing faults.
//!
//! ## Why substituting the modern API is sound and not a guess
//!
//! The two registries are the same registry with two front doors, and the
//! types on either side are the same types:
//!
//! - `ComponentDescription` and `AudioComponentDescription` are both five
//!   32-bit fields in the same order — type, subtype, manufacturer, flags,
//!   flags mask — so the guest's descriptor is passed through by address.
//! - `Component` and `AudioComponent` are both opaque pointers, as are
//!   `ComponentInstance` and `AudioComponentInstance`.
//! - Everything the guest does *after* opening — `AudioUnitSetProperty`,
//!   `AudioUnitInitialize`, `AudioOutputUnitStart` — takes that instance and is
//!   already passed through, and takes it under both spellings.
//!
//! So the only difference between the two APIs is the error type: the modern
//! calls return `OSStatus` where `OpenAComponent` returns one too and
//! `FindNextComponent` returns the component itself. Nothing is translated
//! here; the calls are renamed.
//!
//! ## What is deliberately not here
//!
//! `GetComponentInfo`, `CountComponents` and the rest of the Component Manager.
//! They have modern counterparts as well, and no program has reached one — the
//! rule this project follows is that an entry no program exercises is not
//! evidence of anything.

use microlift::arch::x86_64::context::RegisterContext;

use crate::abi::{GuestMemory, Result};

use super::set_int_return;

/// One of this module's implementations, in the shape [`crate::shims`] wants.
pub type Shim = fn(&str, &mut RegisterContext, &mut dyn GuestMemory) -> Result<()>;

/// Whether `name` (underscore already stripped) is one of this module's.
pub fn shim_for(name: &str) -> Option<Shim> {
    match name {
        "FindNextComponent" => Some(find_next_component),
        "OpenAComponent" => Some(open_a_component),
        "CloseComponent" => Some(close_component),
        _ => None,
    }
}

/// The three modern entry points, found by `dlsym` rather than linked — the
/// same care [`audiounit`](super::audiounit) takes and for the same reason:
/// linking AudioToolbox runs its initializers before the guest starts, and they
/// leave `errno` set for a guest that reads the host's.
///
/// `RTLD_DEFAULT` is enough. This shim runs only because the guest imported a
/// Component Manager entry point, and resolving *that* import is what loaded
/// CoreServices and AudioToolbox behind it.
fn host_fn<T: Copy>(name: &core::ffi::CStr, cache: &std::sync::OnceLock<Option<T>>) -> Option<T> {
    *cache.get_or_init(|| {
        // SAFETY: a NUL-terminated name and a well-known pseudo-handle. The
        // result is either null or the framework's own function, and `T` is the
        // type `AudioComponent.h` declares for it.
        let sym = unsafe { libc::dlsym(libc::RTLD_DEFAULT, name.as_ptr()) };
        (!sym.is_null())
            .then(|| unsafe { core::mem::transmute_copy::<*mut core::ffi::c_void, T>(&sym) })
    })
}

type FindNextFn = unsafe extern "C" fn(*mut core::ffi::c_void, *const u8) -> *mut u8;
type InstanceNewFn = unsafe extern "C" fn(*mut u8, *mut *mut u8) -> i32;
type InstanceDisposeFn = unsafe extern "C" fn(*mut u8) -> i32;

/// The refusal for a host that does not export the modern registry, which no
/// macOS running arm64 code is — but a missing symbol answered with a null call
/// is the one outcome worse than saying so.
fn no_audio_component(symbol: &str) -> crate::abi::AbiError {
    crate::abi::AbiError::Refused {
        symbol: symbol.to_string(),
        reason: "AudioToolbox does not export the AudioComponent registry, so the \
                 Component Manager call it stands in for cannot be answered",
    }
}

/// `Component FindNextComponent(Component aComponent, ComponentDescription *looking)`
fn find_next_component(
    symbol: &str,
    ctx: &mut RegisterContext,
    _mem: &mut dyn GuestMemory,
) -> Result<()> {
    static FOUND: std::sync::OnceLock<Option<FindNextFn>> = std::sync::OnceLock::new();
    let Some(f) = host_fn(c"AudioComponentFindNext", &FOUND) else {
        return Err(no_audio_component(symbol));
    };
    // SAFETY: both arguments are opaque to this runtime — a component handle
    // the previous call returned, and a twenty-byte descriptor in guest memory,
    // which is identity-mapped. Neither is dereferenced here.
    let found = unsafe { f(ctx.rdi as *mut core::ffi::c_void, ctx.rsi as *const u8) };
    ctx.rax = found as u64;
    Ok(())
}

/// `OSErr OpenAComponent(Component aComponent, ComponentInstance *ci)`
fn open_a_component(
    symbol: &str,
    ctx: &mut RegisterContext,
    _mem: &mut dyn GuestMemory,
) -> Result<()> {
    static FOUND: std::sync::OnceLock<Option<InstanceNewFn>> = std::sync::OnceLock::new();
    let Some(f) = host_fn(c"AudioComponentInstanceNew", &FOUND) else {
        return Err(no_audio_component(symbol));
    };
    // SAFETY: `rdi` is a component this process's own AudioToolbox handed out,
    // and `rsi` is a pointer-sized slot in guest memory for the instance.
    let status = unsafe { f(ctx.rdi as *mut u8, ctx.rsi as *mut *mut u8) };
    set_int_return(ctx, status);
    Ok(())
}

/// `OSErr CloseComponent(ComponentInstance aComponentInstance)`
fn close_component(
    symbol: &str,
    ctx: &mut RegisterContext,
    _mem: &mut dyn GuestMemory,
) -> Result<()> {
    static FOUND: std::sync::OnceLock<Option<InstanceDisposeFn>> = std::sync::OnceLock::new();
    let Some(f) = host_fn(c"AudioComponentInstanceDispose", &FOUND) else {
        return Err(no_audio_component(symbol));
    };
    // SAFETY: the instance `OpenAComponent` produced, which is an
    // `AudioComponentInstance` because this module made it one.
    let status = unsafe { f(ctx.rdi as *mut u8) };
    set_int_return(ctx, status);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct NoMemory;

    impl GuestMemory for NoMemory {
        fn read(&self, va: u64, out: &mut [u8]) -> core::result::Result<(), machish::Error> {
            Err(machish::Error::OutOfBounds {
                addr: va,
                len: out.len(),
            })
        }
        fn write(&mut self, va: u64, data: &[u8]) -> core::result::Result<(), machish::Error> {
            Err(machish::Error::OutOfBounds {
                addr: va,
                len: data.len(),
            })
        }
    }

    /// The shim finds AudioToolbox with `RTLD_DEFAULT`, which in a real run is
    /// satisfied by the guest's own import having loaded it. A test process has
    /// no guest, so it loads the framework itself — which keeps the whole path
    /// under test, the real registry included.
    fn framework_loaded() -> bool {
        static LOADED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *LOADED.get_or_init(|| {
            let path = c"/System/Library/Frameworks/AudioToolbox.framework/AudioToolbox";
            // SAFETY: a NUL-terminated path; the handle is deliberately leaked,
            // as every `dlopen` in this runtime is.
            !unsafe { libc::dlopen(path.as_ptr(), libc::RTLD_LAZY | libc::RTLD_GLOBAL) }.is_null()
        })
    }

    /// `{ 'auou', 'def ', 'appl', 0, 0 }` — the default output unit, laid out as
    /// the guest's twenty-byte `ComponentDescription`.
    fn default_output_description() -> [u32; 5] {
        [
            u32::from_be_bytes(*b"auou"),
            u32::from_be_bytes(*b"def "),
            u32::from_be_bytes(*b"appl"),
            0,
            0,
        ]
    }

    #[test]
    fn the_default_output_unit_is_found_where_the_legacy_call_finds_nothing() {
        // The finding this module exists for, asserted rather than described:
        // the same description through the modern registry produces a
        // component. If this ever starts failing, the substitution is no longer
        // the right answer and the module should say so instead.
        if !framework_loaded() {
            return;
        }
        let desc = default_output_description();
        let mut ctx = RegisterContext::new();
        ctx.rdi = 0;
        ctx.rsi = desc.as_ptr() as u64;
        find_next_component("_FindNextComponent", &mut ctx, &mut NoMemory).unwrap();
        assert_ne!(ctx.rax, 0, "AudioComponentFindNext should find auou/def ");
    }

    #[test]
    fn a_component_opens_and_closes() {
        if !framework_loaded() {
            return;
        }
        let desc = default_output_description();
        let mut ctx = RegisterContext::new();
        ctx.rdi = 0;
        ctx.rsi = desc.as_ptr() as u64;
        find_next_component("_FindNextComponent", &mut ctx, &mut NoMemory).unwrap();
        let component = ctx.rax;
        if component == 0 {
            return;
        }

        let mut instance: *mut u8 = core::ptr::null_mut();
        ctx.rdi = component;
        ctx.rsi = (&raw mut instance) as u64;
        open_a_component("_OpenAComponent", &mut ctx, &mut NoMemory).unwrap();
        assert_eq!(ctx.rax as i32, 0, "OpenAComponent should succeed");
        assert!(!instance.is_null(), "and should write the instance out");

        ctx.rdi = instance as u64;
        close_component("_CloseComponent", &mut ctx, &mut NoMemory).unwrap();
        assert_eq!(ctx.rax as i32, 0);
    }
}
