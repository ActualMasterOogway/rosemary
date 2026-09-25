#![allow(dead_code)]

use std::collections::HashMap;

use super::object::{Kind, Section, Sym};

const SHT_PROGBITS: u32 = 1;
const SHT_SYMTAB: u32 = 2;
const SHT_STRTAB: u32 = 3;
const SHT_NOBITS: u32 = 8;

const SHF_WRITE: u64 = 0x1;
const SHF_ALLOC: u64 = 0x2;
const SHF_EXECINSTR: u64 = 0x4;

const ET_REL: u16 = 1;
#[cfg(target_arch = "aarch64")]
const EM_MACHINE: u16 = 183; // EM_AARCH64
#[cfg(not(target_arch = "aarch64"))]
const EM_MACHINE: u16 = 62; // EM_X86_64

const EHDR_SIZE: usize = 64;
const SHDR_SIZE: usize = 64;
const SYM_SIZE: usize = 24;

/// `STB_GLOBAL` in the high nibble, so LLDB keeps the symbol in the table it
/// searches, over `STT_FUNC` or `STT_OBJECT`.
const GLOBAL_FUNC: u8 = (1 << 4) | 2;
const GLOBAL_OBJECT: u8 = (1 << 4) | 1;

/// The section name this format gives a [`Kind`].
///
/// Deliberately the plain names and not the caller's own: LLDB's ELF reader
/// types a section by matching these, and a `.text` under any other spelling is
/// not disassembled as code. Two sections may end up sharing a name, which ELF
/// permits - what distinguishes them is their address, and symbols reach them
/// by index.
fn name(kind: Kind) -> String {
    match kind {
        Kind::Code => ".text".to_string(),
        Kind::Data => ".data".to_string(),
        Kind::Debug(name) => format!(".debug_{name}"),
    }
}

fn flags(kind: Kind) -> u64 {
    match kind {
        Kind::Code => SHF_ALLOC | SHF_EXECINSTR,
        Kind::Data => SHF_ALLOC | SHF_WRITE,
        Kind::Debug(_) => 0,
    }
}

/// A string table under construction, which hands back the offset of each name
/// it is given.
///
/// Append-only, and deliberately: a real image brings hundreds of thousands of
/// symbols, and searching what is already written for each new name is
/// quadratic in the size of the table. What repeats is section names, and
/// [`write`] shares those itself - there are a handful of them.
struct Strings {
    bytes: Vec<u8>,
}

impl Strings {
    /// Both of ELF's string tables start with the empty name at offset zero.
    fn new() -> Self {
        Strings { bytes: vec![0] }
    }

    fn add(&mut self, name: &str) -> u32 {
        let at = self.bytes.len() as u32;
        self.bytes.extend_from_slice(name.as_bytes());
        self.bytes.push(0);
        at
    }
}

/// One section header, written in ELF64's field order.
struct Header {
    name: u32,
    kind: u32,
    flags: u64,
    addr: u64,
    offset: u64,
    size: u64,
    link: u32,
    info: u32,
    align: u64,
    entsize: u64,
}

impl Header {
    fn write(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.name.to_le_bytes());
        out.extend_from_slice(&self.kind.to_le_bytes());
        out.extend_from_slice(&self.flags.to_le_bytes());
        out.extend_from_slice(&self.addr.to_le_bytes());
        out.extend_from_slice(&self.offset.to_le_bytes());
        out.extend_from_slice(&self.size.to_le_bytes());
        out.extend_from_slice(&self.link.to_le_bytes());
        out.extend_from_slice(&self.info.to_le_bytes());
        out.extend_from_slice(&self.align.to_le_bytes());
        out.extend_from_slice(&self.entsize.to_le_bytes());
    }
}

