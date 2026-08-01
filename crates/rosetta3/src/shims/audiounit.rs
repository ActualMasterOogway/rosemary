//! `AudioUnitSetProperty`: a guest function pointer installed as a *property*.
//!
//! ## The same problem as [`super::zlib`], one level further out
//!
//! An AudioUnit's render callback is not an argument. It is
//! `AURenderCallbackStruct { AURenderCallback inputProc; void *inputProcRefCon; }`,
//! handed to `AudioUnitSetProperty` as an opaque `const void *inData` with a
//! byte count — so the signature that describes the call correctly is
//! `(P, U32, U32, U32, P, U32)` and says nothing whatsoever about the function
//! pointer sixteen bytes away. Only the *property id* says the structure is
//! there at all, which is why this is a list of ids rather than a rule.
//!
//! What it costs to get wrong is the most alarming failure this runtime
//! produces. CoreAudio does not call the callback on the thread that installed
//! it; it calls it from `HALC_ProxyIOContext::IOWorkLoop` on
//! `com.apple.audio.IOThread.client`, a real-time thread this runtime never
//! created and knows nothing about. An unsubstituted guest pointer therefore
//! faults on a non-executable page *on another thread*, several seconds after
//! the call that installed it, with a stack that is entirely CoreAudio's.
//!
//! That the crossing works at all is [`crate::thunk`]'s doing: a thunk is a
//! process-global native trampoline, and it makes a guest stack for whatever
//! thread arrives on it. The audio IO thread is exactly the "host → guest on a
//! new thread" case, arriving without warning.
//!
//! ## Copied, not patched in place
//!
//! Unlike a `z_stream`, the property structure is *copied* by
//! `AudioUnitSetProperty` — it is `const void *` with a length, and the unit
//! owns what it keeps. So the substitution goes into a temporary of this
//! runtime's and the guest's own memory is never written. That is the better of
//! the two shapes, and it is available here only because the callee copies.

use core::ffi::c_void;

use microlift::arch::x86_64::context::RegisterContext;

use crate::abi::{Callbacks, GuestMemory, Result};

use super::{bad_pointer, set_int_return};

/// `kAudioUnitProperty_SetRenderCallback`, the one a program that plays audio
/// installs.
const SET_RENDER_CALLBACK: u32 = 23;
/// `kAudioUnitProperty_SetExternalBuffer` is *not* here; this is the deprecated
/// `kAudioUnitProperty_SetInputCallback`, which older code still sets and which
/// carries the identical structure.
const SET_INPUT_CALLBACK: u32 = 22;
/// `kAudioOutputUnitProperty_SetInputCallback`, the output-unit spelling.
const OUTPUT_SET_INPUT_CALLBACK: u32 = 2005;

/// `sizeof(AURenderCallbackStruct)`: a function pointer and a cookie.
const CALLBACK_STRUCT_SIZE: u32 = 16;

/// `kAudioUnitErr_InvalidPropertyValue`, which is what the framework answers a
/// caller whose property data is the wrong size — so a guest handing over a
/// structure this shim would misread gets its own framework's error rather than
/// a silently truncated read.
const INVALID_PROPERTY_VALUE: i32 = -10851;

/// The framework's own entry point, found by `dlsym` rather than linked.
///
/// Deliberately not `#[link(name = "AudioToolbox", kind = "framework")]`.
/// Linking it pulls AudioToolbox — and the whole of CoreAudio behind it — into
/// *every* run of this driver, including a `/bin/ls`, and its initializers run
/// before the guest does. That is not just startup cost: they leave `errno`
/// set, and `errno` is passthrough here, so the guest's first read of it sees a
/// value from a library the guest never called. A fixture that checks
/// `errno == 0` at start-up caught it.
///
/// `RTLD_DEFAULT` is enough and needs no fallback: this shim runs only because
/// the guest imported `AudioUnitSetProperty`, and resolving that import is what
/// loaded the framework.
type SetPropertyFn = unsafe extern "C" fn(*mut c_void, u32, u32, u32, *const c_void, u32) -> i32;

fn host_set_property() -> Option<SetPropertyFn> {
    static FOUND: std::sync::OnceLock<Option<SetPropertyFn>> = std::sync::OnceLock::new();
    *FOUND.get_or_init(|| {
        let name = c"AudioUnitSetProperty";
        // SAFETY: a NUL-terminated name and a well-known pseudo-handle; the
        // result is either null or the framework's own function, and the type
        // is the one AudioToolbox.h declares.
        let sym = unsafe { libc::dlsym(libc::RTLD_DEFAULT, name.as_ptr()) };
        (!sym.is_null()).then(|| unsafe { core::mem::transmute::<*mut c_void, SetPropertyFn>(sym) })
    })
}

