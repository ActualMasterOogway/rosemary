//! Which bytes to lift, given an address control is about to reach.
//!
//! This is a port of FEX-Emu's `FEXCore::Frontend::Decoder`
//! (`FEXCore/Source/Interface/Core/Frontend.cpp`,
//! `DecodeInstructionsAtEntry` and the three functions around it). The rules
//! below are its rules, and where this file departs from them the comment says
//! so and why.
//!
//! ## A region is a set of basic blocks, not an interval
//!
//! That is the whole of the redesign, and everything else follows from it.
//!
//! The previous version of this module walked control flow, collected the
//! instructions it reached, and then **trimmed the answer to the first
//! contiguous run** — because the lifter took one byte slice and a slice cannot
//! have holes in it. So a jump table in the middle of a function ended the
//! region in front of it, and the switch's case bodies past the table became a
//! separate translation each, reached one at a time through `UnknownJump`.
//! Alignment padding between two blocks did the same. So did Mono's inline call
//! arguments, except worse: those *do* decode, into whatever they happen to
//! mean, and one `mov ds, edx` inside an eight-byte pointer refused the whole
//! region — which is what [`crate::cache`]'s prefix bisection existed to
//! salvage, at up to a dozen extra lift-and-compile attempts each.
//!
//! FEX has no such trim, because its decoder hands the IR generator a *vector of
//! blocks* and the bytes between them are simply not in it. Microlift now takes
//! the same thing ([`LifterConfig::block_ranges`]), so this module can too: the
//! holes are named, never decoded, and an edge into one is an ordinary
//! out-of-region exit the runtime answers with a region of its own — the same
//! answer it already gave for an edge past the end.
//!
//! [`LifterConfig::block_ranges`]: microlift::core::LifterConfig::block_ranges
//!
//! ## The walk
//!
//! A worklist of block entries, lowest address first, starting at `pc`. Each is
//! decoded forward until something ends the block, and what ends one is:
//!
//! - **A terminator.** `ret`, an indirect jump, `ud2`. Nothing further is
//!   statically reachable, which is precisely the case the JIT loop answers.
//! - **A branch**, whose targets are added to the worklist. Both edges of a
//!   conditional; the single target of an unconditional near jump.
//! - **A call**, whose *resume point* is added — never the callee, which is a
//!   region of its own. The resume point also becomes a declared entry point.
//! - **Running into the next known block**, which is what
//!   [`Walk::next_block_start`] tracks. Two blocks never overlap and no
//!   instruction is decoded twice.
//! - **An instruction that will not decode.** In a non-entry block this
//!   *deletes the block* and the walk carries on; only the entry block's own
//!   bytes have to be code. FEX's comment on that line is "Improves
//!   compile-times", and it is also what makes garbage reachable from a
//!   mispredicted edge cost nothing.
//! - **The instruction budget**, [`max_instructions`], counted across the whole
//!   region rather than per block.
//!
//! A branch *backwards* into a block already decoded splits that block in two at
//! the target, so the target becomes a block entry rather than a second
//! translation ([`Walk::add_branch_target`]). If the target is not on that
//! block's instruction grid it is left alone and becomes an edge out of the
//! region, which is the honest answer: those bytes mean something different read
//! from there.
//!
//! ## The two bounds that keep a region local
//!
//! Neither is a correctness property. Both are about what a compiler is asked to
//! swallow at once.
//!
//! - **Nothing before the entry.** A target below `pc` is not followed. FEX's
//!   `TargetRIP >= EntryPoint`.
//! - **[`MAX_FORWARD_BRANCH_DIST`], four guest pages.** A branch further forward
//!   than that is not followed either. FEX's note on this is worth repeating
//!   verbatim, because the reasoning is not obvious: it makes "the cost code
//!   better match the guest code layout, avoiding massive (range-wise) code
//!   blocks in highly fragmented guest code. Such branches are often not-taken
//!   branches to garbage in obfuscated code."
//!
//! ## Zeros are memory nobody has written, not an instruction
//!
//! `00 00` decodes cleanly as `add byte ptr [rax], al`, so a decoder walking
//! forward through zero-filled memory never stops on its own: it produces a
//! plausible instruction every two bytes until it runs out of buffer. In an
//! image that is a nuisance — padding is `int3` or `nop`, so it rarely comes up.
//! In memory the guest **generates code into** it is fatal, and Terraria is
//! where that showed: an anonymous mapping is zero-filled, so a zero byte in a
//! Mono arena is a byte the guest has not written *yet*. One region ran off the
//! end of a freshly emitted method through a quarter-megabyte of untouched
//! arena, and then *claimed* every address in it — so when Mono compiled its
//! next method into that space and jumped to it, the cache served the zeros.
//!
//! FEX stops on two consecutive zero bytes, and this does too. The one
//! difference is where: FEX exempts its whole entry block, this exempts only the
//! entry *instruction*. Exempting the instruction is what guarantees progress
//! (a region always holds at least the instruction it was made for); exempting
//! the whole block would leave the zero-filled-arena case open, since the run
//! into the zeros is usually a fallthrough inside the entry block itself.
//!
//! ## Mono, and calls in memory the guest can write
//!
//! FEX carries one deliberate hack for Mono and this port carries it: **in
//! writable executable memory, a call ends its block and its resume point is not
//! followed.** FEX's reasoning is that "Mono generated code often contains
//! noreturn calls with garbage following them, and calls are always backpatched
//! after CIL compilation leading to n recompiles for a multiblock with n calls",
//! and it chooses "to minimize stutters over raw performance".
//!
//! Both halves apply here. The garbage after a call is Mono's inline argument —
//! read by the trampoline through its own return address, never executed — and
//! it is exactly what used to refuse a whole region and send it through prefix
//! bisection. The backpatching is worse and was invisible: every time Mono
//! rewrites a call site, a region that decoded past it is stale, and a region
//! that stops at it is not.

