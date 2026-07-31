//! `section_64` records and the `S_*` type classification.
//!
//! ELF loading never reads the section table — `selfish` parses program headers
//! and the dynamic array only. Mach-O loading cannot avoid it: initializers,
//! thread-local storage, symbol stubs, and the lazy/non-lazy pointer tables are
//! all identified by section *type*, and the stub table in particular is what
//! makes call-site interception possible at all.

use crate::error::Result;
use crate::raw::{View, Word};
use crate::spec::constants::{self, SECTION_TYPE};

/// One `section_64` record.
#[derive(Copy, Clone, Debug)]
pub struct Section<'a> {
    pub seg_name: &'a str,
    pub sect_name: &'a str,
    /// Virtual address, before any slide.
    pub addr: Word,
    pub size: Word,
    /// File offset. Meaningless for zero-fill sections.
    pub offset: u32,
    /// Alignment as a power of two.
    pub align: u32,
    pub reloff: u32,
    pub nreloc: u32,
    pub flags: u32,
    /// For stub and symbol-pointer sections, the starting index into the
    /// indirect symbol table.
    pub reserved1: u32,
    /// For `S_SYMBOL_STUBS`, the size in bytes of each stub (6 on x86-64).
    pub reserved2: u32,
}

impl<'a> Section<'a> {
    /// Parse a `section_64` at `off`.
    pub fn parse(view: &View<'a>, off: usize) -> Result<Self> {
        Ok(Section {
            sect_name: view.fixed_str(off, 16)?,
            seg_name: view.fixed_str(off + 16, 16)?,
            addr: view.u64(off + 32)?,
            size: view.u64(off + 40)?,
            offset: view.u32(off + 48)?,
            align: view.u32(off + 52)?,
            reloff: view.u32(off + 56)?,
            nreloc: view.u32(off + 60)?,
            flags: view.u32(off + 64)?,
            reserved1: view.u32(off + 68)?,
            reserved2: view.u32(off + 72)?,
        })
    }

    /// The `S_*` type, with the attribute bits masked off.
    pub fn section_type(&self) -> u8 {
        (self.flags & SECTION_TYPE) as u8
    }

    /// Whether this section occupies no file bytes and maps as zeroes.
    pub fn is_zerofill(&self) -> bool {
        matches!(
            self.section_type(),
            constants::S_ZEROFILL | constants::S_THREAD_LOCAL_ZEROFILL
        )
    }

    /// `__stubs`: fixed-size trampolines, `reserved2` bytes each, that jump
    /// through the corresponding lazy pointer.
    pub fn is_symbol_stubs(&self) -> bool {
        self.section_type() == constants::S_SYMBOL_STUBS
    }

    /// `__la_symbol_ptr`: slots dyld would bind on first call. We bind eagerly.
    pub fn is_lazy_pointers(&self) -> bool {
        self.section_type() == constants::S_LAZY_SYMBOL_POINTERS
    }

    /// `__got` / `__nl_symbol_ptr`: slots bound at load time.
    pub fn is_non_lazy_pointers(&self) -> bool {
        self.section_type() == constants::S_NON_LAZY_SYMBOL_POINTERS
    }

    /// Either flavour of symbol pointer table.
    pub fn is_symbol_pointers(&self) -> bool {
        self.is_lazy_pointers() || self.is_non_lazy_pointers()
    }

    /// `__mod_init_func`: an array of function pointers to run before `main`.
    pub fn is_mod_init(&self) -> bool {
        self.section_type() == constants::S_MOD_INIT_FUNC_POINTERS
    }

    /// `__mod_term_func`: destructors, run at unload.
    pub fn is_mod_term(&self) -> bool {
        self.section_type() == constants::S_MOD_TERM_FUNC_POINTERS
    }

    /// Any of the thread-local storage section types.
    pub fn is_thread_local(&self) -> bool {
        matches!(
            self.section_type(),
            constants::S_THREAD_LOCAL_REGULAR
                | constants::S_THREAD_LOCAL_ZEROFILL
                | constants::S_THREAD_LOCAL_VARIABLES
                | constants::S_THREAD_LOCAL_VARIABLE_POINTERS
                | constants::S_THREAD_LOCAL_INIT_FUNCTION_POINTERS
        )
    }

    /// `__thread_vars`: the array of `tlv_descriptor`s.
    pub fn is_thread_local_variables(&self) -> bool {
        self.section_type() == constants::S_THREAD_LOCAL_VARIABLES
    }

    /// How many entries a fixed-stride table holds.
    pub fn entry_count(&self, stride: u64) -> u64 {
        self.size.checked_div(stride).unwrap_or(0)
    }

    /// Whether `addr` falls inside this section.
    pub fn contains(&self, addr: Word) -> bool {
        addr >= self.addr && addr < self.addr.wrapping_add(self.size)
    }
}
