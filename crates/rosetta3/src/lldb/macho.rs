//! A Mach-O `MH_OBJECT` writer with exactly one customer: the symfile this
//! runtime hands LLDB for a translated region.
//!
//! This exists because the `object` crate cannot express the one thing the
//! whole mechanism turns on. Its Mach-O writer lays sections out itself —
//! `vmaddr: 0` on the segment, and section addresses assigned sequentially from
//! zero (`object-0.36/src/write/macho.rs`, the `section_offsets[index].address`
//! loop) — with no way for a caller to say where a section really lives. For an
//! ordinary relocatable object that is right; here it is fatal, because the
//! address of `__TEXT,__text` *is* the message. See [the module
//! note](super) for why LLDB reads it.
//!
//! What it emits is the smallest thing LLDB will parse: a header, one
//! `LC_SEGMENT_64` holding every section, and an `LC_SYMTAB`. No relocations
//! (the addresses are already final), no `LC_BUILD_VERSION`, no dyld info —
//! this object is never linked and never loaded, only read.

/// Section attributes. `S_ATTR_PURE_INSTRUCTIONS` is what makes LLDB treat the
/// section as code rather than data, which is what gets a frame symbolicated
/// instead of merely named.
pub const S_ATTR_PURE_INSTRUCTIONS: u32 = 0x8000_0000;
pub const S_ATTR_SOME_INSTRUCTIONS: u32 = 0x0000_0400;
/// `S_ATTR_DEBUG`, which is how a Mach-O section says "DWARF, not program
/// data".
pub const S_ATTR_DEBUG: u32 = 0x0200_0000;

const MH_MAGIC_64: u32 = 0xfeed_facf;
const CPU_TYPE_ARM64: u32 = 0x0100_000c;
const MH_OBJECT: u32 = 1;
const LC_SEGMENT_64: u32 = 0x19;
const LC_SYMTAB: u32 = 0x2;

const HEADER_SIZE: usize = 32;
const SEGMENT_COMMAND_SIZE: u32 = 72;
const SECTION_SIZE: u32 = 80;
const SYMTAB_COMMAND_SIZE: u32 = 24;
const NLIST_SIZE: usize = 16;

/// `N_SECT | N_EXT`: defined in one of this object's sections, and external so
/// LLDB keeps it in the symbol table it searches.
const N_SECT_EXT: u8 = 0x0f;

pub struct Section {
    pub segname: &'static str,
    pub sectname: &'static str,
    /// The section's Mach-O `addr`. For `__text` this is the live address of
    /// the compiled code; for the DWARF sections it is zero.
    pub addr: u64,
    /// Alignment as a power of two, the field's own encoding.
    pub align: u32,
    pub flags: u32,
    pub data: Vec<u8>,
}

pub struct Sym {
    /// Written verbatim. Mach-O's C-linkage underscore is the caller's to add;
    /// LLDB strips a leading one when it displays the name.
    pub name: String,
    /// 1-based index into `sections`, the `n_sect` field's own encoding.
    pub sect: u8,
    /// The symbol's address, in the same space as its section's `addr`.
    pub value: u64,
}

