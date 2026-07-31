#![cfg(all(feature = "std", feature = "arch-x86_64"))]
//! Requires the `std` and `arch-x86_64` features; compiles away without them,
//! so `--no-default-features` still proves the `no_std` core builds.

//! Hostile and malformed input.
//!
//! Every case here must produce a specific [`Error`] — never a panic, never a
//! hang, never an unbounded allocation. The input to this crate is an untrusted
//! third-party binary, so totality is a correctness requirement rather than
//! polish. Several of these (a zero `cmdsize`, a load-command chain running past
//! `sizeofcmds`) are bugs that have bitten real Mach-O loaders.

use machish::image::MachObject;
use machish::spec::arch::X86_64;
use machish::spec::constants::{
    MH_CIGAM_64, MH_MAGIC, MH_MAGIC_64, MH_OBJECT, MH_EXECUTE, CPU_TYPE_X86_64, CPU_TYPE_ARM64,
    LC_SEGMENT_64, LC_LOAD_DYLIB, LC_SOURCE_VERSION, MACH_HEADER_64_SIZE,
    SEGMENT_COMMAND_64_SIZE,
};
use machish::Error;

/// Build a `mach_header_64` followed by raw command bytes.
fn image(filetype: u32, ncmds: u32, sizeofcmds: u32, cmds: &[u8]) -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(&MH_MAGIC_64.to_le_bytes());
    v.extend_from_slice(&CPU_TYPE_X86_64.to_le_bytes());
    v.extend_from_slice(&3i32.to_le_bytes()); // CPU_SUBTYPE_X86_64_ALL
    v.extend_from_slice(&filetype.to_le_bytes());
    v.extend_from_slice(&ncmds.to_le_bytes());
    v.extend_from_slice(&sizeofcmds.to_le_bytes());
    v.extend_from_slice(&0u32.to_le_bytes()); // flags
    v.extend_from_slice(&0u32.to_le_bytes()); // reserved
    assert_eq!(v.len(), MACH_HEADER_64_SIZE);
    v.extend_from_slice(cmds);
    v
}

fn cmd(cmd: u32, cmdsize: u32) -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(&cmd.to_le_bytes());
    v.extend_from_slice(&cmdsize.to_le_bytes());
    v
}

fn parse(bytes: &[u8]) -> Result<MachObject<'_, X86_64>, Error> {
    MachObject::parse(bytes, X86_64)
}

#[test]
fn empty_and_short_input() {
    assert!(parse(&[]).is_err());
    assert!(parse(&[0xcf]).is_err());
    assert!(parse(&MH_MAGIC_64.to_le_bytes()).is_err());
    // A header truncated one byte short of complete.
    let mut v = image(MH_EXECUTE, 0, 0, &[]);
    v.truncate(MACH_HEADER_64_SIZE - 1);
    assert!(parse(&v).is_err());
}

#[test]
fn distinguishes_wrong_file_from_unsupported_flavor() {
    // Not a Mach-O at all.
    let mut v = 0x0bad_f00du32.to_le_bytes().to_vec();
    v.resize(64, 0);
    assert_eq!(parse(&v).err(), Some(Error::BadMagic(0x0bad_f00d)));

    // A real Mach-O, but 32-bit: recognized and deliberately refused.
    let mut v = MH_MAGIC.to_le_bytes().to_vec();
    v.resize(64, 0);
    assert_eq!(parse(&v).err(), Some(Error::UnsupportedMachOForm(MH_MAGIC)));

    // Byte-swapped 64-bit, likewise.
    let mut v = MH_CIGAM_64.to_le_bytes().to_vec();
    v.resize(64, 0);
    assert_eq!(parse(&v).err(), Some(Error::UnsupportedMachOForm(MH_CIGAM_64)));
}

#[test]
fn rejects_unsupported_filetype() {
    // MH_OBJECT is a relocatable .o: not something a loader maps.
    let v = image(MH_OBJECT, 0, 0, &[]);
    assert_eq!(parse(&v).err(), Some(Error::UnsupportedFiletype(MH_OBJECT)));
}

#[test]
fn rejects_wrong_cpu_type() {
    let mut v = image(MH_EXECUTE, 0, 0, &[]);
    v[4..8].copy_from_slice(&CPU_TYPE_ARM64.to_le_bytes());
    assert_eq!(
        parse(&v).err(),
        Some(Error::WrongCpuType { expected: CPU_TYPE_X86_64, found: CPU_TYPE_ARM64 })
    );
}

/// The classic infinite loop: a command that advances the cursor by zero.
/// If this test hangs rather than fails, the guard has regressed.
#[test]
fn zero_cmdsize_does_not_loop_forever() {
    let c = cmd(LC_SEGMENT_64, 0);
    let v = image(MH_EXECUTE, 1, 8, &c);
    assert_eq!(parse(&v).err(), Some(Error::BadLoadCommand { why: "zero cmdsize" }));
}

