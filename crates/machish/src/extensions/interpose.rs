//! `__DATA,__interpose`: an image that replaces a function everyone else calls.
//!
//! An image built with `DYLD_INTERPOSE` — or with clang's
//! `__attribute__((used)) static const struct { const void *r; const void *o; }`
//! idiom that expands to the same thing — carries a section of **pairs**:
//! `{ replacement, replacee }`, sixteen bytes each. dyld reads it and rewrites
//! every binding, in every *other* image, whose target is `replacee` to point at
//! `replacement` instead. That is how a program drops in a different allocator
//! without any of its libraries being rebuilt.
//!
//! This is not a curiosity. Roblox ships mimalloc as an interposing dylib: the
//! program calls `mi_malloc` directly for its own allocations and then frees
//! them with plain `free`, which is only correct because `free` *is* `mi_free`
//! by the time anything runs. A loader that ignores the section links the
//! program successfully, runs it for a minute, and aborts inside the system
//! allocator on a pointer it never handed out.
//!
//! ## Collected, not applied
//!
//! Same posture as [`initializers`](super::initializers): the section is
//! *decoded* here and the rewrite belongs to whoever owns the bindings. In this
//! workspace that is `rosetta3`, and the reason is specific to it rather than
//! stylistic — an interposed binding stops resolving to a host function pointer
//! and starts naming guest code, so applying one changes how the recompiler must
//! treat every call site that reaches it. Only the driver knows that.
//!
//! ## Why it reads from memory
//!
//! Both halves of a pair are fixups. The replacement is a rebase (the
//! interposer's own function) and the replacee is a **bind** — `_free` imported
//! from `libSystem` — so in the file bytes they are a chain link and a symbol
//! index, and in memory, after binding, they are the two addresses the rewrite
//! is stated in terms of. Reading the file would get the right answer for
//! neither.

use crate::error::Result;
use crate::extensions::{Extension, read_words};
use crate::image::MachObject;
use crate::memory::AddressSpace;
use crate::raw::Word;
use crate::report::{ImageReport, Interposition};
use crate::spec::MachSpec;

/// Collects `__interpose` tuples into the report.
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
///
/// A pair with either half zero is dropped. That is not defensiveness: the
/// replacee is a bind, and a *weak* bind nothing satisfied leaves a zero — which
/// would otherwise mean "rewrite every binding that resolved to nothing", i.e.
/// every unsatisfied weak import in the program, to this replacement.
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
