mod nlist;
mod trie;

pub use nlist::parse_nlist;
pub use trie::parse_trie;

use alloc::string::String;
use alloc::vec::Vec;

use crate::error::Result;
use crate::image::MachObject;
use crate::raw::Word;
use crate::spec::MachSpec;
use crate::spec::constants as c;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ExportKind {
    /// An ordinary function or data address.
    Regular,
    /// A thread-local variable; the address is of its `tlv_descriptor`, not of
    /// any thread's copy.
    ThreadLocal,
    /// An absolute value that must not be slid.
    Absolute,
}

/// Where an export's value comes from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExportValue {
    /// A definition in this image, at [`Export::vmaddr`].
    Direct,
    /// This image re-exports another's symbol. Resolution continues in the
    /// dependency at `ordinal`, under `imported_name` if the name was changed.
    ///
    /// There is no address: a re-export is a forwarding edge, and treating one
    /// as a definition at address zero is the classic way to bind a caller to
    /// nothing.
    Reexport {
        /// 1-based dependency ordinal, as in [`crate::image::Dependency`].
        ordinal: u16,
        /// The name to look for in that dependency, when it differs.
        imported_name: Option<String>,
    },
    /// The symbol resolves through a resolver function chosen at first call.
    /// [`Export::vmaddr`] is the stub; `resolver` is the chooser.
    StubAndResolver {
        /// Unslid VA of the resolver.
        resolver: Word,
    },
}

/// One exported symbol.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Export {
    /// The symbol name **with** its leading underscore, exactly as an import
    /// will ask for it. Stripping it here would force every consumer to put it
    /// back.
    pub name: String,
    /// Raw flags word, kept so a consumer can see bits this crate does not yet
    /// interpret.
    pub flags: u32,
    pub kind: ExportKind,
    /// Unslid virtual address of the definition. Meaningless (and zero) for
    /// [`ExportValue::Reexport`].
    pub vmaddr: Word,
    pub value: ExportValue,
}

impl Export {
    /// Whether another image's definition of this name may legitimately win.
    pub fn is_weak(&self) -> bool {
        self.flags & c::EXPORT_SYMBOL_FLAGS_WEAK_DEFINITION != 0
    }

    pub fn is_reexport(&self) -> bool {
        matches!(self.value, ExportValue::Reexport { .. })
    }

    /// The address this export resolves to once the image is slid.
    ///
    /// `None` for a re-export, which has no address of its own, and for an
    /// absolute symbol the slide is deliberately not applied.
    pub fn address(&self, slide: Word) -> Option<Word> {
        match self.value {
            ExportValue::Reexport { .. } => None,
            _ if self.kind == ExportKind::Absolute => Some(self.vmaddr),
            _ => Some(self.vmaddr.wrapping_add(slide)),
        }
    }
}

/// Every symbol an image offers, sorted by name.
///
/// Sorted rather than hashed because the core is `no_std` without a default
/// hasher, and because bind streams ask for names in roughly emission order,
/// where a binary search over a contiguous slice beats a map's pointer chasing.
#[derive(Clone, Debug, Default)]
pub struct ExportTable {
    entries: Vec<Export>,
}

impl ExportTable {
    /// Build a table from an already-parsed list, sorting and de-duplicating.
    pub fn from_entries(mut entries: Vec<Export>) -> Self {
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        entries.dedup_by(|a, b| a.name == b.name);
        ExportTable { entries }
    }

    /// Parse an image's exports from whichever source it carries.
    ///
    /// Prefers the trie: it is what dyld itself reads, and it is the only form
    /// that records re-export edges. Falls back to `LC_SYMTAB` only when no
    /// trie is present at all, which in practice means a very old image or one
    /// a stripping tool has been unkind to.
    pub fn parse<S: MachSpec>(obj: &MachObject<'_, S>) -> Result<Self> {
        let base = obj.preferred_base()?;

        let trie_range = obj
            .exports_trie()
            .and_then(|d| d.range())
            .or_else(|| obj.dyld_info().and_then(|d| exports_range(&d)));

        if let Some(range) = trie_range
            && let Some(bytes) = obj.bytes().get(range)
        {
            return Ok(Self::from_entries(parse_trie(bytes, base)?));
        }
        Ok(Self::from_entries(parse_nlist(obj)?))
    }

    /// Parse an image's exports from `LC_SYMTAB`, ignoring any trie.
    ///
    /// Exposed on its own so the two paths can be compared against each other,
    /// which is how the trie walker is checked without a second implementation
    /// to trust.
    pub fn parse_symtab<S: MachSpec>(obj: &MachObject<'_, S>) -> Result<Self> {
        Ok(Self::from_entries(parse_nlist(obj)?))
    }

    /// Look up an export by its full name, leading underscore included.
    pub fn get(&self, name: &str) -> Option<&Export> {
        let i = self
            .entries
            .binary_search_by(|e| e.name.as_str().cmp(name))
            .ok()?;
        Some(&self.entries[i])
    }

    pub fn contains(&self, name: &str) -> bool {
        self.get(name).is_some()
    }

    pub fn entries(&self) -> &[Export] {
        &self.entries
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn iter(&self) -> core::slice::Iter<'_, Export> {
        self.entries.iter()
    }
}

impl<'t> IntoIterator for &'t ExportTable {
    type Item = &'t Export;
    type IntoIter = core::slice::Iter<'t, Export>;

    fn into_iter(self) -> Self::IntoIter {
        self.entries.iter()
    }
}

/// The trie extent recorded inside `LC_DYLD_INFO`, which predates
/// `LC_DYLD_EXPORTS_TRIE` and stores the same bytes under different fields.
fn exports_range(info: &crate::image::DyldInfo) -> Option<core::ops::Range<usize>> {
    if info.export_size == 0 {
        return None;
    }
    let start = info.export_off as usize;
    Some(start..start + info.export_size as usize)
}