/// Serialize `sections` and `syms` into an ELF object.
///
/// The section header table is the caller's sections in order, so a [`Sym`]'s
/// 1-based index is its `st_shndx` unchanged: index zero is ELF's own reserved
/// null section.
pub fn write(sections: &[Section], syms: &[Sym]) -> Vec<u8> {
    let mut shstr = Strings::new();
    // Every executable segment of an image is a `.text`, so the section names
    // are written once and shared. Few enough that a lookup is free.
    let mut shared: HashMap<String, u32> = HashMap::new();
    let mut names = Vec::with_capacity(sections.len());
    for section in sections {
        let text = name(section.kind);
        let at = match shared.get(&text) {
            Some(&at) => at,
            None => {
                let at = shstr.add(&text);
                shared.insert(text, at);
                at
            }
        };
        names.push(at);
    }
    let shstrtab_name = shstr.add(".shstrtab");
    let symtab_name = shstr.add(".symtab");
    let strtab_name = shstr.add(".strtab");

    // The null symbol ELF reserves index zero for, then the caller's.
    let mut strtab = Strings::new();
    let mut symbols = vec![0u8; SYM_SIZE];
    for sym in syms {
        let at = strtab.add(&sym.name);
        let section = sections.get(sym.sect.wrapping_sub(1) as usize);
        symbols.extend_from_slice(&at.to_le_bytes());
        symbols.push(match section.is_some_and(|s| s.kind.executable()) {
            true => GLOBAL_FUNC,
            false => GLOBAL_OBJECT,
        });
        symbols.push(0); // st_other
        symbols.extend_from_slice(&u16::from(sym.sect).to_le_bytes());
        // In a relocatable object a symbol's value is an offset into its own
        // section, and a reader adds the section's address back. Handing it the
        // address the caller gave would place the symbol at twice it.
        let value = sym
            .value
            .wrapping_sub(section.map_or(0, |section| section.addr));
        symbols.extend_from_slice(&value.to_le_bytes());
        // No extent: a symbol here names where something starts, and LLDB takes
        // the end from the next one.
        symbols.extend_from_slice(&0u64.to_le_bytes());
    }

    // Room for the header, which is filled in at the end: it carries offsets
    // into everything below it.
    let mut out: Vec<u8> = vec![0; EHDR_SIZE];

    // Section payloads, each at its own alignment.
    let mut offsets = Vec::with_capacity(sections.len());
    for section in sections {
        if section.data.is_empty() {
            // Nothing in the file: either a `SHT_NOBITS` mapping the debugger
            // reads out of the process, or an empty section.
            offsets.push(0u64);
            continue;
        }
        let align = 1usize << section.align;
        out.resize(out.len().next_multiple_of(align), 0);
        offsets.push(out.len() as u64);
        out.extend_from_slice(&section.data);
    }

    let shstroff = out.len() as u64;
    out.extend_from_slice(&shstr.bytes);
    let stroff = out.len() as u64;
    out.extend_from_slice(&strtab.bytes);

    out.resize(out.len().next_multiple_of(8), 0);
    let symoff = out.len() as u64;
    out.extend_from_slice(&symbols);

    out.resize(out.len().next_multiple_of(8), 0);
    let shoff = out.len() as u64;

    Header {
        name: 0,
        kind: 0, // SHT_NULL
        flags: 0,
        addr: 0,
        offset: 0,
        size: 0,
        link: 0,
        info: 0,
        align: 0,
        entsize: 0,
    }
    .write(&mut out);

    for ((section, &name), &offset) in sections.iter().zip(&names).zip(&offsets) {
        Header {
            name,
            kind: match section.data.is_empty() && section.size != 0 {
                true => SHT_NOBITS,
                false => SHT_PROGBITS,
            },
            flags: flags(section.kind),
            addr: section.addr,
            offset,
            size: section.size,
            link: 0,
            info: 0,
            align: 1 << section.align,
            entsize: 0,
        }
        .write(&mut out);
    }

    let shstrtab_index = sections.len() as u32 + 1;
    for (name, kind, offset, size, link, entsize) in [
        (
            shstrtab_name,
            SHT_STRTAB,
            shstroff,
            shstr.bytes.len() as u64,
            0,
            0,
        ),
        (
            symtab_name,
            SHT_SYMTAB,
            symoff,
            symbols.len() as u64,
            // The string table the symbol names are in, and how many of the
            // entries are local: only the null symbol is.
            shstrtab_index + 2,
            SYM_SIZE as u64,
        ),
        (
            strtab_name,
            SHT_STRTAB,
            stroff,
            strtab.bytes.len() as u64,
            0,
            0,
        ),
    ] {
        Header {
            name,
            kind,
            flags: 0,
            addr: 0,
            offset,
            size,
            link,
            info: match kind {
                SHT_SYMTAB => 1,
                _ => 0,
            },
            align: 1,
            entsize,
        }
        .write(&mut out);
    }

    let shnum = sections.len() as u16 + 4;

    let header = &mut out[..EHDR_SIZE];
    header[..4].copy_from_slice(b"\x7fELF");
    header[4] = 2; // ELFCLASS64
    header[5] = 1; // ELFDATA2LSB
    header[6] = 1; // EV_CURRENT
    header[16..18].copy_from_slice(&ET_REL.to_le_bytes());
    header[18..20].copy_from_slice(&EM_MACHINE.to_le_bytes());
    header[20..24].copy_from_slice(&1u32.to_le_bytes()); // e_version
    header[40..48].copy_from_slice(&shoff.to_le_bytes());
    header[52..54].copy_from_slice(&(EHDR_SIZE as u16).to_le_bytes());
    header[58..60].copy_from_slice(&(SHDR_SIZE as u16).to_le_bytes());
    header[60..62].copy_from_slice(&shnum.to_le_bytes());
    header[62..64].copy_from_slice(&(shstrtab_index as u16).to_le_bytes());

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn object() -> Vec<u8> {
        write(
            &[
                Section::resident(Kind::Code, 0x1_0234_5000, 2, vec![0xc3, 0x90, 0x90, 0x90]),
                Section::mapped(Kind::Data, "__DATA_CONST", 0x1_0300_0000, 12, 0x4000),
                Section::resident(Kind::Debug("info"), 0, 0, vec![1, 2, 3]),
            ],
            &[Sym {
                name: "guest+0x40".into(),
                sect: 1,
                value: 0x1_0234_5040,
            }],
        )
    }

    fn u16_at(bytes: &[u8], offset: usize) -> u16 {
        u16::from_le_bytes(bytes[offset..offset + 2].try_into().unwrap())
    }

    fn u32_at(bytes: &[u8], offset: usize) -> u32 {
        u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
    }

    fn u64_at(bytes: &[u8], offset: usize) -> u64 {
        u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap())
    }

    /// The `index`th section header.
    fn shdr(bytes: &[u8], index: usize) -> &[u8] {
        let shoff = u64_at(bytes, 40) as usize;
        &bytes[shoff + index * SHDR_SIZE..][..SHDR_SIZE]
    }

    /// A NUL-terminated name out of a string table.
    fn string(bytes: &[u8], table: usize, at: usize) -> &str {
        let rest = &bytes[table + at..];
        let end = rest.iter().position(|&b| b == 0).unwrap();
        std::str::from_utf8(&rest[..end]).unwrap()
    }

    fn shstrtab(bytes: &[u8]) -> usize {
        u64_at(shdr(bytes, u16_at(bytes, 62) as usize), 24) as usize
    }

    /// A symbol table the size a real image brings, written in one pass.
    ///
    /// The bound is three orders of magnitude looser than the work takes,
    /// because what it catches is not a slow machine: a writer that searches
    /// the table for each name it is handed spends twelve seconds on a
    /// *twentieth* of these, and a large example app never finished loading.
    #[test]
    fn a_large_symbol_table_is_written_in_one_pass() {
        let sections = vec![Section::mapped(
            Kind::Code,
            "__TEXT",
            0x1_0000_0000,
            12,
            0x400_0000,
        )];
        let syms: Vec<Sym> = (0..200_000u64)
            .map(|i| Sym {
                name: format!("ExampleApp+{:#x}", i * 16),
                sect: 1,
                value: 0x1_0000_0000 + i * 16,
            })
            .collect();

        let start = std::time::Instant::now();
        let bytes = write(&sections, &syms);
        let elapsed = start.elapsed();

        // The null section, the one above, and the section-name table come first.
        let symtab = shdr(&bytes, sections.len() + 2);
        assert_eq!(
            u64_at(symtab, 32) as usize / SYM_SIZE,
            syms.len() + 1,
            "every symbol, and the null one"
        );
        assert!(
            elapsed < std::time::Duration::from_secs(20),
            "writing {} symbols took {elapsed:?}",
            syms.len()
        );
    }

    #[test]
    fn the_header_describes_an_object_for_this_host() {
        let bytes = object();
        assert_eq!(&bytes[..4], b"\x7fELF");
        assert_eq!(bytes[4], 2, "ELFCLASS64");
        assert_eq!(u16_at(&bytes, 16), ET_REL);
        assert_eq!(u16_at(&bytes, 18), EM_MACHINE);
        assert_eq!(u16_at(&bytes, 58) as usize, SHDR_SIZE);
        // Three sections, plus the null one and the three tables.
        assert_eq!(u16_at(&bytes, 60), 7, "e_shnum");
    }

    #[test]
    fn the_text_section_keeps_the_address_it_was_given() {
        let bytes = object();
        // The whole mechanism: an allocated section header whose `sh_addr` is
        // where the code really is, which is where LLDB loads it.
        let text = shdr(&bytes, 1);
        assert_eq!(
            string(&bytes, shstrtab(&bytes), u32_at(text, 0) as usize),
            ".text"
        );
        assert_eq!(u32_at(text, 4), SHT_PROGBITS);
        assert_eq!(u64_at(text, 8), SHF_ALLOC | SHF_EXECINSTR);
        assert_eq!(u64_at(text, 16), 0x1_0234_5000, "sh_addr");
        assert_eq!(u64_at(text, 32), 4, "sh_size");

        // A DWARF section is not allocated, so it gets no load address and is
        // read out of this object instead.
        let debug = shdr(&bytes, 3);
        assert_eq!(
            string(&bytes, shstrtab(&bytes), u32_at(debug, 0) as usize),
            ".debug_info"
        );
        assert_eq!(u64_at(debug, 8), 0, "sh_flags");
        assert_eq!(u64_at(debug, 16), 0, "sh_addr");
    }

    #[test]
    fn a_section_already_in_the_process_carries_no_bytes() {
        let bytes = object();
        // The hundred-megabyte case: an extent and an address, and nothing in
        // the file for LLDB to read when it can read the process instead.
        let data = shdr(&bytes, 2);
        assert_eq!(u32_at(data, 4), SHT_NOBITS);
        assert_eq!(u64_at(data, 8), SHF_ALLOC | SHF_WRITE);
        assert_eq!(u64_at(data, 16), 0x1_0300_0000, "sh_addr");
        assert_eq!(u64_at(data, 32), 0x4000, "sh_size");
        assert!(bytes.len() < 0x4000, "the extent was written out");
    }

    #[test]
    fn section_payloads_land_where_their_headers_say() {
        let bytes = object();
        let text = shdr(&bytes, 1);
        let offset = u64_at(text, 24) as usize;
        assert_eq!(&bytes[offset..offset + 4], &[0xc3, 0x90, 0x90, 0x90]);
        assert_eq!(offset % 4, 0, "honours the section's alignment");

        let debug = shdr(&bytes, 3);
        let offset = u64_at(debug, 24) as usize;
        assert_eq!(&bytes[offset..offset + 3], &[1, 2, 3]);
    }

    #[test]
    fn the_symbol_names_the_text_section_and_its_address() {
        let bytes = object();
        let symtab = shdr(&bytes, 5);
        assert_eq!(u32_at(symtab, 4), SHT_SYMTAB);
        let symoff = u64_at(symtab, 24) as usize;
        assert_eq!(
            u64_at(symtab, 32) as usize / SYM_SIZE,
            2,
            "the null symbol and ours"
        );
        let strtab = u64_at(shdr(&bytes, u32_at(symtab, 40) as usize), 24) as usize;

        // The second entry: the first is ELF's reserved null symbol.
        let sym = &bytes[symoff + SYM_SIZE..][..SYM_SIZE];
        assert_eq!(sym[4], GLOBAL_FUNC, "st_info");
        assert_eq!(u16_at(sym, 6), 1, "st_shndx is the 1-based section index");
        assert_eq!(
            u64_at(sym, 8),
            0x40,
            "st_value, from the start of its section"
        );
        // No underscore: that is Mach-O's, and LLDB would show it here.
        assert_eq!(
            string(&bytes, strtab, u32_at(sym, 0) as usize),
            "guest+0x40"
        );
    }

    #[test]
    fn one_name_is_written_once_however_many_sections_share_it() {
        // Every executable segment of a mapped image is a `.text`, and a reader
        // distinguishes them by address rather than by name.
        let bytes = write(
            &[
                Section::mapped(Kind::Code, "__TEXT", 0x1000, 12, 0x1000),
                Section::mapped(Kind::Code, "__TEXT_EXEC", 0x2000, 12, 0x1000),
            ],
            &[],
        );
        assert_eq!(u32_at(shdr(&bytes, 1), 0), u32_at(shdr(&bytes, 2), 0));
        assert_eq!(
            string(
                &bytes,
                shstrtab(&bytes),
                u32_at(shdr(&bytes, 2), 0) as usize
            ),
            ".text"
        );
    }
}