/// Serialize `sections` and `syms` into a Mach-O object.
///
/// Section order is preserved and it matters: LLDB places sections in file
/// order against a running high-water mark, so the section whose address must
/// survive has to come first. See [the module note](super).
pub fn write(sections: &[Section], syms: &[Sym]) -> Vec<u8> {
    let sizeofcmds =
        SEGMENT_COMMAND_SIZE + SECTION_SIZE * sections.len() as u32 + SYMTAB_COMMAND_SIZE;
    let commands_end = HEADER_SIZE + sizeofcmds as usize;

    // Section payloads, each at its own alignment.
    let mut offset = commands_end;
    let mut offsets = Vec::with_capacity(sections.len());
    for section in sections {
        let align = 1usize << section.align;
        offset = offset.next_multiple_of(align);
        offsets.push(offset as u32);
        offset += section.data.len();
    }
    let data_end = offset;

    let symoff = data_end.next_multiple_of(8);
    let stroff = symoff + syms.len() * NLIST_SIZE;

    // The string table's first byte is the empty name, by convention.
    let mut strtab = vec![0u8];
    let mut strx = Vec::with_capacity(syms.len());
    for sym in syms {
        strx.push(strtab.len() as u32);
        strtab.extend_from_slice(sym.name.as_bytes());
        strtab.push(0);
    }
    strtab.resize(strtab.len().next_multiple_of(8), 0);

    let mut out = Vec::with_capacity(stroff + strtab.len());

    // ---------------------------------------------------------- header ------

    out.extend_from_slice(&MH_MAGIC_64.to_le_bytes());
    out.extend_from_slice(&CPU_TYPE_ARM64.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes()); // CPU_SUBTYPE_ARM64_ALL
    out.extend_from_slice(&MH_OBJECT.to_le_bytes());
    out.extend_from_slice(&2u32.to_le_bytes()); // ncmds: the segment and the symtab
    out.extend_from_slice(&sizeofcmds.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes()); // flags
    out.extend_from_slice(&0u32.to_le_bytes()); // reserved

    // ------------------------------------------------- LC_SEGMENT_64 --------

    // An `MH_OBJECT` has exactly one, unnamed, segment; the section records
    // carry the segment names, and they carry the addresses too. The segment's
    // own `vmaddr` stays at the zero every relocatable object has: a reader of
    // one of these takes each section's address from the section, which is the
    // property this whole file depends on.
    let payload = (data_end - commands_end) as u64;

    out.extend_from_slice(&LC_SEGMENT_64.to_le_bytes());
    out.extend_from_slice(
        &(SEGMENT_COMMAND_SIZE + SECTION_SIZE * sections.len() as u32).to_le_bytes(),
    );
    out.extend_from_slice(&[0u8; 16]); // segname ""
    out.extend_from_slice(&0u64.to_le_bytes()); // vmaddr
    out.extend_from_slice(&payload.to_le_bytes()); // vmsize
    out.extend_from_slice(&(commands_end as u64).to_le_bytes()); // fileoff
    out.extend_from_slice(&payload.to_le_bytes()); // filesize
    out.extend_from_slice(&7i32.to_le_bytes()); // maxprot rwx
    out.extend_from_slice(&5i32.to_le_bytes()); // initprot rx
    out.extend_from_slice(&(sections.len() as u32).to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes()); // flags

    for (section, &offset) in sections.iter().zip(&offsets) {
        out.extend_from_slice(&name16(section.sectname));
        out.extend_from_slice(&name16(section.segname));
        out.extend_from_slice(&section.addr.to_le_bytes());
        out.extend_from_slice(&(section.data.len() as u64).to_le_bytes());
        out.extend_from_slice(&offset.to_le_bytes());
        out.extend_from_slice(&section.align.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes()); // reloff
        out.extend_from_slice(&0u32.to_le_bytes()); // nreloc
        out.extend_from_slice(&section.flags.to_le_bytes());
        out.extend_from_slice(&[0u8; 12]); // reserved1..3
    }

    // ----------------------------------------------------- LC_SYMTAB --------

    out.extend_from_slice(&LC_SYMTAB.to_le_bytes());
    out.extend_from_slice(&SYMTAB_COMMAND_SIZE.to_le_bytes());
    out.extend_from_slice(&(symoff as u32).to_le_bytes());
    out.extend_from_slice(&(syms.len() as u32).to_le_bytes());
    out.extend_from_slice(&(stroff as u32).to_le_bytes());
    out.extend_from_slice(&(strtab.len() as u32).to_le_bytes());

    debug_assert_eq!(out.len(), commands_end, "load commands are mis-sized");

    // ------------------------------------------------------- payload --------

    for (section, &offset) in sections.iter().zip(&offsets) {
        out.resize(offset as usize, 0);
        out.extend_from_slice(&section.data);
    }

    out.resize(symoff, 0);
    for (sym, &strx) in syms.iter().zip(&strx) {
        out.extend_from_slice(&strx.to_le_bytes());
        out.push(N_SECT_EXT);
        out.push(sym.sect);
        out.extend_from_slice(&0u16.to_le_bytes()); // n_desc
        out.extend_from_slice(&sym.value.to_le_bytes());
    }
    out.extend_from_slice(&strtab);

    out
}