#[test]
fn rejects_unaligned_cmdsize() {
    // Every load command must be 8-byte aligned; a misaligned one desynchronizes
    // every subsequent field.
    let mut c = cmd(LC_LOAD_DYLIB, 12);
    c.resize(12, 0);
    let v = image(MH_EXECUTE, 1, 12, &c);
    assert_eq!(parse(&v).err(), Some(Error::BadLoadCommand { why: "unaligned cmdsize" }));
}

#[test]
fn rejects_command_running_past_sizeofcmds() {
    // cmdsize claims far more than the declared command region holds, which
    // would let this command read the next one's bytes as its payload.
    let mut c = cmd(LC_LOAD_DYLIB, 4096);
    c.resize(64, 0);
    let v = image(MH_EXECUTE, 1, 16, &c);
    assert_eq!(parse(&v).err(), Some(Error::BadLoadCommand { why: "cmdsize past sizeofcmds" }));
}

#[test]
fn rejects_segment_command_too_small_for_its_own_fields() {
    // A 16-byte LC_SEGMENT_64 cannot hold a 72-byte segment_command_64. Caught
    // directly rather than as an incidental read past the end of the file.
    let mut c = cmd(LC_SEGMENT_64, 16);
    c.resize(16, 0);
    let v = image(MH_EXECUTE, 1, 16, &c);
    assert_eq!(
        parse(&v).err(),
        Some(Error::BadLoadCommand { why: "segment command too small" })
    );
}

#[test]
fn rejects_sizeofcmds_past_end_of_file() {
    let v = image(MH_EXECUTE, 1, 0x1000, &cmd(LC_SEGMENT_64, 16));
    assert_eq!(parse(&v).err(), Some(Error::Truncated { what: "load commands" }));
}

#[test]
fn rejects_absurd_command_count() {
    // Would otherwise drive a 4-billion-element Vec::with_capacity.
    let v = image(MH_EXECUTE, u32::MAX, 0, &[]);
    assert_eq!(parse(&v).err(), Some(Error::TooDeep("load command count")));
}

#[test]
fn rejects_more_commands_than_bytes() {
    // ncmds says ten, sizeofcmds only covers one. A payload-free command type
    // is used so this isolates the count-vs-extent mismatch instead of tripping
    // a size check on the command's own fields first.
    let mut c = cmd(LC_SOURCE_VERSION, 16);
    c.resize(16, 0);
    let v = image(MH_EXECUTE, 10, 16, &c);
    assert_eq!(
        parse(&v).err(),
        Some(Error::BadLoadCommand { why: "command header past sizeofcmds" })
    );
}

#[test]
fn rejects_segment_whose_sections_overflow_its_command() {
    // A segment_command_64 claiming sections that do not fit inside cmdsize.
    // Without the check these would be read from whatever follows.
    let mut c = Vec::new();
    c.extend_from_slice(&LC_SEGMENT_64.to_le_bytes());
    c.extend_from_slice(&(SEGMENT_COMMAND_64_SIZE as u32).to_le_bytes());
    c.resize(SEGMENT_COMMAND_64_SIZE, 0);
    // nsects = 100, at offset 64 within the command.
    c[64..68].copy_from_slice(&100u32.to_le_bytes());

    let v = image(MH_EXECUTE, 1, SEGMENT_COMMAND_64_SIZE as u32, &c);
    assert_eq!(
        parse(&v).err(),
        Some(Error::BadLoadCommand { why: "sections past segment command" })
    );
}

#[test]
fn rejects_dylib_string_offset_out_of_bounds() {
    // The install-name offset points far outside the command.
    let mut c = Vec::new();
    c.extend_from_slice(&LC_LOAD_DYLIB.to_le_bytes());
    c.extend_from_slice(&32u32.to_le_bytes());
    c.extend_from_slice(&0xffff_fff0u32.to_le_bytes()); // name offset
    c.resize(32, 0);

    let v = image(MH_EXECUTE, 1, 32, &c);
    assert!(parse(&v).is_err(), "an out-of-range string offset must not panic");
}

#[test]
fn rejects_unterminated_dylib_name() {
    // A name offset inside the file, but with no NUL before the end.
    let mut c = Vec::new();
    c.extend_from_slice(&LC_LOAD_DYLIB.to_le_bytes());
    c.extend_from_slice(&32u32.to_le_bytes());
    c.extend_from_slice(&24u32.to_le_bytes()); // name at command + 24
    c.resize(24, 0);
    c.extend_from_slice(b"NoNulHere"); // runs to EOF with no terminator
    let len = c.len() as u32;

    let v = image(MH_EXECUTE, 1, len.next_multiple_of(8), &c);
    assert!(parse(&v).is_err());
}

