//! Runtime-supplied fast-call bridges: servicing an import without a VM exit.
//!
//! An intercepted import used to cost a full round trip. Microlift wrote the
//! whole guest register file into the context, returned out of the translation,
//! and [`crate::vm::Vm::run_loop`] looked the symbol up — a `BTreeMap` probe for
//! the site, a string match through [`crate::shims::dispatch`], a second string
//! lookup in [`crate::abi::table`] — before [`crate::abi::marshal`] built the
//! argument list and made the call. Every one of those lookups has the same
//! answer at a given call site forever, and it was re-derived on every call.
//!
//! A **bridge** is the same work with the loop taken out of the middle. The
//! runtime hands microlift a native function
//!
//! ```text
//! extern "C" fn(*mut RegisterContext, extra: u64) -> u32
//! ```
//!
//! and the lifted code calls it in place of the exit. Returning `0` means the
//! call was serviced and execution continues inline; returning non-zero returns
//! to the run loop with `exit_reason` saying why. See `microlift::core::bridge`
//! for the contract.
//!
//! ## What this is *not*
//!
//! It is not a second implementation of the ABI seam. Every bridge here ends up
//! in [`crate::abi::call_import_with`], the same function an
//! `ImportedFunctionCall` exit reaches, so a bridged call and a slow one differ
//! in what surrounds the call and in nothing else. That is the same relationship
//! [`crate::abi`] describes between a fast import and a slow one, and it is why
//! [`crate::ImportPolicy::AlwaysSlow`] stays a usable baseline: it registers no
//! bridges at all, so the two can be run against each other.
//!
//! ## Why an import can be bridged when it cannot be inlined
//!
//! A fast import needs the callee's signature spelled in
//! [`ArgType`](crate::abi::ArgType)s and placeable by the guest's
//! [`CallAbi`](microlift::core::abi::CallAbi), because microlift reproduces the
//! call. That rules out variadics, struct-by-value, callback takers, and
//! everything [`crate::shims`] exists for — see
//! [`classify`](crate::abi::classify), where each of those becomes
//! [`Classification::Slow`](crate::abi::Classification::Slow). A bridge
//! reproduces nothing: it is handed the context and does whatever the runtime
//! would have done at the exit. So the entire slow class becomes an inline call,
//! `printf` and `objc_msgSend` included.
//!
//! ## Three deferrals, and one way to say each
//!
//! - **Handled** ([`HANDLED`]) — the common case. Microlift reloads the
//!   clobbered registers and carries on.
//! - **Redirected** ([`resume`]) — the shim moved `rip` rather than returning a
//!   value: `longjmp`, an `objc_msgSend` standing in for the transfer itself, an
//!   import that resolved into another guest image. The run loop must continue
//!   at the new `rip`, which is what an `UnknownJump` already means to it.
//! - **Deferred** ([`defer`]) — leave the exit microlift seeded and let the run
//!   loop service the call the old way. Nothing on the fast path uses this, and
//!   it is what makes every unusual case survivable: a bridge that declines is a
//!   bridge that costs an exit, not one that is wrong.
//!
//! An error takes the redirect path with the failure parked on the thread, where
//! [`crate::vm::Vm::run_loop`] already looks for one after every step.

use std::sync::Arc;

use microlift::arch::x86_64::context::RegisterContext;
use microlift::core::bridge::{BridgeImport, ImportKind, ImportRegistry};
use microlift::core::exit::{ExitReason, TransferKind};

/// The call was serviced; continue inline.
pub const HANDLED: u32 = 0;

/// The run loop should take over. The exit reason in the context says how.
const EXIT: u32 = 1;

/// Continue the guest wherever the context now says, without servicing anything
/// further.
///
/// `UnknownJump` with a transfer kind that is not `DirectCall`, because that is
/// the one exit [`crate::vm::Vm::step`] answers by simply carrying on at `rip` —
/// which is exactly what a redirect wants, and exactly what a parked error wants
/// too, since the loop checks for one immediately afterwards.
fn resume(ctx: &mut RegisterContext) -> u32 {
    ctx.exit_reason = ExitReason::UnknownJump as u32;
    ctx.exit_code = TransferKind::IndirectJump as u32;
    EXIT
}

/// Hand the call back to the run loop, exactly as if no bridge had been
/// registered.
///
/// Microlift seeds the context with a well-formed exit before calling — an
/// `ImportedFunctionCall` at an import site, an `IndirectCall` at an indirect
/// one — so declining is a matter of leaving it alone.
fn defer(_ctx: &mut RegisterContext) -> u32 {
    EXIT
}