use std::collections::BTreeSet;
use std::ops::Range;

use iced_x86::{Decoder, DecoderOptions, FlowControl, Instruction};
use static_init::dynamic;

/// Ceiling on how many instructions one region may contain, across all of its
/// blocks. FEX's `MaxInstPerBlock` (`FEX_MAXINST`), whose default is 5000.
///
/// It is 1024 here, and the gap is not a disagreement about region shape — it is
/// the backend. FEX emits arm64 from its own IR in roughly a microsecond per
/// guest instruction; this runtime measures ~24 µs through Cranelift, whose
/// register allocator is moreover **superlinear in live ranges**, so a big
/// region is disproportionately expensive rather than proportionately so. The
/// outliers that produces are extreme — 4841 instructions once cost 21.7s — and
/// they are all in generated code, where nothing bounds a walk the way a
/// function's end does.
///
/// **What a cap costs is small.** The bytes past it are reached later from their
/// own PC, so the price is one extra region at the boundary and the
/// cross-block optimisation Cranelift would have done across it. On a program
/// with no region this long it costs exactly nothing.
///
/// **The value is measured.** Four minutes of Terraria's start-up, same binary,
/// cap varied — time spent compiling, the single worst compile, and how far the
/// guest got:
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
/// bottleneck once the outliers are gone. So the choice is the largest value
/// that removes them: real functions a compiler emits are hundreds of
/// instructions, so 1024 is above essentially everything an ordinary program
/// contains and below everything that hurt.
const MAX_REGION_INSTRUCTIONS: usize = 1024;

/// How far forward a branch may point and still be followed.
///
/// FEX's `MAX_FORWARD_BRANCH_DIST`, `FEX_PAGE_SIZE * 4`, and the guest's page is
/// the one that matters: this is a claim about how the *guest's compiler* laid
/// its code out, not about how the host maps it.
const MAX_FORWARD_BRANCH_DIST: u64 = 4 * 4096;

/// The guest page the zero-byte rule is confined to. See the module note.
const GUEST_PAGE: u64 = 4096;

/// The longest an x86-64 encoding can be. FEX's `MAX_INST_SIZE`.
const MAX_INSTRUCTION_LEN: u64 = 15;

/// The environment variables this module reads. FEX has the first two as
/// `FEX_MAXINST` and `FEX_MULTIBLOCK`; the rest are constants there, and are
/// knobs here because the backend they are being tuned against is not FEX's.
pub const CAP_ENV: &str = "ROSETTA3_REGION_CAP";
pub const MULTIBLOCK_ENV: &str = "ROSETTA3_MULTIBLOCK";
pub const FORWARD_ENV: &str = "ROSETTA3_REGION_FORWARD";
pub const MONO_ENV: &str = "ROSETTA3_REGION_MONO";

/// Read a `usize` knob once, falling back to `default`.
fn knob(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(default)
}

/// The instruction cap in force, which is [`MAX_REGION_INSTRUCTIONS`] unless
/// `ROSETTA3_REGION_CAP` says otherwise.
pub fn max_instructions() -> usize {
    #[dynamic]
    static CAP: usize = match knob(CAP_ENV, MAX_REGION_INSTRUCTIONS) {
        0 => MAX_REGION_INSTRUCTIONS,
        n => n,
    };
    *CAP
}

