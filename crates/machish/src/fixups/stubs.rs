//! Recovering the `__stubs` entry that fronts each bound pointer slot.

use alloc::collections::BTreeMap;

use crate::error::Result;
use crate::image::{MachObject, SymbolTable};
use crate::raw::Word;
use crate::spec::MachSpec;
use crate::spec::constants as c;

/// Unslid slot VA to unslid stub VA, for every slot that has a stub.
#[derive(Clone, Debug, Default)]
pub struct StubMap {
    by_slot: BTreeMap<Word, Word>,
}

impl StubMap {
    /// Pair every symbol-pointer slot with its `__stubs` entry.
    ///
    /// Returns an empty map, not an error, for an image with no stubs or no
    /// `LC_DYSYMTAB`. Plenty of images legitimately have neither - a dylib that
    /// imports only data, for one - and the absence of stubs is not a defect.
    pub fn build<S: MachSpec>(obj: &MachObject<'_, S>) -> Result<Self> {
        let mut map = StubMap::default();

        let (Some(symtab), Some(dysymtab)) = (obj.symtab(), obj.dysymtab()) else {
            return Ok(map);
        };
        if dysymtab.nindirectsyms == 0 {
            return Ok(map);
        }
        let table = SymbolTable::new(*obj.view(), symtab, Some(dysymtab))?;

        // Symbol index -> stub VA. A symbol reached through two stubs keeps the
        // first, which is the one the linker's own call sites branch to.
        let mut stub_of_symbol: BTreeMap<u32, Word> = BTreeMap::new();
        for sect in &obj.sections {
            if !sect.is_symbol_stubs() || sect.reserved2 == 0 {
                continue;
            }
            let stride = u64::from(sect.reserved2);
            for i in 0..sect.entry_count(stride) {
                let Some(sym) = indirect_at(&table, sect.reserved1, i) else {
                    continue;
                };
                let va = sect.addr.wrapping_add(i * stride);
                stub_of_symbol.entry(sym).or_insert(va);
            }
        }
        if stub_of_symbol.is_empty() {
            return Ok(map);
        }

        for sect in &obj.sections {
            if !sect.is_symbol_pointers() {
                continue;
            }
            for j in 0..sect.entry_count(c::POINTER_SIZE) {
                let Some(sym) = indirect_at(&table, sect.reserved1, j) else {
                    continue;
                };
                let Some(&stub) = stub_of_symbol.get(&sym) else {
                    continue;
                };
                let slot = sect.addr.wrapping_add(j * c::POINTER_SIZE);
                map.by_slot.insert(slot, stub);
            }
        }
        Ok(map)
    }

    /// The `__stubs` entry fronting `slot_va`, if there is one.
    pub fn stub_for(&self, slot_va: Word) -> Option<Word> {
        self.by_slot.get(&slot_va).copied()
    }

    pub fn len(&self) -> usize {
        self.by_slot.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_slot.is_empty()
    }

    /// Every `(slot, stub)` pair, in slot order.
    pub fn iter(&self) -> impl Iterator<Item = (Word, Word)> + '_ {
        self.by_slot.iter().map(|(&k, &v)| (k, v))
    }
}

/// Indirect table entry `base + i`, or `None` for a sentinel or a bad index.
///
/// A `LOCAL`/`ABS` sentinel means the slot holds a local address rather than an
/// imported symbol - a rebase, not a bind - and has no stub by construction.
fn indirect_at(table: &SymbolTable<'_>, base: u32, i: u64) -> Option<u32> {
    let index = usize::try_from(u64::from(base).checked_add(i)?).ok()?;
    let raw = table.indirect(index).ok()?;
    if raw & (c::INDIRECT_SYMBOL_LOCAL | c::INDIRECT_SYMBOL_ABS) != 0 {
        return None;
    }
    Some(raw)
}
