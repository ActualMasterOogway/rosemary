//! libcurl's two variadic setters, whose *argument's meaning is the previous
//! argument*.
//!
//! ## Why a signature cannot say this
//!
//! `curl_easy_setopt(CURL *, CURLoption, ...)` is declared variadic and is
//! never called variadically: every call passes exactly one further argument,
//! and what that argument *is* — a `long`, a `char *`, a `void *`, a
//! `curl_off_t`, or a **function pointer** — is decided by the option number in
//! front of it. So there is no fixed signature to write down, and the two
//! things this runtime does at a seam both depend on a value only known at the
//! call: where the argument goes, and whether it has to become a thunk.
//!
//! The placement half is the ordinary variadic problem, in its smallest form.
//! SysV AMD64 hands the third argument to the guest in `rdx`; Apple's arm64 ABI
//! puts a variadic argument on the **stack**. Forwarding through a fixed
//! three-parameter prototype would pass it in `x2`, where the host's
//! `va_arg` never looks — and curl would read whatever was on its stack, which
//! for `CURLOPT_URL` is a pointer it then follows. The host declaration below
//! is therefore variadic in Rust too, which puts the compiler in charge of the
//! placement rather than this runtime guessing at it. One `u64` covers every
//! type in the list: on LP64 all five are eight bytes.
//!
//! ## The callback half, and the rule that decides it
//!
//! `CURLOPT_WRITEFUNCTION` and its dozen relatives take a guest function
//! pointer that curl calls back — from inside `curl_easy_perform`, on whatever
//! thread performed it — so each must cross as a thunk, exactly as
//! `qsort`'s comparator does. Which options those are is not guessed and not
//! enumerated: curl builds the *type* into the option number itself, in
//! ten-thousands, and `CURLOPTTYPE_FUNCTIONPOINT` is 20000. Every function
//! option is `20000..30000` and nothing else is, which is a fact about the
//! header rather than a list that goes stale as curl adds options.
//!
//! A guest that passes null for a function option is asking for curl's default
//! handler back, and null is left null — [`Callbacks::substitute`] answers that
//! way for every other callback taker in this runtime, and the reason is the
//! same one: a trampoline to address zero is not what "no callback" means.
//!
//! ## `curl_easy_getinfo`, which needs only the first half
//!
//! Its third argument is an **out** pointer — `long *`, `char **`, `double *` —
//! that curl writes through, and the guest is identity-mapped, so the host
//! writes into the guest's own storage with nothing done. It is here for the
//! placement alone, and never substitutes: no `CURLINFO_*` is a function
//! pointer, which is why the type tags are checked rather than assumed on the
//! way past.

use core::ffi::{c_int, c_void};

use microlift::arch::x86_64::context::RegisterContext;

use crate::abi::{Callbacks, GuestMemory, Result};

use super::set_int_return;

/// One of this module's implementations, in the shape [`crate::shims`] wants —
/// the wider one, carrying [`Callbacks`]: whether a guest function pointer
/// crosses here is decided by the option number, so the marshaller cannot be
/// the thing that places it.
pub type Shim = crate::signal::Shim;

// The host's own libcurl — `/usr/lib/libcurl.4.dylib`, which is the same
// library the guest links against by install name, so this is the passthrough
// the rest of the runtime performs through a slot, reached here by name.
//
// Declared **variadic**, which is the whole point: it is what makes the
// compiler place the third argument the way `va_arg` will read it.
#[link(name = "curl")]
unsafe extern "C" {
    fn curl_easy_setopt(handle: *mut c_void, option: c_int, ...) -> c_int;
    fn curl_easy_getinfo(handle: *mut c_void, info: c_int, ...) -> c_int;
}

/// The multiplier curl encodes an option's type with, and the tag that means a
/// function pointer.
///
/// From `curl.h`: `CURLOPTTYPE_LONG` 0, `OBJECTPOINT` 10000,
/// `FUNCTIONPOINT` 20000, `OFF_T` 30000, `BLOB` 40000. The option number is the
/// tag plus a small ordinal, so the tag is the option divided by ten thousand.
const CURLOPTTYPE_FUNCTIONPOINT: c_int = 20_000;
const CURLOPTTYPE_OFF_T: c_int = 30_000;