/// Continue inline — unless something failed underneath the call.
///
/// A host function this runtime forwards to may call *back* into the guest, and
/// a failure there cannot be returned through the host frame in the middle. It
/// is parked on the thread instead, and [`crate::vm::Vm::run_loop`] raises it
/// after the next region exit. A bridged call produces no exit, so a bridge that
/// reported success would leave the failure parked and let the guest carry on as
/// though the callback had worked.
///
/// `qsort` with a guest comparator that `longjmp`s out of itself is the shape:
/// the comparator's refusal is parked, `qsort` returns normally, and without
/// this the program prints the answer for the sort that never happened.
fn settle(vm: &crate::vm::Vm, ctx: &mut RegisterContext) -> u32 {
    match vm.has_pending() {
        true => resume(ctx),
        false => HANDLED,
    }
}

/// Record the guest stack pointer the way a VM exit would have.
///
/// The run loop sets [`ThreadState::rsp`](crate::thread::ThreadState) after
/// every region exit, and a host→guest callback entered from inside a host
/// function starts its frame below that value. A bridged call produces no exit,
/// so without this the recorded pointer is whatever the *last* exit left —
/// higher up the stack, above live frames — and a nested guest callback would
/// build itself on top of them. `qsort` with a guest comparator is the smallest
/// program that would notice.
fn publish_rsp(ctx: &RegisterContext) {
    if let Some(state) = crate::thread::current() {
        state.rsp.set(ctx.rsp);
    }
}

/// Run `body`, turning a Rust **panic** into a parked [`VmError`].
///
/// The bridges are `extern "C"`, so a panic escaping one aborts the process
/// with `panic in a function that cannot unwind` — a message naming this
/// runtime's calling convention and nothing about the guest. That is the worst
/// diagnostic in the project: no symbol, no address, no guest state, and the
/// real panic's own message swallowed by the barrier.
///
/// A panic here is a bug in this runtime rather than something the guest did,
/// which is exactly why it should be *reported* rather than aborted on: the
/// guest is at a known address, the run loop can name it, and the report says
/// which shim or dispatch broke instead of which ABI attribute forbade
/// unwinding.
///
/// Starbound is what made this worth having: it reached
/// `Application: Initializing SDL Video` and aborted inside
/// [`indirect_call`] with those two words and no others.
fn caught(vm: &crate::vm::Vm, what: &str, body: impl FnOnce() -> u32) -> u32 {
    // `AssertUnwindSafe` because the only state crossing in is the guest's
    // register context and this runtime's own locks, and a panic leaves both in
    // the state the failing operation left them — which is what the parked
    // error then reports on.
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(body)) {
        Ok(code) => code,
        Err(payload) => {
            let detail = payload
                .downcast_ref::<&str>()
                .map(|s| (*s).to_string())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "a panic with no message".to_string());
            vm.set_pending(
                crate::abi::AbiError::Refused {
                    symbol: what.to_string(),
                    reason: Box::leak(format!("the runtime panicked: {detail}").into_boxed_str()),
                }
                .into(),
            );
            HANDLED
        }
    }
}

/// Service a call to a registered import.
///
/// `import_va` is the slot or stub address the guest called, which microlift has
/// also parked in `imported_function_va`.
///
/// Private, and reached only as an address through [`import_bridge`]: the sole
/// caller is lifted code, which passes the `RegisterContext` its region was
/// entered with. There is no Rust caller for which the raw pointer would be
/// anything but a hazard.
extern "C" fn import_call(ctx: *mut RegisterContext, import_va: u64) -> u32 {
    // SAFETY: microlift passes the context pointer the translation is running
    // on, which the run loop owns for the duration of `Translation::enter` and
    // no one else touches. The borrow ends before this returns.
    let ctx = unsafe { &mut *ctx };
    let Some(vm) = crate::thunk::current() else {
        // No VM published on this thread. Not reachable from the run loop, and
        // the honest answer is the old path rather than a guess.
        return defer(ctx);
    };
    publish_rsp(ctx);
    caught(vm, "<bridged import call>", || {
        match vm.service_bridged_import(import_va, ctx) {
            Ok(false) => settle(vm, ctx),
            Ok(true) => resume(ctx),
            Err(err) => {
                vm.set_pending(err);
                resume(ctx)
            }
        }
    })
}

