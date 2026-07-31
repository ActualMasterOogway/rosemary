//! What an image *needs*: the normalized fixup stream, and the decoders that
//! produce it from each of Mach-O's three encodings.
//!
//! ## Why this factors on format, not on entry kind
//!
//! `selfish`'s `RelocEngine` (`selfish/src/spec/reloc.rs:28-46`) both
//! interprets a relocation *and* writes the result, and factors on relocation
//! *kind*, because that is the shape of the problem in ELF: one homogeneous
//! table of heterogeneous entries, each with its own arithmetic
//! (`R_X86_64_GLOB_DAT` versus `R_X86_64_RELATIVE` versus a TLS offset).
//!
//! Mach-O is the transpose. There are three whole-file encodings —
//! [`dyld_info`] opcode streams, [`chained`] pointer chains, and [`classic`]
//! indirect symbols — that are mutually exclusive per image, and whose
//! arithmetic barely varies once decoded: almost everything is "store a
//! pointer at this address". Factoring on format is therefore the honest cut,
//! and it buys something ELF's shape does not offer:
//!
//! **Decoders are pure.** [`FixupDecoder::decode`] takes an image and produces
//! [`Fixup`] values. It touches no address space, resolves no symbol, and
//! writes nothing. Three consequences follow, and they are the whole reason for
//! the design:
//!
//! 1. A decoder can be tested by inspecting its output, with no memory, no
//!    providers, and no host.
//! 2. The two encodings a modern toolchain can be made to emit for the *same*
//!    source can be diffed against each other. That cross-check lives in
//!    `tests/fixups_agree.rs` and is the strongest evidence of correctness in
//!    the crate, because the encodings are independent expressions of one
//!    intent.
//! 3. [`Binder`] — the part that resolves and writes — is written once, for all
//!    formats, instead of once per format.

#[cfg(feature = "fixups-chained")]
pub mod chained;
#[cfg(feature = "fixups-classic")]
pub mod classic;
#[cfg(feature = "fixups-dyld-info")]
pub mod dyld_info;

mod binder;
mod stubs;

pub use binder::{BindOutcome, Binder};
pub use stubs::StubMap;

use alloc::vec::Vec;

use crate::error::Result;
use crate::image::MachObject;
use crate::raw::Word;
use crate::spec::MachSpec;

/// How a slot came to need a value, preserved through resolution because the
/// embedder's interception policy differs by kind: a lazy slot has a `__stubs`
/// entry and so two call-site shapes to intercept, a non-lazy one usually does
/// not.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum BindKind {
    /// `__got` / `__nl_symbol_ptr`: bound before the image runs.
    NonLazy,
    /// `__la_symbol_ptr`: dyld would bind on first call. **We bind eagerly**,
    /// so the kind is a record of provenance rather than of timing. See the
    /// crate scope notes for what that costs.
    Lazy,
    /// A `weak_bind` entry, participating in cross-image coalescing that this
    /// crate does not implement.
    WeakCoalesce,
    /// From a chained-fixups import chain.
    Chained,
    /// From the classic indirect symbol table.
    Indirect,
}

/// Which image an import is expected to come from.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum LibraryRef<'a> {
    /// A two-level reference naming a specific dependency by install name.
    InstallName(&'a str),
    /// `BIND_SPECIAL_DYLIB_SELF`: defined by this image.
    SelfImage,
    /// `BIND_SPECIAL_DYLIB_MAIN_EXECUTABLE`: a callback into the executable,
    /// which is how a plugin bundle reaches its host.
    MainExecutable,
    /// `BIND_SPECIAL_DYLIB_FLAT_LOOKUP`: search every image in order.
    Flat,
    /// `BIND_SPECIAL_DYLIB_WEAK_LOOKUP`: bind only if some other image already
    /// defines this name weakly; otherwise leave the slot alone.
    WeakLookup,
    /// An ordinal with no matching `LC_LOAD_DYLIB`. Carried rather than
    /// rejected so a diagnostic can name the number, and so one broken import
    /// does not deny the caller the rest of the stream.
    UnknownOrdinal(u16),
}

impl LibraryRef<'_> {
    /// The install name this reference names, if it names one.
    pub fn install_name(&self) -> Option<&str> {
        match self {
            LibraryRef::InstallName(n) => Some(n),
            _ => None,
        }
    }

    /// Whether resolution should search every provider rather than one.
    pub fn is_flat(&self) -> bool {
        matches!(self, LibraryRef::Flat | LibraryRef::WeakLookup)
    }
}

