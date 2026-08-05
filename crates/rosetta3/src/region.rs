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
//! - **A run of zero bytes**: stop. See below.
//!
//! ## Zeros are memory nobody has written, not an instruction
//!
//! `00 00` decodes cleanly as `add byte ptr [rax], al`, and a decoder walking
//! forward through zero-filled memory therefore never stops: it produces a
//! plausible instruction every two bytes until it runs out of buffer. In an
//! image that is a nuisance — alignment padding is `int3` or `nop`, so it rarely
//! comes up. In memory the guest **generates code into** it is fatal, and
//! Terraria is where that showed.
//!
//! An anonymous mapping is zero-filled by the kernel, so a zero byte in a JIT
//! arena is a byte the guest has not written *yet*. Mono emits one method, this
//! runtime lifts a region at its entry, and the walk runs off the end of that
//! method through a quarter-megabyte of untouched arena — 127,779 instructions,
//! ending only at the mapping's edge. The translation is wasted work, which
//! would be harmless. What is not harmless is that the region now *claims* every
//! address in it: when Mono compiles its next method into that same arena and
//! jumps to it, the code cache finds the earlier translation covering the
//! address, and runs the zeros it decoded there instead of the method that has
//! since been written.
//!
//! So an all-zero instruction ends the walk, exactly as an undecodable one does.
//! No compiler emits `add [rax], al`, and treating it as the end of code costs
//! nothing where it is wrong: the bytes past it are reached later from their own
//! PC, which is what demand-driven lifting is for.
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

/// Ceiling on how many instructions one region may *contain*.
///
/// Distinct from [`MAX_INSTRUCTIONS`], which is a totality bound on the walk.
/// This one is about what happens afterwards: Cranelift's register allocator is
/// **superlinear in live ranges**, so compile time is not proportional to region
/// size — a big region is disproportionately expensive rather than
/// proportionately so, and the outliers are extreme:
///
/// ```text
/// 6314 instructions  ->   0.8s
/// 8827 instructions  ->   1.8s
/// 4841 instructions  ->  21.7s
/// ```
///
/// The worst are in a **Mono code arena**: generated code has no
/// `LC_FUNCTION_STARTS` to bound the walk and Mono packs methods back to back
/// with nothing between them for it to stop on, so one region reached 141 KiB
/// and 25,057 instructions — several hundred generated methods glued together.
/// The same "a region claims more than it should" shape as
/// [the zero-filled arena](self), costing time rather than correctness.
///
/// **What a cap costs is real and small.** The bytes past it are reached later
/// from their own PC — which is how an embedded jump table has always been
/// handled — so the price is one extra region entry at the boundary and the
/// cross-block optimisation Cranelift would have done across it. On a program
/// with no region this long it costs exactly nothing, because it never binds.
///
/// **The value is measured, not round.** Four minutes of Terraria's start-up,
/// same binary, cap varied — time spent compiling, the single worst compile,
/// and how far the guest got (regions lifted out of its own JIT arenas):
///
/// ```text
/// cap        compiling   worst    progress
/// none          96.2s    21.7s        6044
/// 2048          59.2s     4.2s        6690
/// 1024          33.7s     1.1s        6769
///  512          15.2s     0.3s        6839
/// ```
///
/// Progress flattens well before compile time does, which says execution is the
/// bottleneck once the outliers are gone and that cutting further buys nothing
/// *here*. So the choice is the largest value that removes them: real functions
/// a compiler emits are hundreds of instructions, so 1024 is above essentially
/// everything an ordinary program contains — leaving a hot game loop's regions
/// whole, which this workload cannot measure — and below everything that hurt.
const MAX_REGION_INSTRUCTIONS: usize = 1024;

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

    /// This region cut down to its first `kept` instructions.
    ///
    /// The cut is on the instruction grid, never inside one, which is what makes
    /// the result a region rather than a range: the bytes handed to the lifter
    /// must end where an instruction ends, or the lifter's own linear decode of
    /// them disagrees with [`starts`](Region::starts) about where the last one
    /// begins.
    ///
    /// `None` for a cut that would leave nothing, and for one that keeps
    /// everything — the caller wanting a shorter region has nothing to do with
    /// the region it already has.
    ///
    /// Used by the code cache to salvage a region microlift refused; see
    /// `CodeCache::longest_liftable_prefix`.
    pub fn truncated_to(&self, kept: usize) -> Option<Region> {
        if kept == 0 || kept >= self.starts.len() {
            return None;
        }
        // The new end is where the first dropped instruction began.
        let end = *self.starts.get(kept)?;
        Some(Region {
            range: self.range.start..end,
            starts: self.starts[..kept].to_vec(),
        })
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
        && let Some(r) = walk(bytes, base, f.start, bound)
            .map(capped)
            .filter(|r| r.covers(pc))
    {
        return Some(r);
    }
    // A region starting at `pc` always covers it — `pc` is its first
    // instruction — so the cap can never make this one unusable, unlike the
    // function-start attempt above, which falls through to here when the cut
    // lands in front of `pc`.
    walk(bytes, base, pc, bound).map(capped)
}

/// `region` cut down to [`MAX_REGION_INSTRUCTIONS`], if it is longer.
fn capped(region: Region) -> Region {
    region
        .truncated_to(MAX_REGION_INSTRUCTIONS)
        .unwrap_or(region)
}

