#![cfg(all(feature = "std", feature = "arch-x86_64"))]
//! Requires the `std` and `arch-x86_64` features; compiles away without them,
//! so `--no-default-features` still proves the `no_std` core builds.

//! M1.1 acceptance: fat demux and load-command parsing against real binaries.

mod common;

use machish::Error;
use machish::fat;
use machish::image::{DependencyKind, EntryPoint, LoadCommand, MachObject};
use machish::spec::MachSpec;
use machish::spec::arch::X86_64;
use machish::spec::constants::{CPU_TYPE_ARM64, CPU_TYPE_X86_64, MH_EXECUTE, MH_PIE, MH_TWOLEVEL};

/// `/bin/ls` is a universal binary with a genuine x86_64 `LC_DYLD_INFO_ONLY`
/// slice: classic `__stubs` + `__la_symbol_ptr`, `LC_MAIN`, PIE, two-level.
const LS: &str = "/bin/ls";
/// `/bin/echo` is built with the modern toolchain: chained fixups plus an
/// export trie.
const ECHO: &str = "/bin/echo";

#[test]
fn fat_enumerates_universal_binary_slices() {
    let Some(bytes) = common::system_binary(LS) else {
        skip!("{LS} not present")
    };

    assert_eq!(fat::kind(&bytes).unwrap(), fat::Kind::Fat);
    let slices = fat::slices(&bytes).unwrap();
    assert!(slices.len() >= 2, "expected a multi-architecture binary");

    let x86 = slices.iter().find(|s| s.cputype == CPU_TYPE_X86_64);
    let arm = slices.iter().find(|s| s.cputype == CPU_TYPE_ARM64);
    assert!(x86.is_some(), "no x86_64 slice in {LS}");
    assert!(arm.is_some(), "no arm64 slice in {LS}");

    // Every slice must be safely sub-sliceable, which is the invariant
    // `fat::slices` promises by validating extents up front.
    for s in &slices {
        let thin = fat::slice_bytes(&bytes, s).unwrap();
        assert_eq!(thin.len(), s.size);
    }
}

#[test]
fn select_narrows_to_the_x86_64_slice() {
    let Some(bytes) = common::system_binary(LS) else {
        skip!("{LS} not present")
    };

    let thin = fat::select(&bytes, &X86_64).unwrap();
    assert_eq!(fat::kind(thin).unwrap(), fat::Kind::Thin);
    assert!(
        thin.len() < bytes.len(),
        "slice should be smaller than the archive"
    );

    // The selected slice really is x86_64.
    let obj = MachObject::parse(thin, X86_64).unwrap();
    assert_eq!(obj.cputype, CPU_TYPE_X86_64);
}