/// Whether a region may hold more than one basic block. FEX's `FEX_MULTIBLOCK`,
/// on by default there and here.
///
/// Off, a region is exactly one block: it ends at the first branch, call or
/// return and follows nothing. FEX's own note on the option is that multiblock
/// "can cause long JIT compilation times and stutter", which is the trade this
/// runtime feels more sharply than FEX does — so the knob is worth having even
/// though the default matches.
fn multiblock() -> bool {
    #[dynamic]
    static ON: bool = knob(MULTIBLOCK_ENV, 1) != 0;
    *ON
}

/// How far forward a branch may point and still be followed, in bytes.
/// [`MAX_FORWARD_BRANCH_DIST`] unless `ROSETTA3_REGION_FORWARD` says otherwise.
fn forward_limit() -> u64 {
    #[dynamic]
    static DIST: u64 = knob(FORWARD_ENV, MAX_FORWARD_BRANCH_DIST as usize) as u64;
    *DIST
}

/// Whether the Mono call rule applies in writable executable memory. On by
/// default; see the module note for what it is and why FEX has it.
fn mono_rule() -> bool {
    #[dynamic]
    static ON: bool = knob(MONO_ENV, 1) != 0;
    *ON
}

/// One basic block: a run of instructions entered at one address.
#[derive(Clone, Debug, PartialEq, Eq)]
struct DecodedBlock {
    /// Guest addresses the block covers. Ends where its last instruction ends.
    range: Range<u64>,
    /// Every instruction start in it, ascending. Kept because a backward branch
    /// into the block has to be split on the grid or not at all.
    starts: Vec<u64>,
}

/// A region to lift: which bytes are code, and where its instructions begin.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Region {
    /// The span the blocks live in — the first block's entry to the last one's
    /// end. What the caller slices out of the image to hand the lifter.
    ///
    /// **Not** a claim that everything in it is code; see [`blocks`](Self::blocks).
    pub range: Range<u64>,
    /// The blocks, ascending and disjoint. Everything in [`range`](Self::range)
    /// and not in one of these is a hole, which the lifter is told about
    /// explicitly. See the module note.
    pub blocks: Vec<Range<u64>>,
    /// Every instruction start in every block, ascending.
    ///
    /// "Does this translation cover that address" is not a range test, and under
    /// this design it is not even a *block* test: microlift's entry dispatch can
    /// resume at a block leader and refuses anything else, so an address inside
    /// a block but not on its grid is not an entry. Kept for the region's own
    /// accounting and for [`covers`](Self::covers).
    pub starts: Vec<u64>,
    /// Where control can arrive from outside: the address the region was made
    /// for, and the resume point after every call that leaves it.
    ///
    /// FEX's `DecodedBlockInformation::EntryPoints`. Handed to the lifter as
    /// declared resume points, so returning into the middle of this region
    /// enters *this* translation rather than forcing a second one.
    pub entries: Vec<u64>,
}

impl Region {
    /// Whether the region can be entered at `pc`.
    pub fn covers(&self, pc: u64) -> bool {
        self.starts.binary_search(&pc).is_ok()
    }

    /// How many instructions it holds.
    pub fn instructions(&self) -> usize {
        self.starts.len()
    }
}

/// The region to lift so that `pc` can execute.
///
/// `bytes` are the containing mapping's contents and `base` the address of their
/// first byte; the mapping's end is the only upper bound on the walk, which is
/// FEX's `SectionMaxAddress`.
///
/// `writable` says whether these bytes are memory the guest can write — an
/// executable mapping it asked for rather than an image this runtime loaded. It
/// selects the Mono call rule; see the module note.
///
/// The region always begins **at `pc`**, as FEX's does. There is no attempt to
/// start at the enclosing function's first instruction: a call reaching that
/// function arrives at its entry and gets a region there anyway, so the sharing
/// that preference bought happens without it — and paying for it meant two
/// entries into one function competing for a single cache key.
///
/// `None` when `pc` is outside `bytes`, or when nothing at `pc` decodes.
pub fn region_for(bytes: &[u8], base: u64, pc: u64, writable: bool) -> Option<Region> {
    Walk::new(bytes, base, pc, writable).run()
}