/// Whether `id` names a property whose data begins with a callback pointer.
fn carries_a_callback(id: u32) -> bool {
    matches!(
        id,
        SET_RENDER_CALLBACK | SET_INPUT_CALLBACK | OUTPUT_SET_INPUT_CALLBACK
    )
}

/// `OSStatus AudioUnitSetProperty(AudioUnit, AudioUnitPropertyID,
/// AudioUnitScope, AudioUnitElement, const void *inData, UInt32 inDataSize)`
pub fn set_property(
    symbol: &str,
    ctx: &mut RegisterContext,
    mem: &mut dyn GuestMemory,
    callbacks: &dyn Callbacks,
) -> Result<()> {
    let (unit, id, scope) = (ctx.rdi, ctx.rsi as u32, ctx.rdx as u32);
    let (element, data, size) = (ctx.rcx as u32, ctx.r8, ctx.r9 as u32);

    // The guest cannot have reached this call without the framework being
    // loaded, so a miss is a fact about the host rather than about the guest —
    // reported by name, like any other unsatisfied import.
    let Some(set) = host_set_property() else {
        return Err(crate::abi::AbiError::Unsatisfied {
            symbol: symbol.to_string(),
        });
    };

    // Every other property is bytes, and bytes are identity-mapped: the guest's
    // buffer *is* a host pointer to the same bytes. Only a structure with a
    // code pointer in it needs anything done.
    if !carries_a_callback(id) || data == 0 {
        // SAFETY: `data` is either null or a guest address, which under identity
        // mapping is a readable host address for the `size` bytes the guest
        // says it is.
        let rc = unsafe {
            set(
                unit as *mut c_void,
                id,
                scope,
                element,
                data as *const c_void,
                size,
            )
        };
        set_int_return(ctx, rc);
        return Ok(());
    }

    if size < CALLBACK_STRUCT_SIZE {
        set_int_return(ctx, INVALID_PROPERTY_VALUE);
        return Ok(());
    }

    let mut buf = [0u8; CALLBACK_STRUCT_SIZE as usize];
    mem.read(data, &mut buf)
        .map_err(|_| bad_pointer(symbol, data))?;
    let proc_va = u64::from_le_bytes(buf[0..8].try_into().unwrap());
    let ref_con = u64::from_le_bytes(buf[8..16].try_into().unwrap());

    // The cookie is passed through untouched: it is the guest's own pointer and
    // the *guest's* callback is what will dereference it.
    let substituted: [u64; 2] = [callbacks.substitute(symbol, proc_va)?, ref_con];

    // SAFETY: `unit` is a host `AudioUnit` the guest received from
    // `AudioComponentInstanceNew`, and `substituted` is a live local of exactly
    // the size being declared. The framework copies it before returning.
    let rc = unsafe {
        set(
            unit as *mut c_void,
            id,
            scope,
            element,
            substituted.as_ptr().cast(),
            CALLBACK_STRUCT_SIZE,
        )
    };
    set_int_return(ctx, rc);
    Ok(())
}

/// One of this module's implementations, in the shape [`crate::shims`] wants —
/// the wider one, carrying [`Callbacks`].
pub type Shim = crate::signal::Shim;

