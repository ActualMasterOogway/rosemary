use alloc::borrow::ToOwned;
use alloc::vec::Vec;

use crate::error::Result;
use crate::exports::{Export, ExportKind, ExportValue};
use crate::image::{MachObject, SymbolTable};
use crate::spec::MachSpec;

/// Recover an image's exports from its symbol table.
///
/// Returns an empty list for an image with no `LC_SYMTAB`.
pub fn parse_nlist<S: MachSpec>(obj: &MachObject<'_, S>) -> Result<Vec<Export>> {
    let Some(symtab) = obj.symtab() else {
        return Ok(Vec::new());
    };
    let table = SymbolTable::new(*obj.view(), symtab, obj.dysymtab())?;

    let mut out = Vec::new();
    for (_, sym) in table.iter() {
        // `N_PEXT` is a trap here, since a private-external symbol is `N_EXT` in
        // the object file so the static linker can see it, and is not an export.
        // Treating one as exported publishes a symbol dyld would never have offered.
        if !sym.is_external() || sym.is_private_external() || !sym.is_defined() {
            continue;
        }
        let Ok(name) = table.name(&sym) else { continue };
        if name.is_empty() {
            continue;
        }

        let kind = if sym.is_absolute() {
            ExportKind::Absolute
        } else if obj
            .section_at(sym.n_value)
            .is_some_and(|s| s.is_thread_local())
        {
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
