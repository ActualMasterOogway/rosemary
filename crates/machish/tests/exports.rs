#![cfg(all(feature = "std", feature = "arch-x86_64"))]
//! M1.3 acceptance: the export trie and its `LC_SYMTAB` fallback.
//!
//! The load-bearing test here is [`trie_and_nlist_agree`]. The two parsers are
//! independent readings of the same intent from two unrelated on-disk
//! structures, so agreement between them is evidence neither has a systematic
//! error — which no single-parser test against a fixture can give you, since
//! the fixture is only ever what the toolchain happened to emit.

mod common;

use machish::exports::{ExportKind, ExportTable, ExportValue};
use machish::image::MachObject;
use machish::spec::arch::X86_64;

/// A dylib with a deliberate spread of visibilities: two default-visibility
/// functions, one exported datum, one hidden function that must *not* appear,
/// and one static that must not either.
const LIB_SRC: &str = r#"
int exported_data = 42;
int visible_one(int x) { return x + 1; }
int visible_two(int x) { return visible_one(x) * 2; }
__attribute__((visibility("hidden"))) int hidden_fn(int x) { return x - 1; }
static int private_fn(int x) { return x; }
int uses_private(int x) { return private_fn(x) + hidden_fn(x); }
"#;

fn lib(name: &str, extra: &[&str]) -> Option<Vec<u8>> {
    let mut flags = vec!["-dynamiclib"];
    flags.extend_from_slice(extra);
    common::build_x86_64(name, LIB_SRC, &flags)
}

#[test]
fn dylib_trie_enumerates_default_visibility_symbols() {
    let Some(bytes) = lib("exports_trie_lib", &[]) else {
        skip!("cc -arch x86_64 unavailable")
    };
    let obj = MachObject::parse_any(&bytes, X86_64).unwrap();
    assert!(
        obj.exports_trie().is_some(),
        "default toolchain should emit an exports trie"
    );

    let table = ExportTable::parse(&obj).unwrap();

    for want in [
        "_visible_one",
        "_visible_two",
        "_uses_private",
        "_exported_data",
    ] {
        let e = table
            .get(want)
            .unwrap_or_else(|| panic!("{want} missing from the export trie"));
        assert_eq!(e.kind, ExportKind::Regular);
        assert_eq!(e.value, ExportValue::Direct);
    }
    for unwanted in ["_hidden_fn", "_private_fn"] {
        assert!(!table.contains(unwanted), "{unwanted} must not be exported");
    }
}

#[test]
fn export_addresses_land_in_the_right_sections() {
    let Some(bytes) = lib("exports_addr_lib", &[]) else {
        skip!("cc -arch x86_64 unavailable")
    };
    let obj = MachObject::parse_any(&bytes, X86_64).unwrap();
    let table = ExportTable::parse(&obj).unwrap();

    // Structural, not literal: a function's address must fall in an executable
    // segment and the datum's in a writable one. Asserting the actual numbers
    // would bind the test to one version of the linker's layout.
    let f = table.get("_visible_one").unwrap();
    let sect = obj
        .section_at(f.vmaddr)
        .expect("_visible_one outside every section");
    assert_eq!(sect.seg_name, "__TEXT");

    let d = table.get("_exported_data").unwrap();
    let sect = obj
        .section_at(d.vmaddr)
        .expect("_exported_data outside every section");
    assert_eq!(sect.seg_name, "__DATA");
}

#[test]
fn trie_and_nlist_agree_on_the_same_image() {
    let Some(bytes) = lib("exports_both_lib", &[]) else {
        skip!("cc -arch x86_64 unavailable")
    };
    let obj = MachObject::parse_any(&bytes, X86_64).unwrap();

    let trie = ExportTable::parse(&obj).unwrap();
    let nlist = ExportTable::parse_symtab(&obj).unwrap();
    assert!(!trie.is_empty() && !nlist.is_empty());

    // The symbol table also carries entries the trie has no reason to list
    // (`__mh_execute_header`-alikes and linker-synthesized names vary by
    // toolchain version), so the honest claim is containment plus address
    // agreement, not set equality.
    for e in trie.iter() {
        let Some(n) = nlist.get(&e.name) else {
            panic!("{} is in the trie but not the symbol table", e.name);
        };
        assert_eq!(n.vmaddr, e.vmaddr, "address disagreement for {}", e.name);
        assert_eq!(n.kind, e.kind, "kind disagreement for {}", e.name);
        assert_eq!(
            n.is_weak(),
            e.is_weak(),
            "weakness disagreement for {}",
            e.name
        );
    }
}

#[test]
fn weak_definitions_are_marked() {
    let src = r#"
        __attribute__((weak)) int weak_fn(int x) { return x; }
        int strong_fn(int x) { return weak_fn(x); }
    "#;
    let Some(bytes) = common::build_x86_64("exports_weak_lib", src, &["-dynamiclib"]) else {
        skip!("cc -arch x86_64 unavailable")
    };
    let obj = MachObject::parse_any(&bytes, X86_64).unwrap();
    let table = ExportTable::parse(&obj).unwrap();

    assert!(table.get("_weak_fn").unwrap().is_weak());
    assert!(!table.get("_strong_fn").unwrap().is_weak());
}

#[test]
fn old_style_image_uses_the_dyld_info_export_offset() {
    // `-no_fixup_chains` puts the trie back inside `LC_DYLD_INFO_ONLY` rather
    // than in its own load command, which is a different code path to the same
    // bytes and the only one available on pre-chained-fixups images.
    let Some(bytes) = lib("exports_dyldinfo_lib", &["-Wl,-no_fixup_chains"]) else {
        skip!("cc -arch x86_64 unavailable")
    };
    let obj = MachObject::parse_any(&bytes, X86_64).unwrap();
    assert!(
        obj.exports_trie().is_none(),
        "expected no LC_DYLD_EXPORTS_TRIE"
    );
    assert!(obj.dyld_info().is_some(), "expected LC_DYLD_INFO_ONLY");

    let table = ExportTable::parse(&obj).unwrap();
    assert!(table.contains("_visible_one"));
    assert!(!table.contains("_hidden_fn"));
}

#[test]
fn system_dylib_exports_are_enumerable() {
    // `libSystem.B.dylib` is not on disk as a file on modern macOS, but the
    // x86_64 slice of any universal *executable* still has a trie, and reading
    // one that we did not build proves the walker survives a real linker's
    // output rather than only our own fixtures'.
    let Some(bytes) = common::system_binary("/bin/echo") else {
        skip!("/bin/echo not present")
    };
    let obj = MachObject::parse_any(&bytes, X86_64).unwrap();

    let table = ExportTable::parse(&obj).unwrap();
    // An executable exports very little, but whatever it exports must be
    // well-formed: every direct export inside the image's own span.
    let base = obj.preferred_base().unwrap();
    let span = obj.vm_span().unwrap();
    for e in table.iter() {
        if e.value == ExportValue::Direct && e.kind != ExportKind::Absolute {
            assert!(
                (base..base + span).contains(&e.vmaddr),
                "{} at {:#x} is outside the image",
                e.name,
                e.vmaddr
            );
        }
    }
}
