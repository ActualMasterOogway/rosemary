//! Opt-in LLDB support for translated guest code.
//!
//! LLDB's JITLoaderGDB watches the GDB JIT registration ABI. On macOS it
//! expects the JIT object to be Mach-O, not ELF.
//!
//! Nothing in this module is active unless `ROSETTA3_LLDB` is set to a
//! non-zero value.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use gimli::write::{
    Address, Dwarf, EndianVec, LineProgram, LineString, Sections, Unit,
};
use gimli::{
    Encoding, Format, LineEncoding, LittleEndian, constants,
};
use microlift::arch::x86_64::decode::decode_region;
use microlift::backend::ArtifactBackend;
use microlift::core::srcloc::GuestCodeMap;

use object::write::{
    Object, Symbol, SymbolFlags, SymbolKind, SymbolScope, SymbolSection,
};
use object::{
    Architecture, BinaryFormat, Endianness, SectionKind,
};

/// The ABI LLDB calls the GDB JIT interface.
///
/// Keep these layouts identical to LLDB's:
///
/// struct jit_code_entry {
///     void *next_entry;
///     void *prev_entry;
///     const void *symfile_addr;
///     uint64_t symfile_size;
/// };
#[repr(C)]
pub struct JitCodeEntry {
    pub next: *mut JitCodeEntry,
    pub prev: *mut JitCodeEntry,
    pub symfile_addr: *const u8,
    pub symfile_size: u64,
}

/// struct jit_descriptor {
///     uint32_t version;
///     uint32_t action_flag;
///     void *relevant_entry;
///     void *first_entry;
/// };
#[repr(C)]
pub struct JitDescriptor {
    pub version: u32,
    pub action_flag: u32,
    pub relevant_entry: *mut JitCodeEntry,
    pub first_entry: *mut JitCodeEntry,
}

const JIT_NOACTION: u32 = 0;
const JIT_REGISTER_FN: u32 = 1;

#[unsafe(no_mangle)]
#[used]
pub static mut __jit_debug_descriptor: JitDescriptor = JitDescriptor {
    version: 1,
    action_flag: JIT_NOACTION,
    relevant_entry: std::ptr::null_mut(),
    first_entry: std::ptr::null_mut(),
};

/// LLDB puts a breakpoint on this symbol and examines the descriptor when it
/// fires.
///
/// Do not inline/remove this function: its address is part of the external
/// debugger ABI.
#[unsafe(no_mangle)]
#[inline(never)]
pub extern "C" fn __jit_debug_register_code() {
    std::hint::black_box(());
}

static ENABLED: OnceLock<bool> = OnceLock::new();
static REGISTER: Mutex<()> = Mutex::new(());

pub fn enabled() -> bool {
    *ENABLED.get_or_init(|| {
        std::env::var_os("ROSETTA3_LLDB").is_some_and(|value| {
            let value = value.to_string_lossy();
            !value.is_empty() && value != "0"
        })
    })
}

/// Publish one newly compiled translation to LLDB.
pub fn register(
    code: u64,
    guest_start: u64,
    guest_end: u64,
    image: &str,
    bytes: &[u8],
    map: &GuestCodeMap,
) {
    if !enabled() || map.is_empty() {
        return;
    }

    let Some(object) =
        make_debug_object(code, guest_start, guest_end, image, bytes, map)
    else {
        return;
    };

    // LLDB only needs the object to remain readable for as long as the JIT
    // entry remains registered. Since we currently never unregister entries,
    // leak both objects and entries.
    let object = Box::leak(object.into_boxed_slice());

    let entry = Box::leak(Box::new(JitCodeEntry {
        next: std::ptr::null_mut(),
        prev: std::ptr::null_mut(),
        symfile_addr: object.as_ptr(),
        symfile_size: object.len() as u64,
    }));

    let _guard = REGISTER.lock().unwrap_or_else(|e| e.into_inner());

    // SAFETY:
    //
    // * `entry` and `object` are leaked, so their addresses remain valid.
    // * REGISTER serializes mutations of the global descriptor/list.
    // * LLDB reads this state when it hits __jit_debug_register_code.
    unsafe {
        let descriptor = &raw mut __jit_debug_descriptor;

        entry.next = (*descriptor).first_entry;

        if !entry.next.is_null() {
            (*entry.next).prev = entry;
        }

        (*descriptor).first_entry = entry;
        (*descriptor).relevant_entry = entry;
        (*descriptor).action_flag = JIT_REGISTER_FN;

        __jit_debug_register_code();

        (*descriptor).action_flag = JIT_NOACTION;
        (*descriptor).relevant_entry = std::ptr::null_mut();
    }
}