/// Decide what an indirect call through a runtime-computed address means, and
/// make it if it is a host call.
///
/// The bridge form of [`crate::vm::Vm::dispatch_indirect`], and the reason it is
/// worth having: a `dlsym`ed function pointer, an `objc_msgSend` reached as
/// `callq *%r12`, and a `_Thread_local` read through `__tlv_bootstrap` are all
/// indirect calls to *host* code, and each cost a VM exit. Only a target that
/// turns out to be guest code still needs one, because guest code can only be
/// entered through the JIT.
///
/// Private for the reason [`import_call`] is.
extern "C" fn indirect_call(ctx: *mut RegisterContext, target: u64) -> u32 {
    // SAFETY: as in `import_call`.
    let ctx = unsafe { &mut *ctx };
    let Some(vm) = crate::thunk::current() else {
        return defer(ctx);
    };
    publish_rsp(ctx);
    caught(vm, "<bridged indirect call>", || {
        match vm.dispatch_indirect_bridged(target, ctx) {
            Ok(false) => settle(vm, ctx),
            Ok(true) => resume(ctx),
            Err(err) => {
                vm.set_pending(err);
                resume(ctx)
            }
        }
    })
}

/// The address of a bridge function, as microlift wants it.
fn entry(f: extern "C" fn(*mut RegisterContext, u64) -> u32) -> *const u8 {
    f as usize as *const u8
}

/// The bridge every registered import is reached through.
pub fn import_bridge() -> BridgeImport {
    // `Direct`, not `Slot`: this function is one address for the life of the
    // process. The live-updating that a `Slot` buys is for a *target* that can
    // change under compiled code, and what changes here is the import table this
    // bridge reads at call time — not the bridge itself.
    //
    // The effects mask stays full for the same reason the first version of any
    // of this is conservative: narrowing it is per-symbol knowledge, it belongs
    // beside the signature in [`crate::abi::table`], and a mask that is wrong is
    // a stale register rather than a diagnostic. Full is what the slow exit
    // already pays, so nothing regresses while it waits.
    BridgeImport::direct("rosetta3-import", entry(import_call))
}

/// The bridge every unregistered indirect call is dispatched through.
pub fn indirect_bridge() -> BridgeImport {
    BridgeImport::direct("rosetta3-indirect", entry(indirect_call))
}

/// The imports that did not exist when the [`LifterConfig`] was built.
///
/// A [`crate::Program`]'s own sites are registered up front, which is possible
/// because they are all known before a line of guest code runs. An image the
/// guest `dlopen`s has none of them, and until microlift could be asked at lift
/// time there was no way to add any: the call landed in the run loop as an
/// `UnknownJump`, and [`crate::dyld::import_at`] answered it there — a lookup on
/// the path taken before *every region entry*, which is what the whole lock-free
/// view in that module exists to make affordable.
///
/// Asked here instead, the same question is asked once per call site per lift.
/// The observation that makes this sufficient rather than merely cheaper is that
/// a region lifted *before* a `dlopen` cannot contain a call into that image —
/// the image did not exist — so nothing already compiled needs patching.
///
/// [`LifterConfig`]: microlift::core::LifterConfig
#[derive(Debug)]
pub struct DynamicImports;

impl ImportRegistry for DynamicImports {
    fn lookup(&self, va: u64) -> Option<ImportKind> {
        // The synthetic address a [`crate::trampoline`] jumps to, which is the
        // one kind of import that exists only because this runtime made it.
        // Asked first because it is the cheapest question — two compares against
        // one reservation — and because nothing else can answer it.
        if let Some(kind) = crate::trampoline::import_kind(va) {
            return Some(kind);
        }
        // Consulted only for addresses absent from `LifterConfig::imports`, so
        // this never has to answer for the root program's own sites.
        let site = crate::dyld::import_at(va)?;
        // A loaded image's imports were bridged wholesale, which is correct and
        // leaves the best case on the table: an ordinary `strlen` imported by a
        // `dlopen`ed dylib has exactly the signature that makes a fast import
        // legal, and paying a bridge call plus a table lookup for it is the same
        // cost this module was written to remove from the *root* program. The
        // question is the one [`crate::ImportPolicy::promotion_for`] asks, and
        // the answer has to be the same for a plugin as for the executable that
        // loaded it.
        Some(fast_import(site).unwrap_or_else(|| ImportKind::Bridge(import_bridge())))
    }
}

