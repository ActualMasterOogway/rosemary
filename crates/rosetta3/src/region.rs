//! Which bytes to lift, given an address control is about to reach.
//!
//! ## Why a function's recorded extent is not the answer
//!
//! `LC_FUNCTION_STARTS` says where a region *begins*. It does not promise that
//! the bytes run to the next start without interruption, and `/bin/ls` is the
//! counterexample: its `main` embeds a jump table — four-byte entries — in the
//! middle of `__TEXT,__text`. Handing microlift the whole recorded extent means
//! handing it that table, which it decodes linearly like everything else (`0xfb`
//! is `sti`) and then refuses the function on. Two of `/bin/ls`'s fifty
//! functions are lost that way, `main` among them.
//!
//! So the extent is computed rather than trusted. Starting from the address
//! control is about to reach, this walks the guest's own control flow with the
//! same decoder microlift uses, and returns the **maximal contiguous run of
//! decodable instructions** beginning at the region start. A jump table is not
//! reachable as an instruction, so the run stops in front of it, and the
//! switch's case bodies — which live *after* the table — are simply not in this
//! region. That is not a loss: control reaches one of them at run time through
//! the indirect jump, microlift exits with `UnknownJump`, and the runtime lifts
//! a fresh region at that address. Demand-driven translation is exactly what
//! makes the answer "lift what you can reach now" sufficient.
//!
//! ## What the walk does and does not follow
//!
//! - **Conditional branches**: both edges, since both are code.
//! - **Unconditional near jumps**: the target.
//! - **Calls**: only the resume point. A callee is a region of its own;
//!   following it would inline the whole program into one translation unit.
//! - **`ret`, indirect jumps, `ud2`, `int3`**: stop. There is nothing further
//!   to reach *statically*, which is precisely the case the JIT loop answers.
//!
//! Targets outside the buffer are dropped: they are somewhere else's region.
//!
//! ## Contiguity, not coverage
//!
//! The walk discovers a *set* of instructions; microlift takes a *slice*. Those
//! differ whenever the walk jumps over a hole, so the region is trimmed to the
//! first byte after the start that no decoded instruction covers. Anything
//! beyond the hole is reachable later, from its own PC, and lifting it as part
//! of this region would mean decoding the hole.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;

use iced_x86::{Decoder, DecoderOptions, FlowControl, Instruction};

/// Ceiling on how far one region may run, in bytes.
///
/// Totality, not policy: the walk is over guest-controlled bytes, so it needs a
/// bound that does not depend on them being sensible. Real functions are orders
/// of magnitude smaller.
pub const MAX_REGION: u64 = 256 * 1024;

/// Ceiling on how many instructions one walk may decode.
const MAX_INSTRUCTIONS: usize = 200_000;

/// A region to lift: what to hand microlift, and where its instructions begin.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Region {
    /// Guest addresses the region covers.
    pub range: Range<u64>,
    /// Every instruction start inside it, ascending.
    ///
    /// Kept because "does this translation cover that address" is not a range
    /// test. Microlift's entry dispatch can resume at an instruction boundary
    /// and *silently falls back to the region's first block* for anything else,
    /// so entering a translation at an address that is inside its range but not
    /// on its instruction grid runs the wrong code with no error at all. That
    /// is not hypothetical: `/bin/ls` reaches a function through a pointer while
    /// a cached region spans the bytes, and the wrong answer was a program that
    /// re-ran an earlier line instead of printing.
    ///
    /// The run is contiguous and its instructions abut, so this set is exactly
    /// the boundaries microlift's own linear decode of the same bytes produces.
    pub starts: Vec<u64>,
}

impl Region {
    /// Whether the region can be entered at `pc`.
    pub fn covers(&self, pc: u64) -> bool {
        self.starts.binary_search(&pc).is_ok()
    }
}

/// The region to lift so that `pc` can execute.
///
/// `bytes` are the containing image's mapped contents and `base` the address of
/// their first byte. `function` is the enclosing function's recorded extent from
/// `LC_FUNCTION_STARTS`, when there is one, and it is used for both of the
/// things such a record can honestly say:
///
/// - **Where to start.** Entering at a function's first instruction is the
///   common case, and starting there means the many call sites that reach one
///   function share one translation. Used only if the run from there actually
///   reaches `pc`, so a function interrupted by an embedded jump table degrades
///   to a region starting at `pc`.
/// - **Where to stop.** A function's recorded extent may not be code all the
///   way through, but it is certainly not *more* than that function — and a run
///   that walks past the end of one function into the next is the failure this
///   bound prevents. Without it, a fallthrough or a forward branch near the end
///   can decode straight into the neighbour on the wrong instruction grid, and
///   the resulting translation claims addresses it cannot correctly execute.
///
/// `None` when `pc` is outside `bytes`, or when nothing at `pc` decodes.
pub fn region_for(
    bytes: &[u8],
    base: u64,
    pc: u64,
    function: Option<Range<u64>>,
) -> Option<Region> {
    let bound = function.as_ref().map_or(u64::MAX, |f| f.end);
    if let Some(f) = function.filter(|f| f.contains(&pc))
        && let Some(r) = walk(bytes, base, f.start, bound).filter(|r| r.covers(pc))
    {
        return Some(r);
    }
    walk(bytes, base, pc, bound)
}

