//! The linking **transcript**.
//!
//! `selfish` returns a `LoadedImage` and stops (`selfish/src/loader.rs:133-139`),
//! which is the right answer when the loaded code just runs: the CPU follows
//! the pointers the linker wrote and nobody needs to know what they were.
//!
//! Rosetta 3 needs the opposite. Guest code is *recompiled*, and every call
//! into a host shim has to be recognized at lift time and turned into a native
//! call rather than followed into bytes the lifter would try to translate. That
//! means the loader must report not just that binding succeeded but, for each
//! binding, every address a call site might name — which is what
//! [`BindingRecord`] is.

use alloc::string::String;
use alloc::vec::Vec;

use crate::error::{Error, Result};
use crate::fixups::BindKind;
use crate::raw::Word;

/// One import, resolved.
///
/// Two addresses, not one, and the second is the load-bearing one. See
/// [`crate::fixups::stubs`] for why a classic x86-64 image reports zero
/// intercepted call sites if only `slot_va` is published.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BindingRecord {
    /// Slid VA of the pointer slot that was written. Pass this to
    /// `register_fast_import` to match a `call [rip+disp]` site, which is what
    /// the recompiler compares against `memory_displacement64()`.
    pub slot_va: Word,
    /// Slid VA of the `__stubs` entry that jumps through `slot_va`, when the
    /// image has one. Pass this to match a `call rel32` site, which is the
    /// shape of essentially every import call in a classic image.
    pub stub_va: Option<Word>,
    /// Install name the import asked for, absent for a flat or special
    /// reference.
    pub library: Option<String>,
    /// Symbol name, leading underscore preserved.
    pub symbol: String,
    /// The address written into the slot. Zero for a weak import that no
    /// provider satisfied — a legitimate outcome, and the reason
    /// [`BindingRecord::is_satisfied`] exists rather than a `resolved != 0`
    /// check scattered through callers.
    pub resolved: Word,
    pub kind: BindKind,
    pub weak_import: bool,
    /// Verbatim echo of [`crate::resolve::Resolved::tag`].
    pub tag: u64,
}

impl BindingRecord {
    /// Whether some provider actually answered.
    pub fn is_satisfied(&self) -> bool {
        self.resolved != 0
    }
}

/// An import nothing could satisfy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MissingSymbol {
    pub symbol: String,
    pub library: Option<String>,
    /// Slid VA of the slot left unwritten.
    pub slot_va: Word,
    /// Whether the image tolerates the absence.
    pub weak_import: bool,
}

/// Everything binding one image produced.
#[derive(Clone, Debug, Default)]
pub struct ImageReport {
    /// The image's install name, or its path, for diagnostics.
    pub name: String,
    pub base: Word,
    pub slide: Word,
    /// Which fixup encoding the image used, from
    /// [`crate::fixups::FixupDecoder::name`].
    pub encoding: &'static str,
    pub bindings: Vec<BindingRecord>,
    /// Imports left unsatisfied. Non-weak entries here mean the image will
    /// fault the first time it uses one.
    pub missing: Vec<MissingSymbol>,
    /// How many pointers were slid.
    pub rebases: usize,
    /// Slid VAs of `S_MOD_INIT_FUNC_POINTERS` entries, in section order.
    /// Collected, never called — running them means executing guest code, which
    /// is the driver's job.
    pub initializers: Vec<Word>,
    /// Slid VAs of `S_MOD_TERM_FUNC_POINTERS` entries. Reported, never run.
    pub terminators: Vec<Word>,
    /// Thread-local descriptors, enumerated and not otherwise honoured.
    pub thread_locals: Vec<ThreadLocalDescriptor>,
    /// Where each segment landed, with the file bytes backing it, so a driver
    /// can hand exact ranges to a recompiler without re-reading memory.
    pub segments: Vec<SegmentReport>,
    /// Entry point, if the image declares one.
    pub entry: Option<Word>,
}

impl ImageReport {
    /// Imports that are missing *and* not weak.
    pub fn hard_missing(&self) -> impl Iterator<Item = &MissingSymbol> {
        self.missing.iter().filter(|m| !m.weak_import)
    }

    /// `Err` if any non-weak import went unresolved.
    ///
    /// Separate from binding itself so the caller sees the *whole* list rather
    /// than whichever symbol happened to be decoded first — with a shim table
    /// under construction, "these forty symbols are missing" is the useful
    /// message and "`_strlen` is missing" is forty round trips.
    pub fn require_all_resolved(&self) -> Result<()> {
        let mut it = self.hard_missing();
        let Some(first) = it.next() else { return Ok(()) };
        let rest = it.count();
        let mut msg = first.symbol.clone();
        if rest > 0 {
            msg.push_str(" (and ");
            push_usize(&mut msg, rest);
            msg.push_str(" more)");
        }
        Err(Error::UnresolvedSymbol(msg))
    }
}

/// One thread-local variable's `tlv_descriptor`, as found in
/// `S_THREAD_LOCAL_VARIABLES`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ThreadLocalDescriptor {
    /// Slid VA of the descriptor itself.
    pub at: Word,
    /// The thunk a guest calls to get the variable's per-thread address. In an
    /// unbound image this is a `__tlv_bootstrap` import.
    pub thunk: Word,
    /// The pthread key, zero until the runtime allocates one. We never do.
    pub key: Word,
    /// Byte offset of this variable within the thread's block.
    pub offset: Word,
}

/// Where a segment landed and which file bytes back it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SegmentReport {
    pub name: String,
    /// Slid VA of the segment start.
    pub vmaddr: Word,
    pub vmsize: Word,
    /// Byte range within the (thin) image backing it, absent for zero-fill.
    pub file_range: Option<core::ops::Range<usize>>,
    /// Whether the segment holds code the recompiler should lift.
    pub executable: bool,
}

/// Every image linked together, in load order.
#[derive(Clone, Debug, Default)]
pub struct LinkReport {
    pub images: Vec<ImageReport>,
}

impl LinkReport {
    /// The first image, which by construction is the one the caller asked for.
    pub fn root(&self) -> Option<&ImageReport> {
        self.images.first()
    }

    /// Every binding across every image.
    pub fn bindings(&self) -> impl Iterator<Item = &BindingRecord> {
        self.images.iter().flat_map(|i| i.bindings.iter())
    }

    /// `Err` naming a missing symbol if any image has an unsatisfied hard
    /// import.
    pub fn require_all_resolved(&self) -> Result<()> {
        for img in &self.images {
            img.require_all_resolved()?;
        }
        Ok(())
    }
}

/// Append a decimal `usize`. `core::fmt` would do this, but pulling a
/// `Formatter` in for one number is more machinery than the job needs.
fn push_usize(s: &mut String, mut v: usize) {
    if v == 0 {
        s.push('0');
        return;
    }
    let mut buf = [0u8; 20];
    let mut i = buf.len();
    while v > 0 {
        i -= 1;
        buf[i] = b'0' + (v % 10) as u8;
        v /= 10;
    }
    for &b in &buf[i..] {
        s.push(b as char);
    }
}