/// Whether `name` (underscore already stripped) is one of this module's.
///
/// `AudioUnitGetProperty` is deliberately absent. It can hand a callback struct
/// *back*, and undoing the substitution needs the same care `signal`'s return
/// value gets — but nothing observed reads one back, and a shim written for a
/// case nobody exercises is a guess. It stays an ordinary table entry until
/// something needs otherwise.
pub fn shim_for(name: &str) -> Option<Shim> {
    match name {
        "AudioUnitSetProperty" => Some(set_property),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shim finds the framework with `RTLD_DEFAULT`, which in a real run is
    /// satisfied by the guest's own import having loaded it. A test process has
    /// no guest, so it loads the framework itself — and doing that rather than
    /// stubbing the call keeps the whole path under test, the real
    /// `AudioUnitSetProperty` included.
    fn framework_loaded() -> bool {
        static LOADED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *LOADED.get_or_init(|| {
            let path = c"/System/Library/Frameworks/AudioToolbox.framework/AudioToolbox";
            // SAFETY: a NUL-terminated path; the handle is deliberately leaked,
            // as every `dlopen` in this runtime is.
            !unsafe { libc::dlopen(path.as_ptr(), libc::RTLD_LAZY | libc::RTLD_GLOBAL) }.is_null()
        })
    }

    struct Recording(std::cell::RefCell<Vec<u64>>);

    impl Callbacks for Recording {
        fn substitute(&self, _symbol: &str, va: u64) -> Result<u64> {
            self.0.borrow_mut().push(va);
            Ok(va ^ 0xffff_0000)
        }
    }

    /// Guest memory that is this process's memory, which identity mapping makes
    /// true of a real guest too.
    struct Identity;

    impl GuestMemory for Identity {
        fn read(&self, va: u64, out: &mut [u8]) -> core::result::Result<(), machish::Error> {
            if va == 0 {
                return Err(machish::Error::OutOfBounds {
                    addr: va,
                    len: out.len(),
                });
            }
            // SAFETY: the tests only pass the address of a live local array.
            unsafe { core::ptr::copy_nonoverlapping(va as *const u8, out.as_mut_ptr(), out.len()) };
            Ok(())
        }
        fn write(&mut self, va: u64, data: &[u8]) -> core::result::Result<(), machish::Error> {
            Err(machish::Error::OutOfBounds {
                addr: va,
                len: data.len(),
            })
        }
    }

    fn ctx_for(id: u32, callback: &[u64; 2], size: u32) -> RegisterContext {
        let mut ctx = RegisterContext::new();
        // A null unit: every call below is refused by the framework before it
        // touches anything, which is what makes the *substitution* the thing
        // under test rather than CoreAudio's behaviour.
        ctx.rdi = 0;
        ctx.rsi = u64::from(id);
        ctx.rdx = 0;
        ctx.rcx = 0;
        ctx.r8 = callback.as_ptr() as u64;
        ctx.r9 = u64::from(size);
        ctx
    }

    #[test]
    fn the_render_callback_crosses_out_and_the_cookie_does_not() {
        if !framework_loaded() {
            eprintln!("skipping: AudioToolbox is not present");
            return;
        }
        for id in [
            SET_RENDER_CALLBACK,
            SET_INPUT_CALLBACK,
            OUTPUT_SET_INPUT_CALLBACK,
        ] {
            let cb = [0x1_0000_1234u64, 0x2_0000_5678];
            let callbacks = Recording(std::cell::RefCell::new(Vec::new()));
            let mut ctx = ctx_for(id, &cb, CALLBACK_STRUCT_SIZE);

            set_property("_AudioUnitSetProperty", &mut ctx, &mut Identity, &callbacks)
                .expect("set");

            assert_eq!(
                *callbacks.0.borrow(),
                vec![0x1_0000_1234],
                "property {id}: the proc, and only the proc, must be substituted"
            );
            // The guest's own structure is untouched — the framework copies.
            assert_eq!(cb, [0x1_0000_1234, 0x2_0000_5678]);
        }
    }

    #[test]
    fn an_ordinary_property_is_passed_straight_through() {
        if !framework_loaded() {
            eprintln!("skipping: AudioToolbox is not present");
            return;
        }
        let data = [44100.0f64.to_bits(), 0];
        let callbacks = Recording(std::cell::RefCell::new(Vec::new()));
        // `kAudioUnitProperty_SampleRate` is 2, and carries no pointer.
        let mut ctx = ctx_for(2, &data, 8);

        set_property("_AudioUnitSetProperty", &mut ctx, &mut Identity, &callbacks).expect("set");

        assert!(
            callbacks.0.borrow().is_empty(),
            "a property with no callback in it must not be rewritten"
        );
    }

    #[test]
    fn a_callback_structure_too_small_to_hold_one_is_refused() {
        if !framework_loaded() {
            eprintln!("skipping: AudioToolbox is not present");
            return;
        }
        let cb = [0x1_0000_1234u64, 0];
        let callbacks = Recording(std::cell::RefCell::new(Vec::new()));
        let mut ctx = ctx_for(SET_RENDER_CALLBACK, &cb, 8);

        set_property("_AudioUnitSetProperty", &mut ctx, &mut Identity, &callbacks).expect("set");

        assert_eq!(ctx.rax as i32, INVALID_PROPERTY_VALUE);
        assert!(callbacks.0.borrow().is_empty());
    }
}
