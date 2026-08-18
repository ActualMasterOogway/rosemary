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

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum BindKind {
    /// `__got` / `__nl_symbol_ptr`: bound before the image runs.
    NonLazy,
    /// `__la_symbol_ptr`: dyld would bind on first call. We bind eagerly,
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
    /// The name with its leading underscore, exactly as the image spells
    /// it. Kept intact so it round-trips against [`crate::exports::Export`];
    /// only the `dlsym` provider strips it, at the last possible moment,
    /// because that is the one place the underscore is wrong.
    pub name: &'a str,
    pub library: LibraryRef<'a>,
    /// Absence is not an error; the slot binds to zero instead.
    pub weak_import: bool,
}

/// One decoded fixup, in unslid virtual addresses.
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

pub trait FixupDecoder<S: MachSpec> {
    /// Short name, for diagnostics and for [`crate::report::LinkReport`].
    fn name(&self) -> &'static str;

    /// Whether this image carries this encoding. Exactly one decoder should
    /// claim any given image; [`decode_image`] relies on it.
    fn claims(&self, obj: &MachObject<'_, S>) -> bool;

    /// Append every fixup the image declares to `out`.
    fn decode<'a>(&self, obj: &MachObject<'a, S>, out: &mut Vec<Fixup<'a>>) -> Result<()>;
}

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
    // Statically complete image..?
    Ok(("none", out))
}
