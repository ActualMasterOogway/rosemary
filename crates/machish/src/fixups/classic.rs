//! Classic `LC_DYSYMTAB` indirect-symbol binding, for images predating
//! `LC_DYLD_INFO`.
//!
//! There is no opcode stream and no chain here. The image says "slot *j* of
//! this symbol-pointer section binds to symbol `indirect[reserved1 + j]`", and
//! that is the whole encoding. It is the same table [`super::stubs`] already
//! reads to pair slots with stubs, walked for a different purpose.
//!
//! ## Scope: binds only
//!
//! A classic image expresses **rebases** through `LC_DYSYMTAB`'s local and
//! external relocation tables, in `relocation_info` records, and those are not
//! decoded here. The reason is a judgement about where correctness comes from
//! rather than about effort: no toolchain shipping today can produce such a
//! file — `ld64` has emitted `LC_DYLD_INFO` since Mac OS X 10.6 and chained
//! fixups since Big Sur — so a relocation decoder would be code that only ever
//! runs against inputs nobody can generate, tested only against fixtures this
//! crate wrote itself. That is a worse position than being explicit about the
//! gap.
//!
//! What that costs, concretely: a pre-10.6 non-PIE image binds correctly,
//! because a non-PIE image is loaded at its preferred base and has nothing to
//! slide. A pre-10.6 PIE image loaded at a slide would bind its imports and
//! leave its internal pointers unrebased. If one ever turns up, this is the
//! module to extend.

use alloc::vec::Vec;

use crate::error::{Error, Result};
use crate::fixups::{BindKind, Fixup, FixupDecoder, LibraryRef, SymbolRef};
use crate::image::{MachObject, Nlist, SymbolTable};
use crate::spec::MachSpec;
use crate::spec::constants as c;

/// `n_desc` library ordinal meaning "search every image", the classic
/// spelling of `BIND_SPECIAL_DYLIB_FLAT_LOOKUP`.
const DYNAMIC_LOOKUP_ORDINAL: u16 = 0xfe;
/// `n_desc` library ordinal naming the main executable.
const EXECUTABLE_ORDINAL: u16 = 0xff;

/// The classic indirect-symbol decoder.
#[derive(Copy, Clone, Debug, Default)]
pub struct ClassicFixups;

impl<S: MachSpec> FixupDecoder<S> for ClassicFixups {
    fn name(&self) -> &'static str {
        "classic"
    }

    /// Claims only images carrying *neither* modern encoding. This decoder is
    /// last in the chain anyway, but claiming narrowly means a hybrid image
    /// cannot be double-bound if that order ever changes.
    fn claims(&self, obj: &MachObject<'_, S>) -> bool {
        obj.dyld_info().is_none()
            && obj.chained_fixups().is_none()
            && obj.symtab().is_some()
            && obj.dysymtab().is_some_and(|d| d.nindirectsyms != 0)
    }

    fn decode<'a>(&self, obj: &MachObject<'a, S>, out: &mut Vec<Fixup<'a>>) -> Result<()> {
        let (Some(symtab), Some(dysymtab)) = (obj.symtab(), obj.dysymtab()) else {
            return Ok(());
        };
        let table = SymbolTable::new(*obj.view(), symtab, Some(dysymtab))?;

        for sect in &obj.sections {
            if !sect.is_symbol_pointers() {
                continue;
            }
            let lazy = sect.is_lazy_pointers();
            for j in 0..sect.entry_count(c::POINTER_SIZE) {
                let index = usize::try_from(u64::from(sect.reserved1) + j).map_err(|_| {
                    Error::Truncated {
                        what: "indirect symbol index",
                    }
                })?;
                // A `LOCAL`/`ABS` sentinel means the slot holds an address in
                // this image rather than an import. In a classic image that is
                // a relocation's job, and relocations are out of scope here —
                // so skip rather than invent a fixup for it.
                let Some(sym) = table.indirect_symbol(index)? else {
                    continue;
                };
                if !sym.is_undefined() {
                    continue;
                }
                let name = table.name(&sym)?;
                out.push(Fixup::Bind {
                    at: sect.addr.wrapping_add(j * c::POINTER_SIZE),
                    symbol: SymbolRef {
                        name,
                        library: library_ref(obj, &sym),
                        weak_import: sym.is_weak_reference(),
                    },
                    addend: 0,
                    // The section type is the only record of laziness a classic
                    // image keeps, and it is exactly the distinction the
                    // embedder needs, so preserve it rather than flattening
                    // everything to `Indirect`.
                    kind: if lazy {
                        BindKind::Lazy
                    } else {
                        BindKind::Indirect
                    },
                });
            }
        }
        Ok(())
    }
}

/// Where a classic undefined symbol says its definition lives: the high byte of
/// `n_desc`, under a two-level namespace.
fn library_ref<'a, S: MachSpec>(obj: &MachObject<'a, S>, sym: &Nlist) -> LibraryRef<'a> {
    if !obj.is_two_level() {
        return LibraryRef::Flat;
    }
    match sym.library_ordinal() {
        0 => LibraryRef::SelfImage,
        DYNAMIC_LOOKUP_ORDINAL => LibraryRef::Flat,
        EXECUTABLE_ORDINAL => LibraryRef::MainExecutable,
        n => match obj.dependency_by_ordinal(n) {
            Some(dep) => LibraryRef::InstallName(dep.install_name),
            None => LibraryRef::UnknownOrdinal(n),
        },
    }
}