/// The state of one region's decode. A transcription of FEX's `Decoder`
/// members, with the same names where they had good ones.
struct Walk<'a> {
    bytes: &'a [u8],
    base: u64,
    /// One past the last byte of the mapping. FEX's `SectionMaxAddress`.
    limit: u64,
    /// The address the region is being built for. Nothing below it is followed.
    entry_point: u64,
    /// Whether the bytes are memory the guest can write. See the module note.
    writable: bool,

    /// Blocks decoded so far, ascending by entry and disjoint.
    blocks: Vec<DecodedBlock>,
    /// Block entries still to decode, lowest first.
    to_decode: BTreeSet<u64>,
    /// Entries already taken off the worklist, so a target is never queued
    /// twice.
    visited: BTreeSet<u64>,
    /// Targets found while decoding the current block, merged into
    /// [`to_decode`](Self::to_decode) once it finishes — because a block that
    /// turns out to be garbage takes its targets with it.
    current_targets: BTreeSet<u64>,
    /// Where control can arrive from outside. See [`Region::entries`].
    entries: BTreeSet<u64>,
    /// The lowest address the current block must stop before: the next entry on
    /// the worklist, or the next block already decoded, whichever is nearer.
    next_block_start: u64,
    /// Instructions decoded across the whole region.
    total: usize,
    /// The cap, read once.
    max: usize,
    /// Whether the region may hold more than one block. See [`multiblock`].
    multiblock: bool,
    /// How far forward a branch may point. See [`forward_limit`].
    forward: u64,

    decoder: Decoder<'a>,
    instr: Instruction,
}

impl<'a> Walk<'a> {
    fn new(bytes: &'a [u8], base: u64, pc: u64, writable: bool) -> Self {
        Walk {
            bytes,
            base,
            limit: base.saturating_add(bytes.len() as u64),
            entry_point: pc,
            writable,
            blocks: Vec::new(),
            to_decode: BTreeSet::from([pc]),
            visited: BTreeSet::new(),
            current_targets: BTreeSet::new(),
            entries: BTreeSet::from([pc]),
            next_block_start: u64::MAX,
            total: 0,
            max: max_instructions(),
            multiblock: multiblock(),
            forward: forward_limit(),
            decoder: Decoder::with_ip(64, bytes, base, DecoderOptions::NONE),
            instr: Instruction::default(),
        }
    }

    /// FEX's `DecodeInstructionsAtEntry`.
    fn run(mut self) -> Option<Region> {
        if self.entry_point < self.base || self.entry_point >= self.limit {
            return None;
        }

        // The first block off the worklist is the one the region was made for,
        // and it is the only one whose bytes have to be code.
        let mut entry_block = true;
        let mut budget_spent = false;

        while !budget_spent {
            let Some(&rip) = self.to_decode.iter().next() else {
                break;
            };
            self.to_decode.remove(&rip);
            self.visited.insert(rip);

            // Where this block must stop: the nearest thing already claimed.
            let succ = self.blocks.partition_point(|b| b.range.start < rip);
            self.next_block_start = self
                .to_decode
                .iter()
                .next()
                .copied()
                .unwrap_or(u64::MAX)
                .min(self.blocks.get(succ).map_or(u64::MAX, |b| b.range.start))
                .min(self.limit);

            // Inserted before it is decoded, so a backward branch discovered
            // *within* this block can split it. FEX does the same.
            self.blocks.insert(
                succ,
                DecodedBlock {
                    range: rip..rip,
                    starts: Vec::new(),
                },
            );

            budget_spent = self.decode_block(succ, entry_block);

            // A block that decoded nothing is not a block. This is also how a
            // non-entry block that hit garbage disappears — `decode_block` has
            // already rolled its instructions back.
            if self.blocks[succ].starts.is_empty() {
                self.blocks.remove(succ);
            } else {
                self.to_decode.append(&mut self.current_targets);
            }
            self.current_targets.clear();
            entry_block = false;
        }

        self.finish()
    }

    /// Decode one block, in place at `idx`. Returns whether the instruction
    /// budget ran out, which ends the whole region rather than this block.
    fn decode_block(&mut self, idx: usize, entry_block: bool) -> bool {
        let entry = self.blocks[idx].range.start;
        let mut at = entry;
        loop {
            // Zeros are memory nobody has written. Exempting the region's own
            // first instruction is what makes the walk make progress; see the
            // module note on why the exemption is that narrow.
            if !(entry_block && at == entry) && self.is_zero_run(at) {
                break;
            }
            let Some(off) = self.offset_of(at) else { break };
            if self.decoder.set_position(off).is_err() {
                break;
            }
            self.decoder.set_ip(at);
            self.decoder.decode_out(&mut self.instr);
            let next = self.instr.next_ip();

            // The instruction would run into a block that is already claimed,
            // so it belongs to whoever claimed it. FEX leaves the few bytes
            // before the boundary as a hole rather than decoding them twice.
            if next > self.next_block_start {
                break;
            }

            if self.instr.is_invalid() {
                if entry_block {
                    // The bytes the region was made for are not code. Nothing
                    // to salvage and nothing to hide: the entry block keeps
                    // whatever it decoded, and if that is nothing the region
                    // does not exist.
                    break;
                }
                // Anywhere else this is a mispredicted edge — an embedded jump
                // table read as instructions, a `call`'s inline argument, the
                // cold side of a branch into padding. FEX drops the block and
                // carries on, and says why: it improves compile times.
                self.total -= self.blocks[idx].starts.len();
                self.blocks[idx].starts.clear();
                self.blocks[idx].range.end = entry;
                return false;
            }

            self.total += 1;
            self.blocks[idx].starts.push(at);
            self.blocks[idx].range.end = next;

            if self.total >= self.max {
                return true;
            }

            if !self.can_continue(next) {
                self.branch_targets(next);
                break;
            }
            at = next;
        }
        false
    }

