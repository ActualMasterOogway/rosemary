use crate::error::{Error, Result};
use crate::image::commands::{Dysymtab, Symtab};
use crate::raw::{View, Word};
use crate::spec::constants::{self as c, NLIST_64_SIZE};

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Nlist {
    /// Offset into the string table. Zero means the empty name.
    pub strx: u32,
    pub n_type: u8,
    /// 1-based section number, or `NO_SECT` (0).
    pub n_sect: u8,
    pub n_desc: u16,
    /// Address for a defined symbol; for `N_INDR`, a string-table offset.
    pub n_value: Word,
}

impl Nlist {
    /// A debug entry, which carries no linking information at all.
    pub fn is_stab(&self) -> bool {
        self.n_type & c::N_STAB != 0
    }

    /// Visible outside this image.
    pub fn is_external(&self) -> bool {
        !self.is_stab() && self.n_type & c::N_EXT != 0
    }

    /// Visible to the static linker only; never exported.
    pub fn is_private_external(&self) -> bool {
        self.n_type & c::N_PEXT != 0
    }

    /// The `N_TYPE` field: `N_UNDF`, `N_ABS`, `N_INDR`, or `N_SECT`.
    pub fn symbol_type(&self) -> u8 {
        self.n_type & c::N_TYPE
    }

    pub fn is_undefined(&self) -> bool {
        self.symbol_type() == c::N_UNDF
    }

    /// Defined in this image, either in a section or as an absolute value.
    pub fn is_defined(&self) -> bool {
        matches!(self.symbol_type(), c::N_SECT | c::N_ABS)
    }

    pub fn is_absolute(&self) -> bool {
        self.symbol_type() == c::N_ABS
    }

    pub fn is_weak_definition(&self) -> bool {
        self.n_desc & c::N_WEAK_DEF != 0
    }

    /// A reference whose absence is not an error; it binds to zero instead.
    pub fn is_weak_reference(&self) -> bool {
        self.n_desc & c::N_WEAK_REF != 0
    }

    /// The 1-based dependency ordinal encoded in the high byte of `n_desc`,
    /// which is how a two-level undefined symbol names its library.
    pub fn library_ordinal(&self) -> u16 {
        self.n_desc >> 8
    }
}

/// Indexed access to an image's symbol, string, and indirect-symbol tables.
#[derive(Copy, Clone)]
pub struct SymbolTable<'a> {
    view: View<'a>,
    symtab: Symtab,
    dysymtab: Option<Dysymtab>,
}

impl<'a> SymbolTable<'a> {
    pub fn new(view: View<'a>, symtab: Symtab, dysymtab: Option<Dysymtab>) -> Result<Self> {
        let sym_bytes =
            (symtab.nsyms as usize)
                .checked_mul(NLIST_64_SIZE)
                .ok_or(Error::Truncated {
                    what: "symbol table",
                })?;
        view.slice(symtab.symoff as usize, sym_bytes, "symbol table")?;
        view.slice(
            symtab.stroff as usize,
            symtab.strsize as usize,
            "string table",
        )?;

        if let Some(dy) = dysymtab
            && dy.nindirectsyms != 0
        {
            let n = (dy.nindirectsyms as usize)
                .checked_mul(4)
                .ok_or(Error::Truncated {
                    what: "indirect symbol table",
                })?;
            view.slice(dy.indirectsymoff as usize, n, "indirect symbol table")?;
        }

        Ok(SymbolTable {
            view,
            symtab,
            dysymtab,
        })
    }

    pub fn len(&self) -> usize {
        self.symtab.nsyms as usize
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The `nlist_64` at `index`.
    pub fn nlist(&self, index: usize) -> Result<Nlist> {
        if index >= self.len() {
            return Err(Error::Truncated {
                what: "symbol index",
            });
        }
        let off = self.symtab.symoff as usize + index * NLIST_64_SIZE;
        Ok(Nlist {
            strx: self.view.u32(off)?,
            n_type: self.view.u8(off + 4)?,
            n_sect: self.view.u8(off + 5)?,
            n_desc: self.view.u16(off + 6)?,
            n_value: self.view.u64(off + 8)?,
        })
    }

    /// A string-table entry by offset, with its leading underscore intact.
    pub fn string(&self, strx: u32) -> Result<&'a str> {
        if strx as usize >= self.symtab.strsize as usize {
            return Err(Error::Truncated {
                what: "string table index",
            });
        }
        self.view.cstr(self.symtab.stroff as usize + strx as usize)
    }

    /// The name of `sym`.
    pub fn name(&self, sym: &Nlist) -> Result<&'a str> {
        self.string(sym.strx)
    }

    /// Iterate every symbol, skipping entries that fail to parse rather than
    /// failing the whole table: one malformed `strx` in a stripped system
    /// binary should not deny the caller the other ten thousand symbols.
    pub fn iter(&self) -> impl Iterator<Item = (usize, Nlist)> + '_ {
        (0..self.len()).filter_map(move |i| Some((i, self.nlist(i).ok()?)))
    }

    pub fn dysymtab(&self) -> Option<&Dysymtab> {
        self.dysymtab.as_ref()
    }

    /// Number of entries in the indirect symbol table.
    pub fn indirect_len(&self) -> usize {
        self.dysymtab.map_or(0, |d| d.nindirectsyms as usize)
    }

    /// Entry `index` of the indirect symbol table: a symbol index, or one of
    /// the `INDIRECT_SYMBOL_LOCAL`/`_ABS` sentinels meaning "this slot has no
    /// symbol; it is a rebase, not a bind".
    pub fn indirect(&self, index: usize) -> Result<u32> {
        let dy = self.dysymtab.ok_or(Error::Custom("no LC_DYSYMTAB"))?;
        if index >= dy.nindirectsyms as usize {
            return Err(Error::Truncated {
                what: "indirect symbol index",
            });
        }
        self.view.u32(dy.indirectsymoff as usize + index * 4)
    }

    /// The symbol named by indirect entry `index`, or `None` for a sentinel.
    pub fn indirect_symbol(&self, index: usize) -> Result<Option<Nlist>> {
        let raw = self.indirect(index)?;
        if raw & (c::INDIRECT_SYMBOL_LOCAL | c::INDIRECT_SYMBOL_ABS) != 0 {
            return Ok(None);
        }
        Ok(Some(self.nlist(raw as usize)?))
    }
}
