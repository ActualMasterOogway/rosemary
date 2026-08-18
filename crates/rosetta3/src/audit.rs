//! A static diagnostic: would any call out of `__text` be followed into as guest code?

use std::collections::BTreeSet;
use std::ops::Range;

use microlift::arch::x86_64::X86_64;
use microlift::core::{LifterConfig, lift_to_function_with_call_targets};

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
    /// Functions it refused, almost always an unimplemented instruction.
    pub functions_skipped: usize,
}

impl CallTargetAudit {
    /// Whether every call leaving the section is one microlift knows about.
    pub fn is_clean(&self) -> bool {
        self.unresolved.is_empty()
    }
}

/// Classify every statically-known direct call target in `functions`.
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
        let Some(bytes) = code.get(start..end.min(code.len())) else {
            continue;
        };
        if bytes.is_empty() {
            continue;
        }

        let config = LifterConfig {
            analysis_mode: true,
            ..LifterConfig::default()
        };
        match lift_to_function_with_call_targets::<X86_64>(bytes, f.start, config) {
            Ok((_func, targets)) => {
                audit.functions_lifted += 1;
                // `u64::MAX` is the sentinel analysis mode records for an
                // indirect call, whose target is not statically known.
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