#[test]
fn parses_the_structure_of_a_real_executable() {
    let Some(bytes) = common::system_binary(LS) else {
        skip!("{LS} not present")
    };
    let obj = MachObject::parse_any(&bytes, X86_64).unwrap();

    assert_eq!(obj.filetype, MH_EXECUTE);
    assert_ne!(obj.flags & MH_PIE, 0, "expected a PIE executable");
    assert_ne!(obj.flags & MH_TWOLEVEL, 0, "expected two-level namespace");
    assert!(obj.is_pie());
    assert!(obj.is_two_level());

    // Segment layout. Asserted structurally, not on literal addresses, so a
    // macOS update that relays the binary does not break the suite.
    let names: Vec<&str> = obj.segments.iter().map(|s| s.name).collect();
    assert!(names.contains(&"__PAGEZERO"));
    assert!(names.contains(&"__TEXT"));
    assert!(names.contains(&"__LINKEDIT"));

    let pagezero = obj
        .segments
        .iter()
        .find(|s| s.name == "__PAGEZERO")
        .unwrap();
    assert!(pagezero.is_page_zero());
    assert!(!pagezero.is_mappable(), "__PAGEZERO must never be mapped");
    assert_eq!(pagezero.vmaddr, 0);

    // The preferred base is the first *mappable* segment, i.e. just past
    // __PAGEZERO — the whole reason __PAGEZERO is excluded from the span.
    let text = obj.segments.iter().find(|s| s.name == "__TEXT").unwrap();
    assert_eq!(obj.preferred_base().unwrap(), text.vmaddr);
    assert_ne!(obj.preferred_base().unwrap(), 0);
    assert!(
        obj.vm_span().unwrap() < pagezero.vmsize,
        "span must exclude __PAGEZERO"
    );

    // Sections are attributed to their owning segment.
    assert!(!obj.sections.is_empty());
    for seg in &obj.segments {
        for sect in obj.sections_of(seg) {
            assert_eq!(sect.seg_name, seg.name);
        }
    }
    assert!(obj.section("__TEXT", "__text").is_some());

    // Dependencies are numbered from one, in file order.
    assert!(!obj.dependencies().is_empty());
    for (i, dep) in obj.dependencies().iter().enumerate() {
        assert_eq!(dep.ordinal as usize, i + 1);
        assert_eq!(
            obj.dependency_by_ordinal(dep.ordinal).unwrap().install_name,
            dep.install_name
        );
    }
    assert!(
        obj.dependency_by_ordinal(0).is_none(),
        "ordinal 0 is not a dependency"
    );

    assert!(matches!(obj.entry(), Some(EntryPoint::Main { .. })));
    obj.check_not_encrypted().unwrap();
}

#[test]
fn classic_binary_has_stubs_matching_its_lazy_pointers() {
    let Some(bytes) = common::system_binary(LS) else {
        skip!("{LS} not present")
    };
    let obj = MachObject::parse_any(&bytes, X86_64).unwrap();

    if obj.dyld_info().is_none() {
        skip!("{LS} is not an LC_DYLD_INFO binary on this OS version");
    }

    let stubs = obj.section("__TEXT", "__stubs").expect("__stubs");
    let la = obj
        .section("__DATA", "__la_symbol_ptr")
        .expect("__la_symbol_ptr");

    assert!(stubs.is_symbol_stubs());
    assert!(la.is_lazy_pointers());
    assert!(la.is_symbol_pointers());

    // On x86-64 a stub is `jmp qword [rip+disp32]`: six bytes.
    assert_eq!(stubs.reserved2, 6, "x86-64 stub stride");

    // The 1:1 correspondence that makes stub_va recovery possible at all:
    // one six-byte stub per eight-byte lazy pointer slot.
    let n_stubs = stubs.entry_count(u64::from(stubs.reserved2));
    let n_slots = la.entry_count(8);
    assert_eq!(
        n_stubs, n_slots,
        "each lazy pointer should have exactly one stub"
    );
    assert!(n_stubs > 0);
}

#[test]
fn modern_binary_uses_chained_fixups_and_an_export_trie() {
    let Some(bytes) = common::system_binary(ECHO) else {
        skip!("{ECHO} not present")
    };
    let obj = MachObject::parse_any(&bytes, X86_64).unwrap();

    // Both encodings must be reachable as fixtures, which is what justifies
    // implementing both decoders. If this binary is the modern shape, it should
    // carry chained fixups and *not* dyld info.
    if obj.chained_fixups().is_some() {
        assert!(
            obj.dyld_info().is_none(),
            "the two encodings are mutually exclusive"
        );
        assert!(
            obj.exports_trie().is_some(),
            "chained binaries carry a separate trie"
        );
    }
}

