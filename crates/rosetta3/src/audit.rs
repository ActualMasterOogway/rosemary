//! A static diagnostic: would any call out of `__text` be followed into as
//! guest code?
//!
//! ## This is not the execution model
//!
//! Rosetta 3 is a JIT — it lifts a region when control is about to reach it and
//! never translates ahead of time. This module does the opposite: it walks
//! *every* function in a section without running any of them. That is
//! deliberate, and it is allowed because this is a **diagnostic**, not a
//! pipeline stage. A test that reasoned only about the regions a particular run
//! happened to enter would pass or fail depending on the program's input; this
//! one decides the question over the whole section, once, offline.
//!
//! Nothing here runs during execution, and nothing here caches anything for the
//! JIT to reuse.
//!
//! ## What is being checked
//!
//! In JIT mode a `call rel32` whose target lies outside the lifted region and
//! is not a registered import exits with `ExitReason::UnknownJump` and
//! `TransferKind::DirectCall`
//! (`microlift/src/arch/x86_64/semantics/control_flow.rs:139-192`). In general
//! that is *fine and expected* — it is how control leaves one translation unit
//! for the next, and the runtime answers it by lifting the target.
//!
//! It stops being fine when the target is a `__stubs` entry. Lifting one "as
//! the next region" means decoding six bytes of trampoline as guest code,
//! following its `jmp qword [rip+disp]` into the `__la_symbol_ptr` slot, and
//! finding a **native arm64 host address** there — which the recompiler would
//! then try to decode as x86-64. Every import call in a classic image ends that
//! way if stubs are not registered.
//!
//! So the audit asks: of the call targets that leave `__text`, which are
//! addresses microlift was told about? Anything else, the JIT will follow. The
//! check is falsifiable, which is the point — run it with only `slot_va`
//! registered and it reports every import call in the image.
//!
//! ## Function at a time, not section at a time
//!
//! Each function is analysed separately, with `LC_FUNCTION_STARTS` for the
//! boundaries, and targets are classified against the whole section. Handing
//! microlift all of `__text` at once does not work: it decodes straight through
//! the alignment padding between functions, where `0xfb` is `sti` and `0x00
//! 0x00` is `add [rax], al`, and dies on a bogus "unsupported instruction"
//! nowhere near any real code. This is also, incidentally, the clearest
//! practical argument against static recompilation: knowing which bytes are
//! code is a per-program problem, and a JIT never has to ask.
//!
//! A function the recompiler cannot lift is **counted, not fatal**. An
//! unimplemented instruction is a gap in the lifter, and reporting it as an
//! interception failure would blame the loader for someone else's missing
//! opcode. It does mean the audit's coverage is "every function that lifted",
//! which [`CallTargetAudit::functions_skipped`] states plainly.

use std::collections::BTreeSet;
use std::ops::Range;

use microlift::arch::x86_64::X86_64;
use microlift::core::{lift_to_function_with_call_targets, LifterConfig};

/// A call target the JIT would follow into rather than recognise.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct UnresolvedCall {
    /// The callee address: outside the section, and not a registered import.
    pub target: u64,
}

/// What the audit found.
#[derive(Clone, Debug, Default)]
pub struct CallTargetAudit {
    /// Distinct statically-known direct call targets in the region.
    pub direct_calls: usize,
    /// Of those, how many left the region.
    pub out_of_region: usize,
    /// Of those, how many were registered imports — the intercepted ones.
    pub intercepted: usize,
    /// Out-of-section targets microlift was not told about, which the JIT would
    /// therefore follow into as guest code. Empty is the acceptance condition.
    pub unresolved: Vec<UnresolvedCall>,
    /// Functions the lifter accepted.
    pub functions_lifted: usize,
    /// Functions it refused, almost always an unimplemented instruction. These
    /// contribute nothing to the counts above, so the audit's coverage is the
    /// lifted set — see the module note.
    pub functions_skipped: usize,
}

impl CallTargetAudit {
    /// Whether every call leaving the section is one microlift knows about.
    pub fn is_clean(&self) -> bool {
        self.unresolved.is_empty()
    }
}

/// Classify every statically-known direct call target in `functions`.
///
/// `code` is the whole section and `code_va` its address; `functions` are the
/// sub-ranges to lift, and `region` is what counts as "inside" — normally the
/// section's own extent, so a call from one function to another is internal.
/// `imports` is the set of addresses registered with the lifter: for a Mach-O
/// guest, every `slot_va` *and* every `stub_va`.
pub fn audit_call_targets(
    code: &[u8],
    code_va: u64,
    functions: &[Range<u64>],
    region: Range<u64>,
    imports: &BTreeSet<u64>,
) -> CallTargetAudit {
    let mut audit = CallTargetAudit::default();
    let mut distinct: BTreeSet<u64> = BTreeSet::new();

    for f in functions {
        let (Some(start), Some(end)) = (
            usize::try_from(f.start.wrapping_sub(code_va)).ok(),
            usize::try_from(f.end.wrapping_sub(code_va)).ok(),
        ) else {
            continue;
        };
        let Some(bytes) = code.get(start..end.min(code.len())) else { continue };
        if bytes.is_empty() {
            continue;
        }

        let config = LifterConfig { analysis_mode: true, ..LifterConfig::default() };
        match lift_to_function_with_call_targets::<X86_64>(bytes, f.start, config) {
            Ok((_func, targets)) => {
                audit.functions_lifted += 1;
                // `u64::MAX` is the sentinel analysis mode records for an
                // indirect call, whose target is not statically known. Indirect
                // calls are a different problem — `ExitReason::IndirectCall`,
                // resolved at runtime — and not one registering imports could
                // address.
                distinct.extend(targets.into_iter().filter(|&t| t != u64::MAX));
            }
            Err(_) => audit.functions_skipped += 1,
        }
    }

    // Deduplicated: a function called from forty sites is one call target for
    // this purpose, and counting it forty times would make the numbers say
    // something about the guest's control flow rather than about coverage.
    audit.direct_calls = distinct.len();
    for target in distinct {
        if region.contains(&target) {
            continue;
        }
        audit.out_of_region += 1;
        if imports.contains(&target) {
            audit.intercepted += 1;
        } else {
            audit.unresolved.push(UnresolvedCall { target });
        }
    }
    audit
}
