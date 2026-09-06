//! `__DATA,__interpose`: an image that replaces a function everyone else calls.
//!
//! An image built with `DYLD_INTERPOSE` - or with clang's
//! `__attribute__((used)) static const struct { const void *r; const void *o; }`
//! idiom that expands to the same thing - carries a section of pairs:
//! `{ replacement, replacee }`, sixteen bytes each. dyld reads it and rewrites
//! every binding, in every other image, whose target is `replacee` to point at
//! `replacement` instead. That is how a program drops in a different allocator
//! without any of its libraries being rebuilt.
//!
//! As an example, `mimalloc` uses this feature to ensure that memory allocator implementations
//! do not conflict.
//!
//! The section is decoded here and the rewrite belongs to whoever owns the bindings. An interposed binding
//! stops resolving to a host function pointer and starts naming guest code, so applying one changes how the
//! driver must treat every call site that reaches it, therefore it is best to let the driver decide
//! how to handle symbol interposition.

use crate::error::Result;
use crate::extensions::{Extension, read_words};
use crate::image::MachObject;
use crate::memory::AddressSpace;
use crate::raw::Word;
use crate::report::{ImageReport, Interposition};
use crate::spec::MachSpec;

#[derive(Copy, Clone, Debug, Default)]
pub struct InterposeExtension;

impl<S: MachSpec> Extension<S> for InterposeExtension {
    fn name(&self) -> &'static str {
        "interpose"
    }

    fn claims(&self, obj: &MachObject<'_, S>) -> bool {
        obj.sections.iter().any(|s| s.is_interpose())
    }

    fn observe(
        &self,
        obj: &MachObject<'_, S>,
        mem: &dyn AddressSpace,
        slide: Word,
        report: &mut ImageReport,
    ) -> Result<()> {
        report.interpositions = collect_interpositions(obj, mem, slide)?;
        Ok(())
    }
}

/// Every `{ replacement, replacee }` pair the image declares, as it stands in
/// memory after binding.
pub fn collect_interpositions<S: MachSpec>(
    obj: &MachObject<'_, S>,
    mem: &dyn AddressSpace,
    slide: Word,
) -> Result<alloc::vec::Vec<Interposition>> {
    let mut out = alloc::vec::Vec::new();
    for sect in obj.sections.iter().filter(|s| s.is_interpose()) {
        // Two pointers per entry. A trailing partial entry is a malformed
        // section rather than a short last pair, so the count truncates.
        let count = usize::try_from(sect.size / 16).unwrap_or(0);
        let words = read_words(mem, sect.addr.wrapping_add(slide), count * 2)?;
        #[allow(clippy::chunks_exact_to_as_chunks)]
        for pair in words.chunks_exact(2) {
            let (replacement, replacee) = (pair[0], pair[1]);
            if replacement == 0 || replacee == 0 {
                continue;
            }
            out.push(Interposition {
                replacement,
                replacee,
            });
        }
    }
    Ok(out)
}
