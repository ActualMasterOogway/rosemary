//! Mach-O on-disk constants, named as they are in `<mach-o/loader.h>` so the
//! parser reads like the format specification.

// ---------------------------------------------------------------- magic ----

/// 64-bit Mach-O, host-endian (little). The only form this crate accepts.
pub const MH_MAGIC_64: u32 = 0xfeed_facf;
/// 64-bit Mach-O, byte-swapped. Recognized only so the error can be specific.
pub const MH_CIGAM_64: u32 = 0xcffa_edfe;
/// 32-bit Mach-O, host-endian. Rejected.
pub const MH_MAGIC: u32 = 0xfeed_face;
/// 32-bit Mach-O, byte-swapped. Rejected.
pub const MH_CIGAM: u32 = 0xcefa_edfe;

/// Fat/universal archive header, 32-bit offsets. Always big-endian on disk.
pub const FAT_MAGIC: u32 = 0xcafe_babe;
/// Fat/universal archive header, 64-bit offsets.
pub const FAT_MAGIC_64: u32 = 0xcafe_babf;

// ------------------------------------------------------------- cpu type ----

pub const CPU_ARCH_ABI64: i32 = 0x0100_0000;
pub const CPU_TYPE_X86: i32 = 7;
pub const CPU_TYPE_X86_64: i32 = CPU_TYPE_X86 | CPU_ARCH_ABI64;
pub const CPU_TYPE_ARM: i32 = 12;
pub const CPU_TYPE_ARM64: i32 = CPU_TYPE_ARM | CPU_ARCH_ABI64;

/// The capability/feature bits of a `cpusubtype`, occupying the **top** byte.
/// `CPU_SUBTYPE_LIB64` is `0x8000_0000`, and `arm64e` slices carry `0x80` in
/// that byte to mark the pointer-authentication ABI version — so `/bin/ls`'s
/// arm64e slice reports `cpusubtype` as `0x8000_0002`, not `2`.
pub const CPU_SUBTYPE_FEATURE_MASK: u32 = 0xff00_0000;

/// Strip the capability bits, leaving the architecture identity.
pub const fn subtype_of(cpusubtype: i32) -> i32 {
    (cpusubtype as u32 & !CPU_SUBTYPE_FEATURE_MASK) as i32
}

pub const CPU_SUBTYPE_X86_64_ALL: i32 = 3;
/// Haswell baseline: assumes AVX2/BMI, which the lifter may not implement.
pub const CPU_SUBTYPE_X86_64_H: i32 = 8;
pub const CPU_SUBTYPE_ARM64_ALL: i32 = 0;
pub const CPU_SUBTYPE_ARM64E: i32 = 2;

// ------------------------------------------------------------- filetype ----

pub const MH_OBJECT: u32 = 0x1;
pub const MH_EXECUTE: u32 = 0x2;
pub const MH_DYLIB: u32 = 0x6;
pub const MH_DYLINKER: u32 = 0x7;
pub const MH_BUNDLE: u32 = 0x8;

// ---------------------------------------------------------------- flags ----

/// Two-level namespace: imports are keyed by (library ordinal, name).
pub const MH_TWOLEVEL: u32 = 0x0000_0080;
/// Flat namespace lookup, the pre-10.1 behaviour.
pub const MH_FORCE_FLAT: u32 = 0x0000_0100;
/// Position-independent: the image tolerates being slid.
pub const MH_PIE: u32 = 0x0020_0000;
/// The image's fixups are `LC_DYLD_CHAINED_FIXUPS` style.
pub const MH_DYLDLINK: u32 = 0x0000_0004;

// -------------------------------------------------------- load commands ----

/// Set on commands the loader must understand or refuse to run the image.
pub const LC_REQ_DYLD: u32 = 0x8000_0000;

pub const LC_SYMTAB: u32 = 0x02;
pub const LC_DYSYMTAB: u32 = 0x0b;
pub const LC_LOAD_DYLIB: u32 = 0x0c;
pub const LC_ID_DYLIB: u32 = 0x0d;
pub const LC_LOAD_DYLINKER: u32 = 0x0e;
pub const LC_ROUTINES_64: u32 = 0x1a;
pub const LC_SEGMENT_64: u32 = 0x19;
pub const LC_UUID: u32 = 0x1b;
pub const LC_CODE_SIGNATURE: u32 = 0x1d;
pub const LC_ENCRYPTION_INFO_64: u32 = 0x2c;
pub const LC_UNIXTHREAD: u32 = 0x05;
pub const LC_FUNCTION_STARTS: u32 = 0x26;
pub const LC_DATA_IN_CODE: u32 = 0x29;
pub const LC_SOURCE_VERSION: u32 = 0x2a;
pub const LC_BUILD_VERSION: u32 = 0x32;

pub const LC_RPATH: u32 = 0x1c | LC_REQ_DYLD;
pub const LC_LOAD_WEAK_DYLIB: u32 = 0x18 | LC_REQ_DYLD;
pub const LC_REEXPORT_DYLIB: u32 = 0x1f | LC_REQ_DYLD;
pub const LC_LOAD_UPWARD_DYLIB: u32 = 0x23 | LC_REQ_DYLD;
pub const LC_MAIN: u32 = 0x28 | LC_REQ_DYLD;
pub const LC_DYLD_INFO: u32 = 0x22;
pub const LC_DYLD_INFO_ONLY: u32 = 0x22 | LC_REQ_DYLD;
pub const LC_DYLD_EXPORTS_TRIE: u32 = 0x33 | LC_REQ_DYLD;
pub const LC_DYLD_CHAINED_FIXUPS: u32 = 0x34 | LC_REQ_DYLD;

// ------------------------------------------------------------ vm protect ----

pub const VM_PROT_READ: i32 = 0x1;
pub const VM_PROT_WRITE: i32 = 0x2;
pub const VM_PROT_EXECUTE: i32 = 0x4;

// ------------------------------------------------------ section  types -----

/// `flags & SECTION_TYPE` isolates the type from the attribute bits.
pub const SECTION_TYPE: u32 = 0x0000_00ff;

pub const S_REGULAR: u8 = 0x0;
pub const S_ZEROFILL: u8 = 0x1;
pub const S_NON_LAZY_SYMBOL_POINTERS: u8 = 0x6;
pub const S_LAZY_SYMBOL_POINTERS: u8 = 0x7;
/// `reserved1` indexes the indirect symbol table; `reserved2` is the stub size.
pub const S_SYMBOL_STUBS: u8 = 0x8;
pub const S_MOD_INIT_FUNC_POINTERS: u8 = 0x9;
pub const S_MOD_TERM_FUNC_POINTERS: u8 = 0xa;
pub const S_THREAD_LOCAL_REGULAR: u8 = 0x11;
pub const S_THREAD_LOCAL_ZEROFILL: u8 = 0x12;
pub const S_THREAD_LOCAL_VARIABLES: u8 = 0x13;
pub const S_THREAD_LOCAL_VARIABLE_POINTERS: u8 = 0x14;
pub const S_THREAD_LOCAL_INIT_FUNCTION_POINTERS: u8 = 0x15;

// ----------------------------------------------------- structure  sizes ----

pub const MACH_HEADER_64_SIZE: usize = 32;
pub const LOAD_COMMAND_SIZE: usize = 8;
pub const SEGMENT_COMMAND_64_SIZE: usize = 72;
pub const SECTION_64_SIZE: usize = 80;
pub const FAT_HEADER_SIZE: usize = 8;
pub const FAT_ARCH_SIZE: usize = 20;
pub const FAT_ARCH_64_SIZE: usize = 32;