#[test]
fn image_with_no_mappable_segments_reports_it() {
    // Structurally valid, but nothing to map: preferred_base and vm_span must
    // say so rather than returning a nonsense address.
    let v = image(MH_EXECUTE, 0, 0, &[]);
    let obj = parse(&v).unwrap();
    assert_eq!(obj.preferred_base().err(), Some(Error::NoLoadableSegments));
    assert_eq!(obj.vm_span().err(), Some(Error::NoLoadableSegments));
    assert!(obj.entry().is_none());
}

/// A cheap stand-in for a fuzzer: every prefix of a real binary must fail
/// cleanly rather than panic. Truncation is the single most common corruption,
/// and it exercises nearly every bounds check in the parser.
#[test]
fn every_prefix_of_a_real_binary_fails_cleanly() {
    let Ok(bytes) = std::fs::read("/bin/ls") else { return };
    let thin = match machish::fat::select(&bytes, &X86_64) {
        Ok(t) => t.to_vec(),
        Err(_) => return,
    };

    for len in 0..thin.len().min(4096) {
        // The contract is only "does not panic"; success or failure both fine.
        let _ = parse(&thin[..len]);
    }
    // And the whole thing still parses.
    assert!(parse(&thin).is_ok());
}

/// Flipping bytes in the load-command region must never panic either.
#[test]
fn corrupted_load_commands_fail_cleanly() {
    let Ok(bytes) = std::fs::read("/bin/ls") else { return };
    let thin = match machish::fat::select(&bytes, &X86_64) {
        Ok(t) => t.to_vec(),
        Err(_) => return,
    };

    let sizeofcmds = u32::from_le_bytes(thin[20..24].try_into().unwrap()) as usize;
    let end = (MACH_HEADER_64_SIZE + sizeofcmds).min(thin.len());

    for i in MACH_HEADER_64_SIZE..end {
        for patch in [0x00u8, 0xff] {
            let mut corrupt = thin.clone();
            corrupt[i] = patch;
            let _ = parse(&corrupt);
        }
    }
}

// --------------------------------------------- decoders on hostile input ----
//
// The parser's totality is only half the requirement. The fixup decoders and
// the export trie walker read `__LINKEDIT` — offsets and counts that the header
// checks never look at — so they need their own sweep. Each case below asserts
// only "does not panic and does not hang"; whether a particular corruption
// happens to produce an error or a plausible-looking decode is not something
// worth pinning down, and pinning it down would make the test a change
// detector.

/// The corruptible extent of a real binary: header, load commands, and
/// `__LINKEDIT`, where every structure these decoders read actually lives.
fn thin_slice(path: &str) -> Option<Vec<u8>> {
    let bytes = std::fs::read(path).ok()?;
    Some(machish::fat::select(&bytes, &X86_64).ok()?.to_vec())
}

fn decode_everything(bytes: &[u8]) {
    let Ok(obj) = parse(bytes) else { return };
    let _ = machish::exports::ExportTable::parse(&obj);
    let _ = machish::exports::ExportTable::parse_symtab(&obj);
    let _ = machish::fixups::decode_image(&obj);
    let _ = machish::fixups::StubMap::build(&obj);
    // `__unwind_info` is read from `__TEXT` rather than `__LINKEDIT`, but it is
    // the same shape of risk: offsets and counts inside the section that no
    // header check ever looks at.
    let _ = machish::image::UnwindInfo::parse(&obj, 0);
}

#[test]
fn decoders_survive_every_prefix_of_a_real_binary() {
    for path in ["/bin/ls", "/bin/echo"] {
        let Some(thin) = thin_slice(path) else { continue };
        // Step rather than walk every byte: a full sweep of a 100 KiB binary is
        // 100k full decodes, which turns a fast test into a slow one without
        // reaching a meaningfully different set of code paths.
        for len in (0..thin.len()).step_by(97) {
            decode_everything(&thin[..len]);
        }
        decode_everything(&thin);
    }
}

#[test]
fn decoders_survive_single_byte_corruption() {
    for path in ["/bin/ls", "/bin/echo"] {
        let Some(thin) = thin_slice(path) else { continue };
        for i in (0..thin.len()).step_by(211) {
            for patch in [0x00u8, 0xff, 0x80] {
                let mut corrupt = thin.clone();
                corrupt[i] = patch;
                decode_everything(&corrupt);
            }
        }
    }
}

#[test]
fn a_chained_fixups_command_pointing_at_nonsense_is_an_error() {
    // The chained decoder reads offsets out of its own header, so a header of
    // all-ones is the shortest path to every bounds check it has.
    let Some(mut thin) = thin_slice("/bin/echo") else { return };
    let Ok(obj) = parse(&thin) else { return };
    let Some(cf) = obj.chained_fixups() else { return };
    let at = cf.dataoff as usize;
    if at + 32 > thin.len() {
        return;
    }
    for b in &mut thin[at..at + 32] {
        *b = 0xff;
    }
    let obj = parse(&thin).unwrap();
    assert!(
        machish::fixups::decode_image(&obj).is_err(),
        "a chained header of all-ones must not decode"
    );
}