/// `CURLE_BAD_FUNCTION_ARGUMENT`, which is curl's own answer to a call it
/// cannot make sense of — so a guest that reaches a refusal here gets the error
/// its own library would have given it rather than a plausible success.
const CURLE_BAD_FUNCTION_ARGUMENT: i32 = 43;

/// Whether an option's value is a function pointer curl will call back.
fn is_function_option(option: c_int) -> bool {
    (CURLOPTTYPE_FUNCTIONPOINT..CURLOPTTYPE_OFF_T).contains(&option)
}

/// `CURLcode curl_easy_setopt(CURL *handle, CURLoption option, ...)`
pub fn easy_setopt(
    symbol: &str,
    ctx: &mut RegisterContext,
    _mem: &mut dyn GuestMemory,
    callbacks: &dyn Callbacks,
) -> Result<()> {
    let handle = ctx.rdi;
    let option = ctx.rsi as c_int;
    let mut value = ctx.rdx;

    if handle == 0 {
        set_int_return(ctx, CURLE_BAD_FUNCTION_ARGUMENT);
        return Ok(());
    }
    // The one thing a signature could never have expressed. Null stays null:
    // that is how a guest asks for curl's own default handler back.
    if is_function_option(option) && value != 0 {
        value = callbacks.substitute(symbol, value)?;
    }

    // SAFETY: `handle` came from `curl_easy_init` and is the host's own; the
    // declaration is variadic, so the compiler places `value` where curl's
    // `va_arg` reads it. Every type the option list admits is eight bytes on
    // LP64, so one word is the whole of the argument whichever it is.
    let rc = unsafe { curl_easy_setopt(handle as *mut c_void, option, value) };
    set_int_return(ctx, rc);
    Ok(())
}

/// `CURLcode curl_easy_getinfo(CURL *handle, CURLINFO info, ...)`
pub fn easy_getinfo(
    _symbol: &str,
    ctx: &mut RegisterContext,
    _mem: &mut dyn GuestMemory,
    _callbacks: &dyn Callbacks,
) -> Result<()> {
    let handle = ctx.rdi;
    let info = ctx.rsi as c_int;
    let out = ctx.rdx;

    if handle == 0 {
        set_int_return(ctx, CURLE_BAD_FUNCTION_ARGUMENT);
        return Ok(());
    }
    // Nothing in `CURLINFO_*` is a function pointer, so there is nothing to
    // substitute — and a guest that found one would be handed a raw guest
    // address rather than a thunk, silently. Checked rather than assumed.
    if is_function_option(info) {
        set_int_return(ctx, CURLE_BAD_FUNCTION_ARGUMENT);
        return Ok(());
    }

    // SAFETY: as above. `out` is a guest address curl writes the answer
    // through, and the guest is identity-mapped, so it is the storage the guest
    // asked to have filled in.
    let rc = unsafe { curl_easy_getinfo(handle as *mut c_void, info, out) };
    set_int_return(ctx, rc);
    Ok(())
}

/// Whether `name` (underscore already stripped) is one of this module's.
pub fn shim_for(name: &str) -> Option<Shim> {
    match name {
        "curl_easy_setopt" => Some(easy_setopt),
        "curl_easy_getinfo" => Some(easy_getinfo),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The tag arithmetic, against the numbers `curl.h` actually assigns.
    #[test]
    fn a_function_option_is_the_twenty_thousands() {
        // CURLOPT_WRITEFUNCTION = 20011, HEADERFUNCTION = 20079,
        // XFERINFOFUNCTION = 20219 — the ones a program installs.
        for option in [20_011, 20_079, 20_219, 20_000, 29_999] {
            assert!(is_function_option(option), "{option} is a function option");
        }
        // CURLOPT_URL = 10002 is an object pointer, VERBOSE = 41 a long, and
        // INFILESIZE_LARGE = 30115 an off_t. None of them is a callback, and
        // substituting one would hand curl a trampoline in place of a string.
        for option in [41, 10_002, 30_115, 40_043, 0] {
            assert!(
                !is_function_option(option),
                "{option} is not a function option"
            );
        }
    }
}
