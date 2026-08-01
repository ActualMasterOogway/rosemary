//! Optional per-image passes: things a loader must *notice* but must not *do*.
//!
//! Carried over from `selfish/src/extensions/mod.rs:65-93`, with one method
//! replaced. `selfish`'s `Extension` has `handle_reloc`, letting an extension
//! claim a relocation kind the core does not implement — a hook that exists
//! because ELF relocation types are open-ended and architecture-specific.
//! Mach-O has no equivalent: there is no "unhandled fixup kind", only three
//! whole-file encodings the core already decodes completely.
//!
//! What Mach-O *does* have is reasons to intercept a decoded stream before it
//! is bound, so the hook here is [`Extension::transform_fixups`]. It takes the
//! normalized [`Fixup`] vector and may rewrite or drop entries — the mechanism
//! [`tlv::TlvExtension`] uses to make thread-local bootstrap imports claimable
//! by a shim regardless of which library the image imported them from.
//!
//! ## Why observation happens after binding
//!
//! [`Extension::observe`] runs against the **address space**, not the file, and
//! only once fixups have been applied. That is not an ordering preference; it is
//! forced by chained fixups. An initializer pointer in the file is a packed
//! chain link, not an address, so reading initializers from the file bytes gets
//! the right answer for an `LC_DYLD_INFO` image and garbage for a modern one.
//! Reading them from memory after binding is correct for both, and needs no
//! per-encoding special case.
//!
//! ## Scope
//!
//! Everything here **reports**. Initializers are collected and never called;
//! thread-local descriptors are enumerated and no per-thread block is ever
//! allocated. Running an initializer means executing guest code, which is the
//! driver's job and not the loader's — exactly the posture `selfish` takes with
//! `PT_TLS` at `selfish/src/extensions/tls.rs:16-40`.

#[cfg(feature = "ext-initializers")]
pub mod initializers;
#[cfg(feature = "ext-objc")]
pub mod objc;
#[cfg(feature = "ext-tlv")]
pub mod tlv;

use alloc::vec::Vec;

use crate::error::Result;
use crate::fixups::Fixup;
use crate::image::MachObject;
use crate::memory::AddressSpace;
use crate::raw::Word;
use crate::report::ImageReport;
use crate::spec::MachSpec;

/// An optional pass over one image.
pub trait Extension<S: MachSpec> {
    /// Short name, for diagnostics.
    fn name(&self) -> &'static str;

    /// Whether this extension applies to `obj`. Defaults to yes; an extension
    /// that keys off a section type should say so here rather than checking
    /// again in every method.
    fn claims(&self, obj: &MachObject<'_, S>) -> bool {
        let _ = obj;
        true
    }

    /// Rewrite the decoded fixup stream before it is resolved and applied.
    fn transform_fixups<'a>(
        &self,
        obj: &MachObject<'a, S>,
        fixups: &mut Vec<Fixup<'a>>,
    ) -> Result<()> {
        let _ = (obj, fixups);
        Ok(())
    }

    /// Record what the image contains, reading through `mem` post-binding.
    fn observe(
        &self,
        obj: &MachObject<'_, S>,
        mem: &dyn AddressSpace,
        slide: Word,
        report: &mut ImageReport,
    ) -> Result<()> {
        let _ = (obj, mem, slide, report);
        Ok(())
    }
}

/// The extensions applied to every image in a link, in order.
pub struct ExtensionSet<S: MachSpec> {
    extensions: Vec<alloc::boxed::Box<dyn Extension<S>>>,
}

impl<S: MachSpec> Default for ExtensionSet<S> {
    fn default() -> Self {
        ExtensionSet {
            extensions: Vec::new(),
        }
    }
}

impl<S: MachSpec> ExtensionSet<S> {
    /// An empty set: nothing is collected, nothing is transformed.
    pub fn empty() -> Self {
        Self::default()
    }

    /// The extensions enabled by this build's features.
    ///
    /// Everything here only *reports*, so enabling them by default cannot
    /// change what an image does — it can only make the [`LinkReport`] say more.
    ///
    /// [`LinkReport`]: crate::report::LinkReport
    #[allow(unused_mut, clippy::vec_init_then_push)]
    pub fn defaults() -> Self {
        let mut extensions: Vec<alloc::boxed::Box<dyn Extension<S>>> = Vec::new();
        #[cfg(feature = "ext-initializers")]
        extensions.push(alloc::boxed::Box::new(initializers::InitializersExtension));
        #[cfg(feature = "ext-tlv")]
        extensions.push(alloc::boxed::Box::new(tlv::TlvExtension::new()));
        #[cfg(feature = "ext-objc")]
        extensions.push(alloc::boxed::Box::new(objc::ObjcExtension));
        ExtensionSet { extensions }
    }

    pub fn push(&mut self, ext: alloc::boxed::Box<dyn Extension<S>>) -> &mut Self {
        self.extensions.push(ext);
        self
    }

    pub fn len(&self) -> usize {
        self.extensions.len()
    }

    pub fn is_empty(&self) -> bool {
        self.extensions.is_empty()
    }

    pub fn names(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.extensions.iter().map(|e| e.name())
    }

    pub(crate) fn transform_fixups<'a>(
        &self,
        obj: &MachObject<'a, S>,
        fixups: &mut Vec<Fixup<'a>>,
    ) -> Result<()> {
        for e in &self.extensions {
            if e.claims(obj) {
                e.transform_fixups(obj, fixups)?;
            }
        }
        Ok(())
    }

    pub(crate) fn observe(
        &self,
        obj: &MachObject<'_, S>,
        mem: &dyn AddressSpace,
        slide: Word,
        report: &mut ImageReport,
    ) -> Result<()> {
        for e in &self.extensions {
            if e.claims(obj) {
                e.observe(obj, mem, slide, report)?;
            }
        }
        Ok(())
    }
}

/// Read the pointer table a section holds, as it stands in memory.
///
/// Shared by the initializer and thread-local passes: both are "a section is an
/// array of `n` words; give me the words". Reading through the address space
/// rather than the file is what makes this correct for a chained-fixups image,
/// where the file bytes are packed chain links rather than pointers.
pub(crate) fn read_words(mem: &dyn AddressSpace, at: Word, count: usize) -> Result<Vec<Word>> {
    let mut out = Vec::with_capacity(count.min(1024));
    for i in 0..count {
        let mut buf = [0u8; 8];
        mem.read(at.wrapping_add(i as u64 * 8), &mut buf)?;
        out.push(u64::from_le_bytes(buf));
    }
    Ok(out)
}
