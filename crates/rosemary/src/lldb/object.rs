/// What a section is for.
///
/// LLDB types a section by its name, and only the name its own reader
/// recognises as code is disassembled as code - so the name is the format's to
/// choose, not the caller's.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Executable program text.
    Code,
    /// Mapped, non-executable data.
    Data,
    /// A DWARF section, named without its format's prefix: `info`, `line`.
    Debug(&'static str),
}

impl Kind {
    /// Whether the section holds instructions, which decides both its flags and
    /// the type of any symbol defined in it.
    pub fn executable(self) -> bool {
        matches!(self, Kind::Code)
    }
}

pub struct Section {
    pub kind: Kind,
    /// The mapping this section came from, when the caller has a name for it:
    /// Mach-O's segment name, which is what LLDB shows beside the section. Empty
    /// takes the format's default. ELF has no such field and ignores it.
    pub segment: String,
    /// Where the section really is in this process. Zero for debug sections,
    /// which are placed by the reader rather than by an address.
    pub addr: u64,
    /// Alignment as a power of two.
    pub align: u32,
    /// The section's extent. Larger than `data` for a section whose contents
    /// LLDB is expected to read out of the live process instead - which is
    /// every section of a mapped image, none of which is worth copying.
    pub size: u64,
    pub data: Vec<u8>,
}

impl Section {
    /// A section whose bytes travel with the object.
    pub fn resident(kind: Kind, addr: u64, align: u32, data: Vec<u8>) -> Self {
        Section {
            kind,
            segment: String::new(),
            addr,
            align,
            size: data.len() as u64,
            data,
        }
    }

    /// A section that is already in the process, and so carries no bytes.
    pub fn mapped(kind: Kind, segment: &str, addr: u64, align: u32, size: u64) -> Self {
        Section {
            kind,
            segment: segment.to_string(),
            addr,
            align,
            size,
            data: Vec::new(),
        }
    }
}

pub struct Sym {
    /// The name as a debugger should show it. Mach-O's C-linkage underscore is
    /// the writer's to add, not the caller's.
    pub name: String,
    /// 1-based index into `sections`, which is both formats' own encoding.
    pub sect: u8,
    /// The symbol's address, in the same space as its section's `addr`.
    pub value: u64,
}

/// Serialize `sections` and `syms` into the object file this host's LLDB reads.
pub fn write(sections: &[Section], syms: &[Sym]) -> Vec<u8> {
    #[cfg(target_os = "macos")]
    {
        super::macho::write(sections, syms)
    }
    #[cfg(not(target_os = "macos"))]
    {
        super::elf::write(sections, syms)
    }
}
