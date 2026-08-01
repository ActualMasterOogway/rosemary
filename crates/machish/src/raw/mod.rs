//! Byte-level primitives: a bounds-checked cursor over the file, plus the
//! LEB128 decoding that Mach-O's three bytecode formats are built on.
//!
//! This layer intentionally differs from `selfish`'s `raw`. There, the
//! primitives are `ElfClass` (abstracting ELF32-vs-ELF64 field offsets) and
//! `Endian` (abstracting LSB-vs-MSB), because ELF genuinely ships in all four
//! combinations. Mach-O as we care about it is always `MH_MAGIC_64`,
//! little-endian, so a sixty-constant offset trait with a single implementation
//! would be ceremony. What Mach-O needs and ELF does not is LEB128: the
//! rebase/bind opcode streams, the chained-fixups imports, and the export trie
//! all encode their integers that way.

mod leb;
mod view;

pub use leb::{Cursor, sleb128, uleb128};
pub use view::View;

/// The universal address/offset type. Mach-O is 64-bit here by construction.
pub type Word = u64;