/// Whether the `len` bytes of an instruction at `off` are all zero.
///
/// The end of what anyone has written; see the module note.
fn is_all_zero(bytes: &[u8], off: usize, len: usize) -> bool {
    bytes
        .get(off..off + len)
        .is_some_and(|b| b.iter().all(|&x| x == 0))
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
            let Ok(off) = usize::try_from(ip.wrapping_sub(base)) else {
                break;
            };
            if off >= bytes.len() {
                break;
            }
            decoder.set_position(off).ok()?;
            decoder.set_ip(ip);
            decoder.decode_out(&mut instr);
            if instr.is_invalid() || is_all_zero(bytes, off, instr.len()) {
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
                FlowControl::Return | FlowControl::IndirectBranch | FlowControl::Exception => break,
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
    Some(Region {
        range: start..end,
        starts,
    })
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
        assert_eq!(
            range(&code, BASE + 2, Some(BASE..BASE + 3)),
            Some(BASE..BASE + 3)
        );
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
        assert_eq!(
            range(&code, BASE, Some(BASE..BASE + 4)),
            Some(BASE..BASE + 1)
        );
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
    fn a_run_of_zeros_ends_the_region_rather_than_decoding_forever() {
        // `00 00` is `add byte ptr [rax], al`, so a walk through zero-filled
        // memory never stops on its own. In a JIT arena those zeros are bytes
        // the guest has not written yet, and a region that claimed them would be
        // served from the cache after it *had* written them. See the module note.
        let mut code = vec![0x48, 0x83, 0xEC, 0x08]; // sub rsp, 8
        code.extend(std::iter::repeat_n(0u8, 64));
        assert_eq!(
            range(&code, BASE, None),
            Some(BASE..BASE + 4),
            "the region must stop where the written code does"
        );
    }

    #[test]
    fn a_zero_byte_inside_a_real_instruction_is_not_the_end() {
        // The rule is about instructions that are *entirely* zero, not about
        // zero bytes — an immediate or a displacement is full of them, and
        // stopping there would truncate ordinary code.
        // mov eax, 0x00000001 ; ret
        let code = [0xB8, 0x01, 0x00, 0x00, 0x00, 0xC3];
        assert_eq!(range(&code, BASE, None), Some(BASE..BASE + 6));
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
        // *walk* rather than the instruction budget. Asserted against `walk`
        // because `region_for` now caps a region ahead of either totality
        // bound — see `a_region_is_capped_at_a_compilable_length` — and a test
        // that went through it would be checking the cap twice and these
        // ceilings not at all.
        let code = [0x66u8, 0x90].repeat((MAX_REGION as usize / 2) + 8);
        let r = walk(&code, BASE, BASE, u64::MAX).unwrap();
        assert_eq!(r.range.end - r.range.start, MAX_REGION);
    }

    /// The cap `region_for` applies, and the two things it must not break.
    #[test]
    fn a_region_is_capped_at_a_compilable_length() {
        // One-byte `nop`s, so instruction count and byte count agree and the
        // cut is unambiguous.
        let code = vec![0x90u8; MAX_REGION_INSTRUCTIONS * 2];
        let r = region_for(&code, BASE, BASE, None).unwrap();
        assert_eq!(r.starts.len(), MAX_REGION_INSTRUCTIONS);
        assert_eq!(r.range, BASE..BASE + MAX_REGION_INSTRUCTIONS as u64);

        // The cut is on the instruction grid, so the region can still be
        // entered at its own addresses and nowhere between them.
        assert!(r.covers(r.range.end - 1));
        assert!(!r.covers(r.range.end));

        // And an entry *past* the cut gets a region of its own that covers it,
        // rather than the truncated one that does not. This is the whole reason
        // capping is safe: the bytes past the cap are reached from their own PC.
        let later = BASE + MAX_REGION_INSTRUCTIONS as u64 + 7;
        let r = region_for(&code, BASE, later, None).unwrap();
        assert_eq!(r.range.start, later);
        assert!(r.covers(later));
    }

    /// A cap that lands in front of `pc` must not produce a region that cannot
    /// execute it.
    ///
    /// `region_for` prefers to start at the enclosing function's first
    /// instruction, so that every call site shares one translation. For a
    /// function longer than the cap that preference is unsatisfiable, and the
    /// failure it would cause is the silent one this module exists to prevent:
    /// microlift enters a translation that does not cover the address at its
    /// *first block* instead of failing.
    #[test]
    fn a_capped_region_that_would_miss_the_entry_starts_at_the_entry_instead() {
        let code = vec![0x90u8; MAX_REGION_INSTRUCTIONS * 2];
        let function = Some(BASE..BASE + code.len() as u64);
        let pc = BASE + MAX_REGION_INSTRUCTIONS as u64 + 100;
        let r = region_for(&code, BASE, pc, function).unwrap();
        assert!(r.covers(pc), "{:#x?} does not cover {pc:#x}", r.range);
        assert_eq!(r.range.start, pc);
    }

    #[test]
    fn an_unterminated_run_stops_at_the_instruction_budget() {
        // Against `walk`, for the reason above: this is the totality bound, and
        // `region_for`'s cap would stop it long before the budget did.
        let code = vec![0x90u8; MAX_INSTRUCTIONS + 16];
        let r = walk(&code, BASE, BASE, u64::MAX).unwrap();
        assert_eq!(r.range.end - r.range.start, MAX_INSTRUCTIONS as u64);
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
        assert!(
            !r.covers(BASE + 3),
            "an immediate byte is not an instruction"
        );
    }
}
