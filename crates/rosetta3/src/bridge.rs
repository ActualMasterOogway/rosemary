//! Runtime-supplied fast-call bridges: servicing an import without a VM exit.

use std::sync::Arc;

use microlift::arch::x86_64::context::RegisterContext;
use microlift::core::bridge::{BridgeImport, ImportKind, ImportRegistry, RESUME_LINKED};
use microlift::core::exit::{ExitReason, TransferKind};

/// The call was serviced; continue inline.
pub const HANDLED: u32 = 0;

/// The run loop should take over. The exit reason in the context says how.
const EXIT: u32 = 1;

/// Continue the guest wherever the context now says, without servicing anything
/// further.
fn resume(pending: bool, ctx: &mut RegisterContext) -> u32 {
    ctx.exit_reason = ExitReason::UnknownJump as u32;
    ctx.exit_code = TransferKind::IndirectJump as u32;
    match pending {
        true => EXIT,
        false => RESUME_LINKED,
    }
}

/// Hand the call back to the run loop, exactly as if no bridge had been registered.
fn defer(_ctx: &mut RegisterContext) -> u32 {
    EXIT
}

/// Continue inline — unless something failed underneath the call.
///
/// A host function this runtime forwards to may call *back* into the guest, and a
/// failure there cannot be returned through the host frame in the middle.
fn settle(vm: &crate::vm::Vm, ctx: &mut RegisterContext) -> u32 {
    match vm.has_pending() {
        true => resume(true, ctx),
        false => HANDLED,
    }
}

/// Record the guest stack pointer the way a VM exit would have.
fn publish_rsp(ctx: &RegisterContext) {
    if let Some(state) = crate::thread::current() {
        state.rsp.set(ctx.rsp);
    }
}

/// Run `body`, turning a Rust **panic** into a parked [`VmError`].
///
/// The bridges are `extern "C"`, so a panic escaping one aborts the process with `panic
/// in a function that cannot unwind` — a message naming this runtime's calling
/// convention and nothing about the guest.
///
/// A panic here is a bug in this runtime rather than something the guest did, which is
/// exactly why it should be *reported* rather than aborted on.
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
            Ok(true) => resume(vm.has_pending(), ctx),
            Err(err) => {
                vm.set_pending(err);
                resume(true, ctx)
            }
        }
    })
}

/// Decide what an indirect call through a runtime-computed address means, and make it
/// if it is a host call.
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
            Ok(true) => resume(vm.has_pending(), ctx),
            Err(err) => {
                vm.set_pending(err);
                resume(true, ctx)
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
    // process.
    BridgeImport::direct("rosetta3-import", entry(import_call))
}

/// The bridge every unregistered indirect call is dispatched through.
pub fn indirect_bridge() -> BridgeImport {
    BridgeImport::direct("rosetta3-indirect", entry(indirect_call))
}

/// The imports that did not exist when the `LifterConfig` was built.
#[derive(Debug)]
pub struct DynamicImports;

impl ImportRegistry for DynamicImports {
    fn lookup(&self, va: u64) -> Option<ImportKind> {
        // The synthetic address a [`crate::trampoline`] jumps to, which is the
        // one kind of import that exists only because this runtime made it.
        if let Some(kind) = crate::trampoline::import_kind(va) {
            return Some(kind);
        }
        // Consulted only for addresses absent from `LifterConfig::imports`, so
        // this never has to answer for the root program's own sites.
        let site = crate::dyld::import_at(va)?;
        // A loaded image's imports were bridged wholesale, which is correct and
        // leaves the best case on the table.
        Some(fast_import(site).unwrap_or_else(|| ImportKind::Bridge(import_bridge())))
    }
}

/// `site` as an inline call, if it is one an inline call can express.
fn fast_import(site: &crate::program::ImportSite) -> Option<ImportKind> {
    use microlift::core::FastImport;

    if site.resolved == 0 || crate::trace::watching(&site.symbol) {
        return None;
    }
    if crate::abi::classify(site.tag, &site.symbol) != crate::abi::Classification::Fast {
        return None;
    }
    // Guest-resolved is the exclusion that cannot be skipped, and it is not
    // hypothetical.
    let vm = crate::thunk::current()?;
    if vm.program.is_guest_address(site.resolved) {
        return None;
    }
    // An Objective-C runtime entry point is inlined against a *substituting*
    // address rather than the host's own; everything else against the host's.
    let promotion = match crate::objc::fast::promotion(&site.symbol, site.resolved) {
        Some(promotion) => promotion,
        None => {
            let crate::abi::Lookup::Known(abi) = crate::abi::table::lookup(site.tag, &site.symbol)
            else {
                return None;
            };
            crate::objc::fast::Promotion {
                abi,
                address: site.resolved,
            }
        }
    };
    Some(ImportKind::Fast(FastImport {
        symbol: crate::abi::dynamic_symbol(promotion.abi, promotion.address),
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
        assert_ne!(resume(false, &mut ctx), 0);
        assert_ne!(resume(true, &mut ctx), 0);
        assert_ne!(defer(&mut ctx), 0);
    }

    /// `resume` must name an exit the run loop answers by continuing at `rip`.
    #[test]
    fn resume_does_not_ask_for_the_call_to_be_repeated() {
        let mut ctx = RegisterContext::new();
        resume(false, &mut ctx);
        assert_eq!(ctx.exit_reason, ExitReason::UnknownJump as u32);
        assert_ne!(ctx.exit_code, TransferKind::DirectCall as u32);
        assert!(TransferKind::try_from(ctx.exit_code).is_ok());
    }

    /// The one difference a parked error makes: the same exit, but not a linkable one.
    #[test]
    fn a_parked_error_is_not_linkable() {
        let mut clean = RegisterContext::new();
        let mut parked = RegisterContext::new();
        assert_eq!(resume(false, &mut clean), RESUME_LINKED);
        assert_eq!(resume(true, &mut parked), EXIT);
        // And the exit itself is identical either way: linkability is a
        // statement about what the host still owes, not about the transfer.
        assert_eq!(clean.exit_reason, parked.exit_reason);
        assert_eq!(clean.exit_code, parked.exit_code);
        assert_eq!(clean.rip, parked.rip);
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
