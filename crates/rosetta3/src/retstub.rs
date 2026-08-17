//! One byte of guest code that means "the call this runtime made is finished".
//!
//! ## Why the return address is code now
//!
//! Every entry into guest code pushes a return address before jumping to the
//! callee, and the runtime has to recognise it coming back: a `ret` that pops
//! it is the end of [`crate::vm::Vm::call_with_stack`], not a transfer to more
//! guest code. That recognition used to be *microlift's*. The lifter took a
//! sentinel address ([`LifterConfig::exit_trampoline_address`]) and compared
//! every out-of-region `ret` target against it, reporting
//! [`ExitReason::Return`] on a match.
//!
//! It no longer does by default, and the reason is the cost of the test rather
//! than a change of opinion: materializing a 64-bit constant, comparing and
//! branching, on every `ret` that leaves a region — which, in a recompiler, is
//! most of them. Worse, the sentinel arm is the *unlinkable* one, so the shape
//! that cost the most was also the one block linking could not resolve. With
//! the config left at its default `None` every such `ret` becomes the
//! `UnknownJump`/`TransferKind::Return` exit that block linking already serves,
//! and this runtime supplies its own end-of-call mark.
//!
//! ## What the mark is
//!
//! A page of guest memory whose first byte is `int3`, whose address is what
//! [`address`] hands out and what every entry pushes. A `ret` popping it exits
//! the region as an ordinary return to an untranslated address; the run loop
//! lifts what is there — one instruction, this module's — and running it exits
//! with [`ExitReason::Breakpoint`] and `rip` pointing at the `int3` itself.
//! That exit, at *this* address, is the end of the call.
//!
//! So the recognition microlift did per `ret` is done once per call, by the
//! guest code the return address names. It is lifted and cached like any other
//! region, and after the first call it is reached through the block-link table
//! — which is to say the end of a call is now a linked entry into a
//! two-instruction translation rather than a compare in every function.
//!
//! ## Why the whole page is `int3`, and why only the first byte counts
//!
//! [`is_exit`] answers for the published address and nothing else. A breakpoint
//! from anywhere else is a guest that executed an `int3` of its own — a
//! debugger trap, an assertion — and this runtime has nowhere to deliver it:
//! there is no guest signal path for `SIGTRAP` yet, and continuing past it
//! would run code the guest expected a debugger to have stopped. It panics
//! instead, which is the honest report. The address test is the future-proofing
//! that keeps the two distinguishable once there *is* somewhere to deliver one.
//!
//! The rest of the page is filled with `int3` anyway, for the reason
//! [`crate::trampoline`]'s arena is: a walk that peeks past the one byte reads a
//! one-byte trap rather than the zeroes [`crate::region`] treats as garbage.
//!
//! [`LifterConfig::exit_trampoline_address`]: microlift::core::LifterConfig::exit_trampoline_address
//! [`ExitReason::Return`]: microlift::core::exit::ExitReason::Return
//! [`ExitReason::Breakpoint`]: microlift::core::exit::ExitReason::Breakpoint

use static_init::dynamic;

/// The one-byte x86-64 breakpoint, which is the whole of the stub.
const INT3: u8 = 0xCC;

/// The page, mapped once on the first call into guest code.
#[dynamic]
static STUB: u64 = map().expect("rosetta3: could not map the guest return stub");

/// The address every entry into guest code pushes as its return address.
///
/// Mapped on first use and never unmapped: it is one page for the life of the
/// process, and a stale return address on some guest's stack must never become
/// memory that means something else.
///
/// Panics if the page cannot be mapped, which is a process that cannot run a
/// single guest call — there is no return address to push and nothing further
/// to try.
pub fn address() -> u64 {
    *STUB
}

/// Whether `va` is the return stub — that is, whether a breakpoint there is the
/// end of a call this runtime made.
pub fn is_exit(va: u64) -> bool {
    *STUB == va
}

/// Whether `va` is guest code this module owns. The stub is one instruction, so
/// this is [`is_exit`] — spelled separately because it answers
/// [`crate::program::Program::is_guest_address`]'s question rather than the run
/// loop's, and the two would drift apart if the stub ever grew.
pub fn contains(va: u64) -> bool {
    is_exit(va)
}

/// The region to lift in order to execute the stub at `va`.
///
/// One byte, copied out rather than borrowed, so the walk's upper bound is the
/// instruction itself and nothing follows it. Every other address in the page
/// is refused: it was never handed out, so executing it is a bug to report
/// rather than a trap to decode.
pub fn lift_region(va: u64) -> Option<(crate::region::Region, Vec<u8>)> {
    if !is_exit(va) {
        return None;
    }
    let bytes = [INT3];
    let region = crate::region::region_for(&bytes, va, va, false)?;
    Some((region, bytes.to_vec()))
}

/// `va` as a name, for a diagnostic. See [`crate::program::Program::locate`].
pub fn locate(va: u64) -> Option<String> {
    is_exit(va).then(|| "<return stub>".to_string())
}

/// Map the page and fill it with breakpoints.
///
/// Readable and writable by this runtime and never executable, which is the
/// decision every guest mapping is made under: the guest is x86-64 and this CPU
/// cannot run a byte of it.
fn map() -> Option<u64> {
    let size = machish::memory::host_page_size() as usize;
    // SAFETY: an anonymous private mapping of one page at an address of the
    // kernel's choosing. Nothing is assumed about where it lands.
    let base = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            size,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANON,
            -1,
            0,
        )
    };
    if base == libc::MAP_FAILED {
        return None;
    }
    // SAFETY: the page just mapped, writable and owned by this module. Written
    // before the address is published, so nothing can read it half-filled.
    unsafe { std::ptr::write_bytes(base as *mut u8, INT3, size) };
    Some(base as usize as u64)
}

#[cfg(test)]
mod tests {
    /// The address is stable and the byte behind it is the breakpoint every
    /// part of this rests on: the run loop recognises the *exit*, and the exit
    /// only happens because these bytes lift to one.
    #[test]
    fn the_stub_is_one_breakpoint_at_a_stable_address() {
        let va = super::address();
        assert_eq!(va, super::address(), "the stub moved between calls");
        assert!(super::is_exit(va));
        assert!(!super::is_exit(va + 1), "only the first byte is the stub");

        let (region, bytes) = super::lift_region(va).expect("the stub is liftable");
        assert_eq!(bytes, vec![super::INT3]);
        assert_eq!(
            region.range,
            va..va + 1,
            "the region is the one instruction"
        );
        assert!(super::lift_region(va + 1).is_none());
    }
}