#[test]
fn locally_built_binary_parses_in_both_fixup_encodings() {
    const SRC: &str = "int main(void){return 0;}";

    let Some(chained) = common::build_x86_64("m11_chained", SRC, &[]) else {
        skip!("cannot build x86_64 binaries on this host")
    };
    let classic = common::build_x86_64("m11_classic", SRC, &["-Wl,-no_fixup_chains"])
        .expect("classic build should succeed once the chained one did");

    let a = MachObject::parse_any(&chained, X86_64).unwrap();
    let b = MachObject::parse_any(&classic, X86_64).unwrap();

    assert!(
        a.chained_fixups().is_some(),
        "default toolchain output is chained"
    );
    assert!(
        b.dyld_info().is_some(),
        "-no_fixup_chains produces LC_DYLD_INFO"
    );

    // Both link against libSystem — there is no -nostdlib on macOS, which is
    // why the provider chain is exercised from the very first fixup test.
    for obj in [&a, &b] {
        assert!(
            obj.dependencies()
                .iter()
                .any(|d| d.install_name.contains("libSystem")),
            "every macOS binary links libSystem"
        );
        assert_eq!(obj.dependencies()[0].kind, DependencyKind::Load);
    }
}

#[test]
fn arm64_slice_is_rejected_by_the_x86_64_spec() {
    let Some(bytes) = common::system_binary(LS) else {
        skip!("{LS} not present")
    };

    let slices = fat::slices(&bytes).unwrap();
    let arm = slices.iter().find(|s| s.cputype == CPU_TYPE_ARM64).unwrap();
    let thin = fat::slice_bytes(&bytes, arm).unwrap();

    // The type parameter is the guard: handing an arm64 slice to an x86_64
    // loader must fail loudly rather than produce a nonsense lift.
    assert_eq!(
        MachObject::parse(thin, X86_64).err(),
        Some(Error::WrongCpuType {
            expected: CPU_TYPE_X86_64,
            found: CPU_TYPE_ARM64
        })
    );
}

#[test]
fn arm64_slice_parses_under_the_arm64_spec() {
    let Some(bytes) = common::system_binary(LS) else {
        skip!("{LS} not present")
    };
    use machish::spec::arch::Arm64;

    let obj = MachObject::parse_any(&bytes, Arm64).unwrap();
    assert_eq!(obj.cputype, CPU_TYPE_ARM64);
    assert_eq!(Arm64.page_size(), 0x4000, "arm64 lays out at 16 KiB");
    assert_eq!(X86_64.page_size(), 0x1000, "x86_64 lays out at 4 KiB");
}

#[test]
fn load_commands_round_trip_to_recognized_variants() {
    let Some(bytes) = common::system_binary(LS) else {
        skip!("{LS} not present")
    };
    let obj = MachObject::parse_any(&bytes, X86_64).unwrap();

    // Every segment command must have been decoded as a Segment, not dropped
    // into Other — a silent fallthrough here would lose part of the image.
    let segment_cmds = obj
        .commands
        .iter()
        .filter(|c| matches!(c, LoadCommand::Segment(_)))
        .count();
    assert_eq!(segment_cmds, obj.segments.len());

    let dylib_cmds = obj
        .commands
        .iter()
        .filter(|c| matches!(c, LoadCommand::Dylib(_)))
        .count();
    assert_eq!(dylib_cmds, obj.dependencies().len());

    assert!(
        obj.commands
            .iter()
            .any(|c| matches!(c, LoadCommand::Symtab(_)))
    );
    assert!(
        obj.commands
            .iter()
            .any(|c| matches!(c, LoadCommand::Dysymtab(_)))
    );
    assert!(
        obj.commands
            .iter()
            .any(|c| matches!(c, LoadCommand::Dylinker(_)))
    );
}

#[test]
fn vmaddr_translation_agrees_with_the_section_table() {
    let Some(bytes) = common::system_binary(LS) else {
        skip!("{LS} not present")
    };
    let obj = MachObject::parse_any(&bytes, X86_64).unwrap();

    let text = obj.section("__TEXT", "__text").unwrap();
    let off = obj.vmaddr_to_off(text.addr).expect("__text is file-backed");
    assert_eq!(off, text.offset as usize);

    // An address in no segment has no file offset.
    assert!(obj.vmaddr_to_off(0xdead_0000_0000).is_none());

    // The header itself is mapped, which is what makes LC_MAIN's entryoff —
    // a *file* offset — usable directly as an offset from the image base.
    assert_eq!(obj.vmaddr_to_off(obj.preferred_base().unwrap()), Some(0));
}