/// Mach-O's fixed-width, NUL-padded, *not* necessarily NUL-terminated name
/// field. Names longer than sixteen bytes are truncated rather than rejected:
/// every name this module writes is a compile-time constant well under it.
fn name16(name: &str) -> [u8; 16] {
    let mut buf = [0u8; 16];
    let bytes = name.as_bytes();
    let len = bytes.len().min(16);
    buf[..len].copy_from_slice(&bytes[..len]);
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    fn object() -> Vec<u8> {
        write(
            &[
                Section {
                    segname: "__TEXT",
                    sectname: "__text",
                    addr: 0x1_0234_5000,
                    align: 2,
                    flags: S_ATTR_PURE_INSTRUCTIONS | S_ATTR_SOME_INSTRUCTIONS,
                    data: vec![0xc0, 0x03, 0x5f, 0xd6],
                },
                Section {
                    segname: "__DWARF",
                    sectname: "__debug_info",
                    addr: 0,
                    align: 0,
                    flags: S_ATTR_DEBUG,
                    data: vec![1, 2, 3],
                },
            ],
            &[Sym {
                name: "_guest+0x40".into(),
                sect: 1,
                value: 0x1_0234_5000,
            }],
        )
    }

    fn u32_at(bytes: &[u8], offset: usize) -> u32 {
        u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
    }

    fn u64_at(bytes: &[u8], offset: usize) -> u64 {
        u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap())
    }

    #[test]
    fn the_header_describes_an_arm64_object() {
        let bytes = object();
        assert_eq!(u32_at(&bytes, 0), MH_MAGIC_64);
        assert_eq!(u32_at(&bytes, 4), CPU_TYPE_ARM64);
        assert_eq!(u32_at(&bytes, 12), MH_OBJECT);
        assert_eq!(u32_at(&bytes, 16), 2, "ncmds");
        // Every load command is accounted for, which is what a parser walks.
        assert_eq!(
            u32_at(&bytes, 20),
            SEGMENT_COMMAND_SIZE + 2 * SECTION_SIZE + SYMTAB_COMMAND_SIZE
        );
    }

    #[test]
    fn the_text_section_keeps_the_address_it_was_given() {
        let bytes = object();
        // The whole mechanism: a section header whose `addr` is where the code
        // really is. Section records start after the segment command.
        let text = HEADER_SIZE + SEGMENT_COMMAND_SIZE as usize;
        assert_eq!(&bytes[text..text + 6], b"__text");
        assert_eq!(&bytes[text + 16..text + 22], b"__TEXT");
        assert_eq!(u64_at(&bytes, text + 32), 0x1_0234_5000);
        assert_eq!(u64_at(&bytes, text + 40), 4, "size");
        // A debug section keeps the zero it was given, which is what puts it in
        // LLDB's "place it next to the symfile" case instead of this one.
        let dwarf = text + SECTION_SIZE as usize;
        assert_eq!(u64_at(&bytes, dwarf + 32), 0, "__debug_info addr");
        // The segment stays at zero like any relocatable object's: the address
        // that matters lives on the section, not here.
        assert_eq!(u64_at(&bytes, HEADER_SIZE + 24), 0, "vmaddr");
    }

    #[test]
    fn section_payloads_land_where_their_headers_say() {
        let bytes = object();
        let text = HEADER_SIZE + SEGMENT_COMMAND_SIZE as usize;
        let dwarf = text + SECTION_SIZE as usize;

        let offset = u32_at(&bytes, text + 48) as usize;
        assert_eq!(&bytes[offset..offset + 4], &[0xc0, 0x03, 0x5f, 0xd6]);
        assert_eq!(offset % 4, 0, "honours the section's alignment");

        let offset = u32_at(&bytes, dwarf + 48) as usize;
        assert_eq!(&bytes[offset..offset + 3], &[1, 2, 3]);
        assert_eq!(u32_at(&bytes, dwarf + 64), S_ATTR_DEBUG, "flags");
    }

    #[test]
    fn the_symbol_names_the_text_section_and_its_address() {
        let bytes = object();
        let symtab = HEADER_SIZE + SEGMENT_COMMAND_SIZE as usize + 2 * SECTION_SIZE as usize;
        assert_eq!(u32_at(&bytes, symtab), LC_SYMTAB);
        let symoff = u32_at(&bytes, symtab + 8) as usize;
        assert_eq!(u32_at(&bytes, symtab + 12), 1, "nsyms");
        let stroff = u32_at(&bytes, symtab + 16) as usize;

        assert_eq!(bytes[symoff + 4], N_SECT_EXT, "n_type");
        assert_eq!(bytes[symoff + 5], 1, "n_sect is 1-based");
        assert_eq!(u64_at(&bytes, symoff + 8), 0x1_0234_5000, "n_value");

        let strx = u32_at(&bytes, symoff) as usize;
        let name = &bytes[stroff + strx..];
        let name = &name[..name.iter().position(|&b| b == 0).unwrap()];
        assert_eq!(name, b"_guest+0x40");
    }

    #[test]
    fn a_long_section_name_is_truncated_rather_than_overflowing() {
        assert_eq!(&name16("__debug_str_offsets")[..], b"__debug_str_offs");
    }
}