    /// Whether the block continues past the instruction just decoded. FEX's
    /// `InstCanContinue`.
    fn can_continue(&self, next: u64) -> bool {
        if next == self.next_block_start {
            // Straight into the next block. It gets decoded on its own turn and
            // the two abut, so the lifter sees one run of bytes and puts the
            // fallthrough edge in.
            return false;
        }
        match self.instr.flow_control() {
            FlowControl::Next => true,
            // A `call` to the very next instruction is not a call: it is how
            // 32-bit position-independent code reads its own address. FEX
            // special-cases it and so does microlift's dispatch.
            FlowControl::Call if self.instr.near_branch_target() == next => true,
            _ => false,
        }
    }

    /// Queue whatever the block's terminator reaches. FEX's
    /// `BranchTargetInMultiblockRange`.
    ///
    /// `next` is the address after the terminator.
    fn branch_targets(&mut self, next: u64) {
        if !self.multiblock {
            return;
        }
        let (target, conditional) = match self.instr.flow_control() {
            FlowControl::Call | FlowControl::IndirectCall => {
                // The callee is a region of its own — following it would inline
                // the program into one translation unit. Only the resume point
                // is ours, and it is an entry point because control arrives
                // there from outside, when the callee returns.
                //
                // Except in memory the guest writes, where a call is where a
                // block ends. See the module note on Mono.
                if !(self.writable && mono_rule()) {
                    self.add_branch_target(next, next);
                    self.entries.insert(next);
                }
                return;
            }
            FlowControl::ConditionalBranch => (self.instr.near_branch_target(), true),
            FlowControl::UnconditionalBranch => (self.instr.near_branch_target(), false),
            // A return, an indirect jump, an exception. Nothing further is
            // reachable without running the program, which is the JIT's job.
            _ => return,
        };

        if conditional {
            // The not-taken edge is code whatever the taken one turns out to be.
            self.add_branch_target(next, next);
        }

        // The two bounds that keep a region local; see the module note.
        let forward_limit = next.saturating_add(self.forward).min(self.limit);
        if target >= self.entry_point && target < forward_limit {
            self.add_branch_target(target, next);
        }
    }

    /// Queue `target` as a block entry. FEX's `AddBranchTarget`.
    ///
    /// `after` is the address just past the instruction that named it, which is
    /// only used to decide whether the target can bound the *current* block.
    fn add_branch_target(&mut self, target: u64, after: u64) {
        if self.visited.contains(&target) {
            return;
        }
        let succ = self.blocks.partition_point(|b| b.range.start < target);
        if let Some(prev) = succ.checked_sub(1)
            && self.blocks[prev].range.end > target
        {
            // The target is inside a block already decoded. Split it there, so
            // the target becomes an entry of this region instead of the start of
            // a second translation over the same bytes.
            if let Ok(i) = self.blocks[prev].starts.binary_search(&target)
                && i != 0
            {
                let tail = self.blocks[prev].starts.split_off(i);
                let end = self.blocks[prev].range.end;
                self.blocks[prev].range.end = target;
                self.blocks.insert(
                    succ,
                    DecodedBlock {
                        range: target..end,
                        starts: tail,
                    },
                );
            }
            // Split or not, the bytes at `target` have been dealt with: either
            // they are now a block of their own, or the target is off the grid
            // and the branch simply leaves the region. Marking it visited stops
            // this from being reconsidered on every edge that names it.
            self.visited.insert(target);
            return;
        }

        self.current_targets.insert(target);
        // A target the current block would otherwise decode straight through is
        // where the current block has to stop.
        if target >= after && target < self.next_block_start {
            self.next_block_start = target;
        }
    }

