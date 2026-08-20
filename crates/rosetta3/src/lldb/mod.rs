//! Opt-in LLDB support for translated guest code.

mod macho;

use std::collections::HashMap;
use std::fmt::Write as _;
use std::fs::File;
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use gimli::write::{Address, Dwarf, EndianVec, LineProgram, LineString, Sections, Unit};
use gimli::{Encoding, Format, LineEncoding, LittleEndian, constants};
use iced_x86::{Formatter, IntelFormatter};
use microlift::arch::x86_64::decode::decode_region;
use microlift::core::srcloc::GuestCodeMap;
use static_init::dynamic;


/// The layout LLDB expects, from `JITLoaderGDB.cpp`:
///
/// ```c
/// struct jit_code_entry {
///     void *next_entry;
///     void *prev_entry;
///     const void *symfile_addr;
///     uint64_t symfile_size;
/// };
/// ```
#[repr(C)]
pub struct JitCodeEntry {
    pub next: *mut JitCodeEntry,
    pub prev: *mut JitCodeEntry,
    pub symfile_addr: *const u8,
    pub symfile_size: u64,
}

/// ```c
/// struct jit_descriptor {
///     uint32_t version;
///     uint32_t action_flag;
///     void *relevant_entry;
///     void *first_entry;
/// };
/// ```
#[repr(C)]
pub struct JitDescriptor {
    pub version: u32,
    pub action_flag: u32,
    pub relevant_entry: *mut JitCodeEntry,
    pub first_entry: *mut JitCodeEntry,
}

const JIT_NOACTION: u32 = 0;
const JIT_REGISTER_FN: u32 = 1;

/// LLDB finds this by name, as `eSymbolTypeData`, and reads it out of the
/// debuggee whenever the hook below fires.
#[unsafe(no_mangle)]
#[used]
pub static mut __jit_debug_descriptor: JitDescriptor = JitDescriptor {
    version: 1,
    action_flag: JIT_NOACTION,
    relevant_entry: std::ptr::null_mut(),
    first_entry: std::ptr::null_mut(),
};

/// LLDB puts a breakpoint on this symbol and examines the descriptor when it fires.
#[unsafe(no_mangle)]
#[inline(never)]
pub extern "C" fn __jit_debug_register_code() {
    std::hint::black_box(());
}


/// Whether this module does anything, and the one place that says so out loud.
#[inline(always)]
pub fn enabled() -> bool {
    #[dynamic]
    static ENABLED: bool = {
        let on = std::env::var_os("ROSETTA3_LLDB").is_some_and(|value| {
            let value = value.to_string_lossy();
            !value.is_empty() && value != "0"
        });
        if on {
            eprintln!(
                "rosetta3: publishing translations to LLDB. Its JIT loader is off by \
                 default on macOS,\nrosetta3:   so LLDB needs this before any of it is \
                 visible:\nrosetta3:     settings set plugin.jit-loader.gdb.enable \
                 on\nrosetta3:   x86-64 listings: {}",
                listing_dir().display()
            );
        }
        on
    };
    *ENABLED
}


static REGISTER: Mutex<()> = Mutex::new(());