/// The contiguous decodable run beginning at `start`, stopping before `bound`.
fn walk(bytes: &[u8], base: u64, start: u64, bound: u64) -> Option<Region> {
    let start_off = usize::try_from(start.checked_sub(base)?).ok()?;
    if start_off >= bytes.len() {
        return None;
    }
    let limit = start.saturating_add(MAX_REGION).min(bound);

    // Instruction extents, keyed by address, so the contiguity scan below can
    // ask "what covers this byte" in order.
    let mut covered: BTreeMap<u64, u64> = BTreeMap::new();
    let mut seen: BTreeSet<u64> = BTreeSet::new();
    let mut queue = vec![start];
    let mut decoder = Decoder::with_ip(64, bytes, base, DecoderOptions::NONE);
    let mut instr = Instruction::default();
    let mut budget = MAX_INSTRUCTIONS;

    while let Some(mut ip) = queue.pop() {
        loop {
            if budget == 0 || ip < start || ip >= limit || !seen.insert(ip) {
                break;
            }
            budget -= 1;
            let Ok(off) = usize::try_from(ip.wrapping_sub(base)) else { break };
            if off >= bytes.len() {
                break;
            }
            decoder.set_position(off).ok()?;
            decoder.set_ip(ip);
            decoder.decode_out(&mut instr);
            if instr.is_invalid() {
                break;
            }
            let next = instr.next_ip();
            // An instruction straddling the bound belongs to whatever is on the
            // other side of it, not to this region.
            if next > limit {
                break;
            }
            covered.insert(ip, next);

            match instr.flow_control() {
                // Straight-line, or a call whose callee is someone else's
                // region: carry on at the resume point.
                FlowControl::Next
                | FlowControl::Call
                | FlowControl::IndirectCall
                | FlowControl::Interrupt
                | FlowControl::XbeginXabortXend => ip = next,

                FlowControl::ConditionalBranch => {
                    queue.push(instr.near_branch_target());
                    ip = next;
                }
                FlowControl::UnconditionalBranch => {
                    ip = instr.near_branch_target();
                }

                // Nothing further is reachable without running the program,
                // which is the JIT's job rather than this walk's.
                FlowControl::Return
                | FlowControl::IndirectBranch
                | FlowControl::Exception => break,
            }
        }
    }

    // Trim to the first byte the walk did not cover. Everything past a hole is
    // reachable from its own PC later; including it would mean lifting the hole.
    let mut end = start;
    while let Some((&at, &next)) = covered.range(..=end).next_back() {
        if at <= end && next > end {
            end = next;
        } else {
            break;
        }
    }
    if end <= start {
        return None;
    }
    let end = end.min(base + bytes.len() as u64);
    let starts: Vec<u64> = covered.keys().copied().take_while(|&a| a < end).collect();
    Some(Region { range: start..end, starts })
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: u64 = 0x1_0000;

    fn range(bytes: &[u8], pc: u64, function: Option<Range<u64>>) -> Option<Range<u64>> {
        region_for(bytes, BASE, pc, function).map(|r| r.range)
    }

    #[test]
    fn a_straight_line_function_ends_at_its_ret() {
        // xor eax, eax ; ret ; (padding: int3)
        let code = [0x31, 0xC0, 0xC3, 0xCC, 0xCC];
        assert_eq!(range(&code, BASE, None), Some(BASE..BASE + 3));
    }

    #[test]
    fn a_forward_branch_over_a_hole_stops_at_the_hole() {
        // jmp +2 ; two bytes never walked ; ret
        let code = [0xEB, 0x02, 0x31, 0xC0, 0xC3];
        assert_eq!(
            range(&code, BASE, None),
            Some(BASE..BASE + 2),
            "the un-walked bytes are a hole, and a region has to be contiguous"
        );
    }

    #[test]
    fn a_conditional_branch_follows_both_edges() {
        // je +1 ; nop ; ret
        let code = [0x74, 0x01, 0x90, 0xC3];
        assert_eq!(range(&code, BASE, None), Some(BASE..BASE + 4));
    }

    #[test]
    fn an_embedded_jump_table_terminates_the_region() {
        // jmp rax ; then four bytes of table.
        let code = [0xFF, 0xE0, 0x00, 0x00, 0x00, 0x00];
        assert_eq!(
            range(&code, BASE, None),
            Some(BASE..BASE + 2),
            "an indirect jump is where static reachability stops"
        );
    }

    #[test]
    fn a_call_does_not_pull_in_its_callee() {
        // call +0 (i.e. to the next instruction) ; ret
        let code = [0xE8, 0x00, 0x00, 0x00, 0x00, 0xC3];
        assert_eq!(range(&code, BASE, None), Some(BASE..BASE + 6));
    }

    #[test]
    fn a_function_start_that_does_not_reach_the_pc_is_abandoned() {
        // ret ; <hole: 0f ff is invalid> ; xor eax,eax ; ret
        let code = [0xC3, 0x0F, 0xFF, 0x31, 0xC0, 0xC3];
        let pc = BASE + 3;
        assert_eq!(
            range(&code, pc, Some(BASE..BASE + 6)),
            Some(pc..BASE + 6),
            "the region restarts at the PC rather than lifting the hole"
        );
    }

    #[test]
    fn a_function_start_that_does_reach_the_pc_wins() {
        // Entering at the `ret` still lifts from the function's first
        // instruction, so every entry shares one translation.
        let code = [0x31, 0xC0, 0xC3];
        assert_eq!(range(&code, BASE + 2, Some(BASE..BASE + 3)), Some(BASE..BASE + 3));
    }

    #[test]
    fn a_region_never_runs_past_the_end_of_its_function() {
        // `nop` five times with no terminator, then the next function. Nothing
        // stops the walk except the recorded extent — and if it did not stop,
        // the translation would claim addresses in the neighbour and execute
        // them on this function's instruction grid.
        let code = [0x90, 0x90, 0x90, 0x90, 0x90, 0x55, 0x48, 0x89, 0xE5, 0xC3];
        let first = BASE..BASE + 5;
        assert_eq!(range(&code, BASE, Some(first.clone())), Some(first));
    }

    #[test]
    fn an_instruction_straddling_the_bound_is_left_out() {
        // `mov eax, imm32` is five bytes and the function is recorded as four,
        // so it belongs to whatever the record says comes next.
        let code = [0x90, 0xB8, 0x01, 0x00, 0x00, 0x00];
        assert_eq!(range(&code, BASE, Some(BASE..BASE + 4)), Some(BASE..BASE + 1));
    }

    #[test]
    fn an_address_outside_the_image_has_no_region() {
        let code = [0xC3];
        assert_eq!(range(&code, BASE + 64, None), None);
        assert_eq!(range(&code, BASE - 1, None), None);
    }

    #[test]
    fn undecodable_bytes_yield_nothing_rather_than_an_empty_region() {
        let code = [0x0F, 0xFF];
        assert_eq!(range(&code, BASE, None), None);
    }

    #[test]
    fn a_backward_jump_loop_terminates() {
        // A tight infinite loop: `jmp -2`. The walk must not be one.
        let code = [0xEB, 0xFE];
        assert_eq!(range(&code, BASE, None), Some(BASE..BASE + 2));
    }

    #[test]
    fn a_long_run_of_code_stops_at_the_region_ceiling() {
        // Two-byte `xchg ax,ax` padding, so the byte ceiling is what stops the
        // walk rather than the instruction budget.
        let code = [0x66u8, 0x90].repeat((MAX_REGION as usize / 2) + 8);
        let r = range(&code, BASE, None).unwrap();
        assert_eq!(r.end - r.start, MAX_REGION);
    }

    #[test]
    fn an_unterminated_run_stops_at_the_instruction_budget() {
        let code = vec![0x90u8; MAX_INSTRUCTIONS + 16];
        let r = range(&code, BASE, None).unwrap();
        assert_eq!(r.end - r.start, MAX_INSTRUCTIONS as u64);
    }

    #[test]
    fn coverage_is_the_instruction_grid_and_not_the_range() {
        // nop ; mov eax, imm32 ; ret — the four immediate bytes are inside the
        // region and are not addresses it can be entered at. Answering "yes" to
        // one of them is what silently executes the wrong code.
        let code = [0x90, 0xB8, 0x01, 0x00, 0x00, 0x00, 0xC3];
        let r = region_for(&code, BASE, BASE, None).unwrap();
        assert_eq!(r.range, BASE..BASE + 7);
        assert_eq!(r.starts, vec![BASE, BASE + 1, BASE + 6]);
        assert!(r.covers(BASE + 6));
        assert!(!r.covers(BASE + 3), "an immediate byte is not an instruction");
    }
}