    /// Whether the two bytes at `at` are zero and can be judged. See the module
    /// note; this is FEX's rule, including the page condition.
    ///
    /// The condition is that a whole maximal instruction starting here stays
    /// within one page, which is FEX declining to call bytes garbage when it
    /// cannot see all of the encoding they might belong to.
    fn is_zero_run(&self, at: u64) -> bool {
        let last = at.saturating_add(MAX_INSTRUCTION_LEN);
        if at / GUEST_PAGE != last / GUEST_PAGE {
            return false;
        }
        let Some(off) = self.offset_of(at) else {
            return false;
        };
        matches!(self.bytes.get(off..off + 2), Some([0, 0]))
    }

    /// `at` as an index into `bytes`, if it is in them.
    fn offset_of(&self, at: u64) -> Option<usize> {
        if at < self.base || at >= self.limit {
            return None;
        }
        usize::try_from(at - self.base).ok()
    }

    /// Assemble what the walk found, or `None` if it found nothing.
    fn finish(mut self) -> Option<Region> {
        self.blocks.retain(|b| !b.starts.is_empty());
        let first = self.blocks.first()?.range.start;
        let last = self.blocks.last()?.range.end;

        let starts: Vec<u64> = self
            .blocks
            .iter()
            .flat_map(|b| b.starts.iter())
            .copied()
            .collect();
        debug_assert!(starts.windows(2).all(|w| w[0] < w[1]), "blocks overlap");
        // A region has to be enterable at the address it was made for, or the
        // run loop asks for the same `pc` forever. The entry block is decoded
        // first and from `pc`, so this holds by construction — but it is the one
        // property whose failure is an infinite loop rather than an error.
        debug_assert_eq!(first, self.entry_point);

        let mut entries: Vec<u64> = self.entries.into_iter().collect();
        entries.retain(|e| starts.binary_search(e).is_ok());

        Some(Region {
            range: first..last,
            blocks: self.blocks.into_iter().map(|b| b.range).collect(),
            starts,
            entries,
        })
    }
}

#[cfg(test)]
// A `Vec<Range<u64>>` holding one range is what a one-block region *is*, so
// clippy's suggestion to read it as an iterator of addresses is exactly wrong
// here.
#[allow(clippy::single_range_in_vec_init)]
mod tests {
    use super::*;

    const BASE: u64 = 0x1_0000;

    fn region(bytes: &[u8], pc: u64) -> Option<Region> {
        region_for(bytes, BASE, pc, false)
    }

    /// The region as the lifter sees it: which bytes are code.
    fn blocks(bytes: &[u8], pc: u64) -> Option<Vec<Range<u64>>> {
        region(bytes, pc).map(|r| r.blocks)
    }

    #[test]
    fn a_straight_line_function_is_one_block_ending_at_its_ret() {
        // xor eax, eax ; ret ; (padding: int3)
        let code = [0x31, 0xC0, 0xC3, 0xCC, 0xCC];
        assert_eq!(blocks(&code, BASE), Some(vec![BASE..BASE + 3]));
    }

    /// The defining difference from the old walk, and the reason for the whole
    /// redesign: a hole does not end the region, it is a hole.
    #[test]
    fn a_forward_branch_over_a_hole_keeps_what_is_past_the_hole() {
        // jmp +2 ; two bytes never reached ; xor eax,eax ; ret
        let code = [0xEB, 0x02, 0xFF, 0xFF, 0x31, 0xC0, 0xC3];
        let r = region(&code, BASE).unwrap();
        assert_eq!(
            r.blocks,
            vec![BASE..BASE + 2, BASE + 4..BASE + 7],
            "the un-walked bytes are a hole between two blocks"
        );
        assert_eq!(r.range, BASE..BASE + 7);
        assert_eq!(r.starts, vec![BASE, BASE + 4, BASE + 6]);
    }

    #[test]
    fn a_conditional_branch_follows_both_edges() {
        // je +1 ; nop ; ret
        let code = [0x74, 0x01, 0x90, 0xC3];
        let r = region(&code, BASE).unwrap();
        // Three blocks: the branch, its fallthrough `nop`, and the `ret` both
        // edges reach.
        assert_eq!(
            r.blocks,
            vec![BASE..BASE + 2, BASE + 2..BASE + 3, BASE + 3..BASE + 4]
        );
        assert_eq!(r.starts, vec![BASE, BASE + 2, BASE + 3]);
    }