/// Publish one newly compiled translation to LLDB.
///
/// `code` is where the compiled arm64 landed, `guest_start..guest_end` the
/// x86-64 it was lifted from, `image` that region named against its image (the
/// `libfoo.dylib+0x1234` [`crate::program::Program::locate`] produces), `bytes`
/// the guest instructions themselves, and `map` microlift's host-offset →
/// guest-address table.
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

    let Some(object) = symfile(code, guest_start, guest_end, image, bytes, map) else {
        return;
    };

    // LLDB needs the object to stay readable for as long as the entry is
    // registered, and entries are never withdrawn — a translation lives as long
    // as the process. So both are leaked, deliberately.
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
    // * `entry` and `object` are leaked, so their addresses stay valid.
    // * `REGISTER` serializes every mutation of the global list, which guest
    //   threads reach concurrently: a lift runs outside the code cache's lock.
    // * LLDB reads this state from the stop inside `__jit_debug_register_code`,
    //   so the list is consistent for the whole time it is observable.
    unsafe {
        let descriptor = &raw mut __jit_debug_descriptor;

        // Prepended, and kept.
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


/// Build the Mach-O LLDB will read out of this process's memory.
fn symfile(
    code: u64,
    guest_start: u64,
    guest_end: u64,
    image: &str,
    bytes: &[u8],
    map: &GuestCodeMap,
) -> Option<Vec<u8>> {
    // The extent of the generated code, as the source locations see it.
    let native_size = map.ranges().iter().map(|r| u64::from(r.end)).max()?;
    if native_size == 0 {
        return None;
    }

    let listing = listing(image, guest_start, guest_end, bytes)?;
    let dwarf = dwarf(code, native_size, image, &listing, map)?;

    // `__text` first, and this is not cosmetic.
    let mut sections = vec![macho::Section {
        segname: "__TEXT",
        sectname: "__text",
        addr: code,
        align: 2,
        flags: macho::S_ATTR_PURE_INSTRUCTIONS | macho::S_ATTR_SOME_INSTRUCTIONS,
        // A copy of the live code. LLDB reads the running process for a loaded
        // section, so this is only ever the fallback — but a `__text` with no
        // contents is a section a reader is entitled to refuse.
        //
        // SAFETY: `code..code + native_size` is inside the buffer Cranelift
        // finalized for this region, which is mapped and immutable for the rest
        // of the process's life.
        data: unsafe { std::slice::from_raw_parts(code as *const u8, native_size as usize) }
            .to_vec(),
    }];

    for (sectname, data) in [
        ("__debug_abbrev", dwarf.debug_abbrev.slice()),
        ("__debug_info", dwarf.debug_info.slice()),
        ("__debug_line", dwarf.debug_line.slice()),
        ("__debug_str", dwarf.debug_str.slice()),
    ] {
        if data.is_empty() {
            continue;
        }
        sections.push(macho::Section {
            segname: "__DWARF",
            sectname,
            addr: 0,
            align: 0,
            flags: macho::S_ATTR_DEBUG,
            data: data.to_vec(),
        });
    }

    // Mach-O's C-linkage underscore. LLDB strips it back off for display, so
    // the frame reads `guest_fault+0x470` rather than `_guest_fault+0x470`.
    let syms = vec![macho::Sym {
        name: format!("_{image}"),
        sect: 1,
        value: code,
    }];

    Some(macho::write(&sections, &syms))
}

/// The DWARF for one region.
fn dwarf(
    code: u64,
    native_size: u64,
    image: &str,
    listing: &Listed,
    map: &GuestCodeMap,
) -> Option<Sections<EndianVec<LittleEndian>>> {
    let encoding = Encoding {
        format: Format::Dwarf32,
        version: 4,
        address_size: 8,
    };

    // A directory and a file, not one path in the file field: that is the pair
    // a consumer joins, and LLDB has to be able to open the result.
    let directory = listing
        .path
        .parent()?
        .as_os_str()
        .as_encoded_bytes()
        .to_vec();
    let filename = listing.path.file_name()?.as_encoded_bytes().to_vec();

    let mut program = LineProgram::new(
        encoding,
        LineEncoding::default(),
        LineString::String(directory.clone()),
        None,
        LineString::String(filename.clone()),
        None,
    );
    let default_directory = program.default_directory();
    let file = program.add_file(
        LineString::String(filename.clone()),
        default_directory,
        None,
    );

    program.begin_sequence(Some(Address::Constant(code)));

    // Rows are emitted in host-address order, which is the order `ranges` is
    // already in.
    let mut previous = None;
    let mut rows = 0usize;

    for range in map.ranges() {
        let Some(&line) = listing.lines.get(&range.guest_va) else {
            continue;
        };
        if previous == Some(line) {
            continue;
        }
        program.set_address(Address::Constant(code + u64::from(range.start)));
        let row = program.row();
        row.file = file;
        row.line = u64::from(line);
        program.generate_row();
        previous = Some(line);
        rows += 1;
    }

    if rows == 0 {
        return None;
    }

    // `end_sequence` takes an offset from the last address set, not an address.
    program.set_address(Address::Constant(code + native_size));
    program.end_sequence(0);

    let mut unit = Unit::new(encoding, program);

    let root = unit.root();
    let die = unit.get_mut(root);
    die.set(
        constants::DW_AT_producer,
        gimli::write::AttributeValue::String(b"rosetta3".to_vec()),
    );
    // The listing really is assembler, and saying so keeps LLDB from looking
    // for a language plugin it will not find.
    die.set(
        constants::DW_AT_language,
        gimli::write::AttributeValue::Language(constants::DW_LANG_Mips_Assembler),
    );
    die.set(
        constants::DW_AT_name,
        gimli::write::AttributeValue::String(filename),
    );
    die.set(
        constants::DW_AT_comp_dir,
        gimli::write::AttributeValue::String(directory),
    );
    // Without an address range on the unit itself, an address lookup never
    // reaches the DIEs inside it.
    die.set(
        constants::DW_AT_low_pc,
        gimli::write::AttributeValue::Address(Address::Constant(code)),
    );
    die.set(
        constants::DW_AT_high_pc,
        gimli::write::AttributeValue::Udata(native_size),
    );
    die.set(
        constants::DW_AT_stmt_list,
        gimli::write::AttributeValue::LineProgramRef,
    );

    let function = unit.add(root, constants::DW_TAG_subprogram);
    let die = unit.get_mut(function);
    die.set(
        constants::DW_AT_name,
        gimli::write::AttributeValue::String(image.as_bytes().to_vec()),
    );
    die.set(
        constants::DW_AT_low_pc,
        gimli::write::AttributeValue::Address(Address::Constant(code)),
    );
    die.set(
        constants::DW_AT_high_pc,
        gimli::write::AttributeValue::Udata(native_size),
    );
    die.set(
        constants::DW_AT_external,
        gimli::write::AttributeValue::Flag(true),
    );

    let mut dwarf = Dwarf::new();
    dwarf.units.add(unit);

    let mut sections = Sections::new(EndianVec::new(LittleEndian));
    dwarf.write(&mut sections).ok()?;
    Some(sections)
}


/// Where a region's guest instructions ended up in its image's listing.
#[derive(Clone)]
struct Listed {
    path: Arc<PathBuf>,
    /// Guest VA → line number.
    lines: Arc<HashMap<u64, u32>>,
}

/// One image's listing file, and how far into it we have written.
struct Listing {
    path: Arc<PathBuf>,
    file: File,
    next_line: u32,
}

#[derive(Default)]
struct Listings {
    images: HashMap<String, Listing>,
    /// Regions already listed.
    regions: HashMap<(String, u64), Listed>,
}

/// One lock over all of it, held across the decode and the append.
static LISTINGS: Mutex<Option<Listings>> = Mutex::new(None);

/// Where listings go: under the temp directory, in a per-process subdirectory so two
/// runs cannot interleave into one file and so a stale set is obviously stale.
fn listing_dir() -> PathBuf {
    std::env::temp_dir()
        .join("rosetta3-lldb")
        .join(std::process::id().to_string())
}

/// Append this region's x86-64 to its image's listing, and say which lines it
/// took.
fn listing(image: &str, guest_start: u64, guest_end: u64, bytes: &[u8]) -> Option<Listed> {
    // `locate` returns `image+0xoffset`; the file is per image, so the offset
    // is dropped here and kept in the block header below.
    let (name, _) = image.rsplit_once('+').unwrap_or((image, ""));

    let mut guard = LISTINGS.lock().unwrap_or_else(|e| e.into_inner());
    let listings = guard.get_or_insert_with(Listings::default);

    let key = (name.to_string(), guest_start);
    if let Some(listed) = listings.regions.get(&key) {
        return Some(listed.clone());
    }

    let listing = match listings.images.get_mut(name) {
        Some(listing) => listing,
        None => {
            let directory = listing_dir();
            std::fs::create_dir_all(&directory).ok()?;
            let safe: String = name
                .chars()
                .map(
                    |c| match c.is_ascii_alphanumeric() || c == '.' || c == '-' {
                        true => c,
                        false => '_',
                    },
                )
                .collect();
            let path = directory.join(format!("{safe}.x86-64.s"));
            let file = File::create(&path).ok()?;
            listings.images.entry(name.to_string()).or_insert(Listing {
                path: Arc::new(path),
                file,
                next_line: 1,
            })
        }
    };

    let mut text = String::new();
    let mut lines = HashMap::new();
    let mut line = listing.next_line;

    let _ = writeln!(
        text,
        "// ---- {image}  {guest_start:#x}..{guest_end:#x} ----"
    );
    line += 1;

    let mut formatter = IntelFormatter::new();
    let mut rendered = String::new();

    for instruction in decode_region(bytes, guest_start) {
        rendered.clear();
        formatter.format(&instruction, &mut rendered);
        let _ = writeln!(text, "{:#018x}  {rendered}", instruction.ip());
        lines.insert(instruction.ip(), line);
        line += 1;
    }

    if lines.is_empty() {
        return None;
    }

    text.push('\n');
    line += 1;

    // Written straight through rather than buffered.
    listing.file.write_all(text.as_bytes()).ok()?;
    listing.next_line = line;

    let listed = Listed {
        path: Arc::clone(&listing.path),
        lines: Arc::new(lines),
    };
    listings.regions.insert(key, listed.clone());
    Some(listed)
}