/// A reference to a symbol some other image is expected to define.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct SymbolRef<'a> {
    /// The name **with** its leading underscore, exactly as the image spells
    /// it. Kept intact so it round-trips against [`crate::exports::Export`];
    /// only the `dlsym` provider strips it, at the last possible moment,
    /// because that is the one place the underscore is wrong.
    pub name: &'a str,
    pub library: LibraryRef<'a>,
    /// Absence is not an error; the slot binds to zero instead.
    pub weak_import: bool,
}

/// One decoded fixup, in unslid virtual addresses.
///
/// ## Why there are two rebase variants
///
/// The original design had one, `Rebase { at }`, meaning "add the slide to
/// whatever is stored there". That is correct for the opcode encodings, where
/// the linker leaves a real unslid pointer in the slot, and *wrong* for chained
/// fixups, where the stored word is a packed chain link — a 36-bit target, an
/// 8-bit high byte, and a 12-bit offset to the next link, all sharing the eight
/// bytes. Adding a slide to that produces garbage. The decoder has already
/// unpacked the target, so it says so: [`Fixup::RebaseTo`].
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Fixup<'a> {
    /// Add the slide to the pointer already stored at `at`.
    Rebase {
        /// Unslid VA of the slot.
        at: Word,
    },
    /// Store `target + slide` at `at`, ignoring what is there now.
    RebaseTo {
        /// Unslid VA of the slot.
        at: Word,
        /// Unslid VA the slot should point at.
        target: Word,
    },
    /// Store a resolved symbol address (plus `addend`) at `at`.
    Bind {
        /// Unslid VA of the slot.
        at: Word,
        symbol: SymbolRef<'a>,
        addend: i64,
        kind: BindKind,
    },
}

impl Fixup<'_> {
    /// Unslid VA of the slot this fixup writes.
    pub fn address(&self) -> Word {
        match self {
            Fixup::Rebase { at } | Fixup::RebaseTo { at, .. } | Fixup::Bind { at, .. } => *at,
        }
    }

    pub fn is_bind(&self) -> bool {
        matches!(self, Fixup::Bind { .. })
    }
}

/// A whole-file fixup encoding.
///
/// Implementors must be **total**: `decode` may return an error on arbitrary
/// bytes but must never panic, hang, or allocate without bound. The input is a
/// third-party binary.
pub trait FixupDecoder<S: MachSpec> {
    /// Short name, for diagnostics and for [`crate::report::LinkReport`].
    fn name(&self) -> &'static str;

    /// Whether this image carries this encoding. Exactly one decoder should
    /// claim any given image; [`decode_image`] relies on it.
    fn claims(&self, obj: &MachObject<'_, S>) -> bool;

    /// Append every fixup the image declares to `out`.
    fn decode<'a>(&self, obj: &MachObject<'a, S>, out: &mut Vec<Fixup<'a>>) -> Result<()>;
}

/// The decoders compiled into this build, in the order they are consulted.
///
/// Ordering matters for exactly one image shape: a binary carrying both
/// `LC_DYLD_INFO` and `LC_DYLD_CHAINED_FIXUPS`, which a few tools produce.
/// Chained fixups win there because that is what dyld itself prefers, and
/// disagreeing with dyld about which stream is live is a way to bind half an
/// image.
// Not a `vec![]`: each element is behind its own `cfg`, and `vec![]` has
// nowhere to put the attributes.
#[allow(unused_mut, clippy::vec_init_then_push)]
pub fn default_decoders<S: MachSpec + 'static>() -> Vec<&'static dyn FixupDecoder<S>> {
    let mut v: Vec<&'static dyn FixupDecoder<S>> = Vec::new();
    #[cfg(feature = "fixups-chained")]
    v.push(&chained::ChainedFixups);
    #[cfg(feature = "fixups-dyld-info")]
    v.push(&dyld_info::DyldInfoFixups);
    #[cfg(feature = "fixups-classic")]
    v.push(&classic::ClassicFixups);
    v
}

/// Decode an image with the first decoder that claims it.
///
/// Returns the decoder's name alongside the stream, since "which encoding was
/// this image using" is a question the report has to answer and is not
/// recoverable from the normalized output.
pub fn decode_image<'a, S: MachSpec + 'static>(
    obj: &MachObject<'a, S>,
) -> Result<(&'static str, Vec<Fixup<'a>>)> {
    let mut out = Vec::new();
    for d in default_decoders::<S>() {
        if d.claims(obj) {
            d.decode(obj, &mut out)?;
            return Ok((d.name(), out));
        }
    }
    // Not an error. A statically-complete image — no imports, nothing to slide
    // — genuinely has no fixups, and failing here would reject it.
    Ok(("none", out))
}