    /// An embedded jump table is unreachable as an instruction, so it is not in
    /// any block — and unlike before, the case bodies past it still are.
    #[test]
    fn an_embedded_jump_table_is_a_hole_and_not_the_end() {
        // je +6 ; jmp rax ; four bytes of table ; ret
        let code = [0x74, 0x06, 0xFF, 0xE0, 0x00, 0x00, 0x00, 0x00, 0xC3];
        let r = region(&code, BASE).unwrap();
        assert_eq!(
            r.blocks,
            vec![BASE..BASE + 2, BASE + 2..BASE + 4, BASE + 8..BASE + 9],
            "the table is neither decoded nor an end"
        );
    }

    #[test]
    fn a_call_does_not_pull_in_its_callee_but_does_resume_after_it() {
        // call +1 ; ret ; ret   — the callee at BASE+6 is someone else's region.
        let code = [0xE8, 0x01, 0x00, 0x00, 0x00, 0xC3, 0xC3];
        let r = region(&code, BASE).unwrap();
        assert_eq!(r.blocks, vec![BASE..BASE + 5, BASE + 5..BASE + 6]);
        assert!(
            r.entries.contains(&(BASE + 5)),
            "the resume point is where control arrives from outside"
        );
        assert!(r.entries.contains(&BASE));
    }

    /// FEX's Mono rule. See the module note.
    #[test]
    fn in_writable_memory_a_call_ends_the_block() {
        // call +1 ; then eight bytes of inline argument that decode as nonsense.
        let code = [0xE8, 0x01, 0x00, 0x00, 0x00, 0x8E, 0xDA, 0x11, 0x22];
        let r = region_for(&code, BASE, BASE, true).unwrap();
        assert_eq!(r.blocks, vec![BASE..BASE + 5]);
        assert!(!r.entries.contains(&(BASE + 5)));

        // The same bytes in an image: the resume point is followed, because
        // almost every call does return to it.
        let r = region_for(&code, BASE, BASE, false).unwrap();
        assert!(r.entries.contains(&(BASE + 5)));
    }

    /// A backward branch into the middle of a block splits it, so the target is
    /// an entry of this region rather than the start of a second translation of
    /// the same bytes.
    #[test]
    fn a_backward_branch_splits_the_block_it_lands_in() {
        // xor eax,eax ; nop ; dec eax ; jne -3 (to BASE+2) ; ret
        let code = [0x31, 0xC0, 0x90, 0xFF, 0xC8, 0x75, 0xFB, 0xC3];
        let r = region(&code, BASE).unwrap();
        assert_eq!(
            r.blocks,
            vec![BASE..BASE + 2, BASE + 2..BASE + 7, BASE + 7..BASE + 8],
            "the loop head is a block of its own"
        );
        assert_eq!(r.starts, vec![BASE, BASE + 2, BASE + 3, BASE + 5, BASE + 7]);
    }

    /// And a branch into the *middle of an instruction* is not a split: those
    /// bytes mean something else read from there, so the edge leaves the region.
    #[test]
    fn a_branch_off_the_instruction_grid_is_not_a_split() {
        // mov eax, imm32 ; jmp -4, landing inside the immediate.
        let code = [0xB8, 0x01, 0x00, 0x00, 0x00, 0xEB, 0xFA, 0xC3];
        let r = region(&code, BASE).unwrap();
        assert_eq!(r.blocks, vec![BASE..BASE + 7]);
        assert!(!r.covers(BASE + 3));
    }

    #[test]
    fn a_backward_jump_loop_terminates() {
        // A tight infinite loop: `jmp -2`. The walk must not be one.
        let code = [0xEB, 0xFE];
        assert_eq!(blocks(&code, BASE), Some(vec![BASE..BASE + 2]));
    }

    #[test]
    fn nothing_before_the_entry_is_followed() {
        // ret ; jmp -3 (back to BASE, which is below the entry at BASE+1)
        let code = [0xC3, 0xEB, 0xFD];
        let r = region(&code, BASE + 1).unwrap();
        assert_eq!(r.blocks, vec![BASE + 1..BASE + 3]);
        assert_eq!(r.range.start, BASE + 1);
    }

    #[test]
    fn a_branch_further_than_the_forward_limit_is_not_followed() {
        // jmp rel32, past MAX_FORWARD_BRANCH_DIST.
        let far = MAX_FORWARD_BRANCH_DIST as u32 + 16;
        let mut code = vec![0xE9];
        code.extend_from_slice(&far.to_le_bytes());
        code.resize(far as usize + 32, 0x90);
        code.push(0xC3);
        let r = region(&code, BASE).unwrap();
        assert_eq!(
            r.blocks,
            vec![BASE..BASE + 5],
            "a distant branch leaves the region rather than stretching it"
        );
    }

