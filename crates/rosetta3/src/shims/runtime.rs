//! Symbols the runtime supplies itself, because the host will not hand them out.
//!
//! `machish`'s native provider passes imports through to the host's own
//! libSystem by `dlsym`, and a handful of symbols it deliberately declines.
//! `___stack_chk_guard` is the one every real program needs: it is the stack
//! protector's canary, and the compiler emits a load of it in the prologue of
//! any function with an array on its stack.
//!
//! It is declined for a good reason — `dlsym` does not export it, and the
//! host's own canary is not something to alias — and the consequence is precise
//! and fatal: the guest reads a null slot, dereferences it, and faults in its
//! own prologue before executing a line of its own code. Milestone 1's report
//! called this out as "1 import unresolved (need shims)". This is that shim.
//!
//! ## What the value has to be
//!
//! Any 64-bit value, as long as it is the *same* one for the whole run: the
//! prologue stores it and the epilogue compares. Its security purpose — being
//! unpredictable to an attacker — is not something a translation layer can
//! provide anyway, since the guest and the runtime share an address space, and
//! pretending otherwise would be theatre. What it must do is exist, be stable,
//! and have a zero byte in it, which is what makes a string overflow unable to
//! copy it back intact.
//!
//! ## `__tlv_bootstrap`
//!
//! The other symbol the host declines, for a better reason than `dlsym`
//! visibility: the host's copy allocates out of the *host's* image state, which
//! knows nothing about a guest descriptor. It is supplied here and implemented
//! in [`crate::tls`].

use machish::resolve::SyntheticDylib;

/// The guest's stack canary, in this process's memory.
///
/// One `static`, so its address is stable for the process lifetime and every
/// image that imports the symbol gets the same slot value — which is what the
/// prologue/epilogue pair requires.
///
/// The low byte is zero for the reason C libraries make it zero: it terminates
/// a `strcpy` that would otherwise restore the canary while overwriting the
/// return address.
static STACK_CHK_GUARD: u64 = 0x00ff_dead_beef_c0de;

/// Symbols the runtime supplies itself, consulted before the host.
///
/// Flat, so it answers a two-level import naming any library: the guest asks
/// `libSystem` for its canary, and the answer is not libSystem's to give.
pub fn provider() -> SyntheticDylib {
    let mut lib = SyntheticDylib::new("<rosetta3 runtime>");
    lib.flat(true).define_data(
        "___stack_chk_guard",
        &STACK_CHK_GUARD as *const u64 as usize as u64,
    );
    // Both spellings, because which one an image imports depends on its
    // toolchain's vintage and `machish`'s TLV extension redirects whichever it
    // finds to a flat lookup — which is this one. See [`crate::tls`] for why the
    // implementation needs no VM.
    for name in machish::extensions::tlv::BOOTSTRAP_SYMBOLS {
        lib.define_fn(name, crate::tls::bootstrap as *const ());
    }
    lib
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_guard_has_a_zero_byte() {
        assert!(
            STACK_CHK_GUARD.to_le_bytes().contains(&0),
            "a canary without a zero byte can be copied back by a string overflow"
        );
    }
}
