//! The `LC_SYMTAB` fallback for images with no export trie.
//!
//! What this path can and cannot recover is worth being precise about, because
//! the difference is not a limitation of the implementation:
//!
//! - **Names and addresses**: fully. An `N_EXT | N_SECT` entry is exactly a
//!   default-visibility definition, and `n_value` is its unslid VA.
//! - **Weak definitions**: fully, from `N_WEAK_DEF` in `n_desc`.
//! - **Thread-local kind**: recovered from the section the symbol lives in,
//!   since `nlist` has no kind field of its own.
//! - **Re-exports**: *not at all*. The classic symbol table has nowhere to
//!   record "this name lives in another library", so an image whose exports are
//!   mostly re-exports (every system umbrella) degrades to near-nothing here.
//!   That asymmetry is the reason the trie is always preferred rather than
//!   merely usual.

use alloc::borrow::ToOwned;
use alloc::vec::Vec;

use crate::error::Result;
use crate::exports::{Export, ExportKind, ExportValue};
use crate::image::{MachObject, SymbolTable};
use crate::spec::MachSpec;

/// Recover an image's exports from its symbol table.
///
/// Returns an empty list, not an error, for an image with no `LC_SYMTAB`: a
/// stripped image genuinely exports nothing this path can see, and that is a
/// fact about the image rather than a parse failure.
pub fn parse_nlist<S: MachSpec>(obj: &MachObject<'_, S>) -> Result<Vec<Export>> {
    let Some(symtab) = obj.symtab() else { return Ok(Vec::new()) };
    let table = SymbolTable::new(*obj.view(), symtab, obj.dysymtab())?;

    let mut out = Vec::new();
    for (_, sym) in table.iter() {
        // `N_PEXT` is the trap here: a private-external symbol is `N_EXT` in
        // the object file so the static linker can see it, and is emphatically
        // not an export. Treating one as exported publishes a symbol dyld would
        // never have offered.
        if !sym.is_external() || sym.is_private_external() || !sym.is_defined() {
            continue;
        }
        let Ok(name) = table.name(&sym) else { continue };
        if name.is_empty() {
            continue;
        }

        let kind = if sym.is_absolute() {
            ExportKind::Absolute
        } else if obj.section_at(sym.n_value).is_some_and(|s| s.is_thread_local()) {
            ExportKind::ThreadLocal
        } else {
            ExportKind::Regular
        };

        let flags = if sym.is_weak_definition() {
            crate::spec::constants::EXPORT_SYMBOL_FLAGS_WEAK_DEFINITION
        } else {
            0
        } | match kind {
            ExportKind::Regular => crate::spec::constants::EXPORT_SYMBOL_FLAGS_KIND_REGULAR,
            ExportKind::ThreadLocal => {
                crate::spec::constants::EXPORT_SYMBOL_FLAGS_KIND_THREAD_LOCAL
            }
            ExportKind::Absolute => crate::spec::constants::EXPORT_SYMBOL_FLAGS_KIND_ABSOLUTE,
        };

        out.push(Export {
            name: name.to_owned(),
            flags,
            kind,
            vmaddr: sym.n_value,
            value: ExportValue::Direct,
        });
    }
    Ok(out)
}