    #[test]
    fn an_address_outside_the_image_has_no_region() {
        let code = [0xC3];
        assert_eq!(blocks(&code, BASE + 64), None);
        assert_eq!(blocks(&code, BASE - 1), None);
    }

    #[test]
    fn undecodable_bytes_at_the_entry_yield_nothing() {
        let code = [0x0F, 0xFF];
        assert_eq!(blocks(&code, BASE), None);
    }

    /// The rule that makes a mispredicted edge free. Only the entry block's
    /// bytes have to be code.
    #[test]
    fn a_non_entry_block_that_will_not_decode_is_dropped() {
        // je +2 ; ret ; 0f ff — the taken edge is garbage, the region is not.
        let code = [0x74, 0x01, 0xC3, 0x0F, 0xFF];
        let r = region(&code, BASE).unwrap();
        assert_eq!(r.blocks, vec![BASE..BASE + 2, BASE + 2..BASE + 3]);
        assert_eq!(r.instructions(), 2);
    }

    #[test]
    fn a_run_of_zeros_ends_a_block_rather_than_decoding_forever() {
        // `00 00` is `add byte ptr [rax], al`, so a walk through zero-filled
        // memory never stops on its own. In a JIT arena those zeros are bytes
        // the guest has not written yet. See the module note.
        let mut code = vec![0x48, 0x83, 0xEC, 0x08]; // sub rsp, 8
        code.extend(std::iter::repeat_n(0u8, 64));
        assert_eq!(
            blocks(&code, BASE),
            Some(vec![BASE..BASE + 4]),
            "the region must stop where the written code does"
        );
    }

    /// The exemption that keeps the walk making progress: a region always holds
    /// the instruction it was made for, even if that instruction is two zeros.
    #[test]
    fn the_entry_instruction_is_exempt_from_the_zero_rule() {
        let code = [0u8; 32];
        let r = region(&code, BASE).unwrap();
        assert_eq!(r.blocks, vec![BASE..BASE + 2]);
        assert!(r.covers(BASE));
    }

    #[test]
    fn a_zero_byte_inside_a_real_instruction_is_not_the_end() {
        // The rule is about a run of zeros where an instruction would start, not
        // about zero bytes — an immediate is full of them.
        // mov eax, 0x00000001 ; ret
        let code = [0xB8, 0x01, 0x00, 0x00, 0x00, 0xC3];
        assert_eq!(blocks(&code, BASE), Some(vec![BASE..BASE + 6]));
    }

    #[test]
    fn a_long_run_of_code_stops_at_the_instruction_cap() {
        let cap = max_instructions();
        let code = vec![0x90u8; cap * 2];
        let r = region(&code, BASE).unwrap();
        assert_eq!(r.instructions(), cap);
        assert_eq!(r.range, BASE..BASE + cap as u64);

        // And an entry past the cap gets a region of its own that covers it,
        // which is what makes capping safe.
        let later = BASE + cap as u64 + 7;
        let r = region(&code, later).unwrap();
        assert_eq!(r.range.start, later);
        assert!(r.covers(later));
    }

    /// Blocks never overlap, whatever order the worklist reaches them in: a
    /// block stops in front of one already claimed.
    #[test]
    fn two_blocks_never_claim_the_same_byte() {
        // je +4 ; nop ; nop ; nop ; nop ; ret — the taken edge lands inside the
        // fallthrough run, so the fallthrough block must stop there.
        let code = [0x74, 0x04, 0x90, 0x90, 0x90, 0x90, 0xC3];
        let r = region(&code, BASE).unwrap();
        let mut sorted = r.blocks.clone();
        sorted.sort_by_key(|b| b.start);
        assert_eq!(r.blocks, sorted);
        assert!(
            r.blocks.windows(2).all(|w| w[0].end <= w[1].start),
            "{:#x?} overlap",
            r.blocks
        );
        assert!(r.covers(BASE + 6));
    }

    #[test]
    fn coverage_is_the_instruction_grid_and_not_the_range() {
        // nop ; mov eax, imm32 ; ret — the four immediate bytes are inside the
        // region and are not addresses it can be entered at.
        let code = [0x90, 0xB8, 0x01, 0x00, 0x00, 0x00, 0xC3];
        let r = region(&code, BASE).unwrap();
        assert_eq!(r.range, BASE..BASE + 7);
        assert_eq!(r.starts, vec![BASE, BASE + 1, BASE + 6]);
        assert!(r.covers(BASE + 6));
        assert!(
            !r.covers(BASE + 3),
            "an immediate byte is not an instruction"
        );
    }
}