/// `site` as an inline call, if it is one an inline call can express.
///
/// The three exclusions are [`crate::ImportPolicy::promotion_for`]'s, and the
/// fourth — an import that resolved into another **guest** image — is
/// [`crate::Program::lifter_config`]'s: guest pages are never executable, so
/// baking a native call to one faults on instruction fetch at a call site that
/// may not be reached for hours.
fn fast_import(site: &crate::program::ImportSite) -> Option<ImportKind> {
    use microlift::core::FastImport;

    if site.resolved == 0 || crate::trace::watching(&site.symbol) {
        return None;
    }
    if crate::abi::classify(site.tag, &site.symbol) != crate::abi::Classification::Fast {
        return None;
    }
    // Guest-resolved is the exclusion that cannot be skipped, and it is not
    // hypothetical: a `dlopen`ed dylib binds against the executable that loaded
    // it, so an import of a loaded image routinely resolves into *guest* memory
    // — DELTARUNE's `libYoYoGamepad.dylib` takes ninety of `GMGamePad`'s methods
    // straight out of `Mac_Runner`. Most of those have no signature and are
    // excluded above, and the ones that do are exactly the dangerous case: a
    // program that defines its own `malloc` (Roblox does) would get a native
    // call baked into memory mapped `r--`.
    //
    // Asked of the running VM because only a `Program` knows where the root
    // image landed. There is always one while lifting — lifting happens inside
    // the run loop — and a `None` here declines to promote, which is the
    // behaviour every one of these imports had before.
    let vm = crate::thunk::current()?;
    if vm.program.is_guest_address(site.resolved) {
        return None;
    }
    let crate::abi::Lookup::Known(abi) = crate::abi::table::lookup(site.tag, &site.symbol) else {
        return None;
    };
    Some(ImportKind::Fast(FastImport {
        symbol: crate::abi::dynamic_symbol(abi, site.resolved),
        abi: crate::abi::guest_call_abi(),
    }))
}

/// The registry, as [`microlift::core::LifterConfig`] wants it.
pub fn registry() -> Arc<dyn ImportRegistry> {
    Arc::new(DynamicImports)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The three answers must stay distinguishable, and only one of them may be
    /// zero: microlift branches on nothing else.
    #[test]
    fn only_handled_is_zero() {
        let mut ctx = RegisterContext::new();
        assert_eq!(HANDLED, 0);
        assert_ne!(resume(&mut ctx), 0);
        assert_ne!(defer(&mut ctx), 0);
    }

    /// `resume` must name an exit the run loop answers by continuing at `rip`.
    /// `DirectCall` is the one transfer kind that would not — it makes
    /// [`crate::vm::Vm::step`] jump to `imported_function_va`, which at a bridge
    /// site is the import that was just serviced. That would call it again,
    /// forever.
    #[test]
    fn resume_does_not_ask_for_the_call_to_be_repeated() {
        let mut ctx = RegisterContext::new();
        resume(&mut ctx);
        assert_eq!(ctx.exit_reason, ExitReason::UnknownJump as u32);
        assert_ne!(ctx.exit_code, TransferKind::DirectCall as u32);
        assert!(TransferKind::try_from(ctx.exit_code).is_ok());
    }

    /// `defer` leaves microlift's seeded exit intact; that is its whole content.
    #[test]
    fn defer_preserves_the_seeded_exit() {
        let mut ctx = RegisterContext::new();
        ctx.exit_reason = ExitReason::ImportedFunctionCall as u32;
        ctx.imported_function_va = 0x1234;
        defer(&mut ctx);
        assert_eq!(ctx.exit_reason, ExitReason::ImportedFunctionCall as u32);
        assert_eq!(ctx.imported_function_va, 0x1234);
    }

    /// A bridge is reached through its address, and a null one would be a call
    /// to zero from lifted code with nothing to report.
    #[test]
    fn bridges_have_addresses() {
        assert!(!entry(import_call).is_null());
        assert!(!entry(indirect_call).is_null());
        assert_ne!(entry(import_call), entry(indirect_call));
    }

    /// Nothing is a dynamic import before anything is `dlopen`ed, and the
    /// registry is consulted per call site while lifting — so the empty answer
    /// is the one that has to be cheap and correct.
    #[test]
    fn the_registry_is_empty_until_something_is_loaded() {
        assert!(DynamicImports.lookup(0x1000).is_none());
    }
}