/// Build an in-memory Mach-O object containing DWARF debug information.
///
/// IMPORTANT: this must be Mach-O on macOS. Apple LLDB's JITLoaderGDB has
/// explicit Mach-O handling and otherwise treats the object as a generic
/// object file.
fn make_debug_object(
    code: u64,
    guest_start: u64,
    _guest_end: u64,
    image: &str,
    bytes: &[u8],
    map: &GuestCodeMap,
) -> Option<Vec<u8>> {
    let (source_path, source_lines) =
        write_listing(code, guest_start, image, bytes)?;

    // Native extent, not guest extent.
    //
    // `code + range.start` is the address appearing in the DWARF line
    // program, so the sequence must terminate at the end of the native
    // generated region.
    let native_size = map
        .ranges()
        .iter()
        .map(|range| u64::from(range.end))
        .max()?;

    if native_size == 0 {
        return None;
    }

    let name = format!("rosetta3::{image}+{guest_start:#x}");

    let encoding = Encoding {
        format: Format::Dwarf32,
        version: 4,
        address_size: 8,
    };

    //
    // DWARF source location.
    //
    // Use a real directory + filename pair instead of putting the complete
    // path into the file field.
    //
    let directory = source_path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("/"));

    let filename = source_path.file_name()?;

    let mut line_program = LineProgram::new(
        encoding,
        LineEncoding::default(),
        LineString::String(
            directory
                .to_string_lossy()
                .as_bytes()
                .to_vec(),
        ),
        None,
        LineString::String(filename.as_encoded_bytes().to_vec()),
        None,
    );

    let directory = line_program.default_directory();

    let file = line_program.add_file(
        LineString::String(filename.as_encoded_bytes().to_vec()),
        directory,
        None,
    );

    line_program.begin_sequence(Some(Address::Constant(code)));

    let mut emitted_any = false;

    for range in map.ranges() {
        let Some(&line) = source_lines.get(&range.guest_va) else {
            continue;
        };

        let native_start = code + u64::from(range.start);

        line_program.set_address(Address::Constant(native_start));

        let row = line_program.row();
        row.file = file;
        row.line = u64::from(line);

        line_program.generate_row();

        emitted_any = true;
    }

    if !emitted_any {
        return None;
    }

    line_program.end_sequence(code + native_size);

    let mut unit = Unit::new(encoding, line_program);

    let root = unit.get_mut(unit.root());

    root.set(
        constants::DW_AT_name,
        gimli::write::AttributeValue::String(
            name.as_bytes().to_vec(),
        ),
    );

    root.set(
        constants::DW_AT_stmt_list,
        gimli::write::AttributeValue::LineProgramRef,
    );

    //
    // Give LLDB a subprogram DIE so it can display a useful function name
    // rather than only a source location.
    //
    let function =
        unit.add(unit.root(), constants::DW_TAG_subprogram);

    let function_die = unit.get_mut(function);

    function_die.set(
        constants::DW_AT_name,
        gimli::write::AttributeValue::String(
            name.as_bytes().to_vec(),
        ),
    );

    function_die.set(
        constants::DW_AT_low_pc,
        gimli::write::AttributeValue::Address(
            Address::Constant(code),
        ),
    );

    function_die.set(
        constants::DW_AT_high_pc,
        gimli::write::AttributeValue::Data8(native_size),
    );

    let mut dwarf = Dwarf::new();

    dwarf.units.add(unit);

    let mut sections =
        Sections::new(EndianVec::new(LittleEndian));

    dwarf.write(&mut sections).ok()?;

    //
    // THIS is the important macOS change.
    //
    // The JIT object must be a Mach-O relocatable object. The live code is
    // still the existing arm64 allocation; the Mach-O exists solely as
    // LLDB's debug/symbol container.
    //
    let mut object = Object::new(
        BinaryFormat::MachO,
        Architecture::Aarch64,
        Endianness::Little,
    );

    //
    // Keep the JIT symbol absolute. LLDB's Mach-O JIT loader will relocate
    // the object sections based on the symfile address.
    //
    object.add_symbol(Symbol {
        name: name.as_bytes().to_vec(),
        value: code,
        size: native_size,
        kind: SymbolKind::Text,
        scope: SymbolScope::Linkage,
        weak: false,
        section: SymbolSection::Absolute,
        flags: SymbolFlags::None,
    });

    //
    // `SectionKind::Debug` is important here. The object crate maps this to
    // Mach-O's __DWARF/__debug_* sections rather than ELF-style sections.
    //
    for (name, data) in [
        (
            b"__debug_abbrev".as_slice(),
            sections.debug_abbrev.slice(),
        ),
        (
            b"__debug_info".as_slice(),
            sections.debug_info.slice(),
        ),
        (
            b"__debug_line".as_slice(),
            sections.debug_line.slice(),
        ),
        (
            b"__debug_str".as_slice(),
            sections.debug_str.slice(),
        ),
    ] {
        if data.is_empty() {
            continue;
        }

        let section =
            object.add_section(
                Vec::new(),
                name.to_vec(),
                SectionKind::Debug,
            );

        object.set_section_data(
            section,
            data.to_vec(),
            1,
        );
    }

    object.write().ok()
}

/// Write the x86-64 view LLDB will show when it resolves a generated line.
fn write_listing(
    _code: u64,
    guest_start: u64,
    image: &str,
    bytes: &[u8],
) -> Option<(PathBuf, BTreeMap<u64, u32>)> {
    //
    // `std::env::temp_dir()` is preferable to hard-coding /tmp on macOS.
    // It also makes this work correctly in unusual sandbox/container setups.
    //
    let dir = std::env::temp_dir().join("rosetta3-lldb");

    std::fs::create_dir_all(&dir).ok()?;

    let safe_image: String = image
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c
            } else {
                '_'
            }
        })
        .collect();

    let path = dir.join(format!(
        "{safe_image}-{guest_start:x}.s"
    ));

    let instructions =
        decode_region(bytes, guest_start);

    let disassembly =
        ArtifactBackend::disassemble(
            bytes,
            Some("x86_64-apple-darwin"),
        )
        .ok()?;

    let mut disassembly = disassembly.lines();

    let mut listing = format!(
        "; rosetta3 guest image: {image}\n\
         ; x86-64 lifted region {guest_start:#x}\n"
    );

    let mut lines = BTreeMap::new();

    for instruction in instructions {
        let text = disassembly.next()?;

        //
        // Count the actual physical line before appending it.
        //
        let line = listing.lines().count() as u32 + 1;

        listing.push_str(&format!(
            "{:#x}: {text}\n",
            instruction.ip()
        ));

        lines.insert(instruction.ip(), line);
    }

    std::fs::write(&path, listing).ok()?;

    Some((path, lines))
}